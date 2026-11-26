//! Store a comment mutation and its replay record in the same transaction.
use super::{ADD, Applied, Author, DELETE, Error, Result, Thread, read, validate_body};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

pub(super) struct Mutation<'a> {
    pub kind: &'a str,
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub target: Uuid,
    pub expected: Option<i64>,
    pub body: Option<&'a str>,
}

/// The persisted identity and timestamp shared by the operation and comment rows.
struct OperationRecord {
    actor: Value,
    author_kind: &'static str,
    fingerprint: String,
    now: DateTime<Utc>,
}

impl OperationRecord {
    fn matches(&self, row: &sqlx::postgres::PgRow, kind: &str) -> Result<bool> {
        Ok(row.try_get::<String, _>("kind")? == kind
            && row.try_get::<Value, _>("actor")? == self.actor
            && row.try_get::<String, _>("fingerprint")? == self.fingerprint)
    }
}

impl<'a> Mutation<'a> {
    pub fn add(operation_id: Uuid, thread: Thread, body: &'a str) -> Self {
        Self {
            kind: ADD,
            operation_id,
            project_id: thread.project_id,
            target: thread.task_id,
            expected: None,
            body: Some(body),
        }
    }

    fn validate(&self) -> Result<()> {
        if [self.operation_id, self.project_id, self.target]
            .iter()
            .any(Uuid::is_nil)
        {
            return Err(Error::Invalid);
        }
        if let Some(body) = self.body {
            validate_body(body)?;
        }
        if (self.kind == ADD) != self.expected.is_none()
            || (self.kind == DELETE) != self.body.is_none()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn comment_id(&self) -> Uuid {
        if self.kind == ADD {
            self.operation_id
        } else {
            self.target
        }
    }

    pub async fn apply(
        &self,
        pool: &PgPool,
        author: &Author,
        now: DateTime<Utc>,
    ) -> Result<Applied> {
        let mut tx = pool.begin().await?;
        let applied = self.apply_in_tx(&mut tx, author, now).await?;
        tx.commit().await?;
        Ok(applied)
    }

    pub async fn apply_in_tx(
        &self,
        tx: &mut PgConnection,
        author: &Author,
        now: DateTime<Utc>,
    ) -> Result<Applied> {
        self.validate()?;
        let record = self.record(author, now)?;
        if !self.reserve(tx, &record).await? {
            return self.replay(tx, &record).await;
        }
        let revision = if self.kind == ADD {
            self.insert(tx, &record).await?
        } else {
            self.change(tx, &record.actor, record.now).await?
        };
        sqlx::query("UPDATE task_comment_operations SET revision=$2 WHERE operation_id=$1")
            .bind(self.operation_id)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        self.applied(tx, revision).await
    }

    fn record(&self, author: &Author, now: DateTime<Utc>) -> Result<OperationRecord> {
        // Postgres keeps microseconds; store what is read back.
        let now = DateTime::from_timestamp_micros(now.timestamp_micros()).ok_or(Error::Invalid)?;
        let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&json!([
            self.kind,
            self.project_id,
            self.target,
            self.expected,
            self.body
        ]))?));
        Ok(OperationRecord {
            actor: serde_json::to_value(author)?,
            author_kind: author.kind(),
            fingerprint,
            now,
        })
    }

    async fn applied(&self, tx: &mut PgConnection, revision: i64) -> Result<Applied> {
        let comment = read(tx, self.comment_id())
            .await?
            .ok_or(Error::Unavailable)?;
        Ok(Applied { comment, revision })
    }

    // The operation row serializes concurrent replays of one operation ID.
    async fn reserve(&self, tx: &mut PgConnection, record: &OperationRecord) -> Result<bool> {
        let result = sqlx::query("INSERT INTO task_comment_operations(operation_id,kind,comment_id,actor,fingerprint,revision,created_at) VALUES($1,$2,$3,$4,$5,0,$6) ON CONFLICT DO NOTHING")
            .bind(self.operation_id)
            .bind(self.kind)
            .bind(self.comment_id())
            .bind(&record.actor)
            .bind(&record.fingerprint)
            .bind(record.now)
            .execute(tx)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn replay(&self, tx: &mut PgConnection, record: &OperationRecord) -> Result<Applied> {
        let row = sqlx::query("SELECT kind,actor,fingerprint,revision FROM task_comment_operations WHERE operation_id=$1")
            .bind(self.operation_id)
            .fetch_one(&mut *tx)
            .await?;
        if !record.matches(&row, self.kind)? {
            return Err(Error::IdConflict);
        }
        self.applied(tx, row.try_get("revision")?).await
    }

    async fn insert(&self, tx: &mut PgConnection, record: &OperationRecord) -> Result<i64> {
        sqlx::query("INSERT INTO task_comments(comment_id,project_id,task_id,author_kind,author_ref,body,created_at,revision) VALUES($1,$2,$3,$4,$5,$6,$7,1)")
            .bind(self.comment_id())
            .bind(self.project_id)
            .bind(self.target)
            .bind(record.author_kind)
            .bind(&record.actor)
            .bind(self.body)
            .bind(record.now)
            .execute(&mut *tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(d) if d.is_unique_violation() => Error::IdConflict,
                _ => Error::Unavailable,
            })?;
        Ok(1)
    }

    async fn change(
        &self,
        tx: &mut PgConnection,
        actor: &Value,
        now: DateTime<Utc>,
    ) -> Result<i64> {
        let row = sqlx::query("SELECT project_id,author_ref,deleted_at,revision FROM task_comments WHERE comment_id=$1 FOR UPDATE")
            .bind(self.comment_id())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
        self.check_revision(&row, actor)?;
        let sql = if self.kind == DELETE {
            "UPDATE task_comments SET body=NULL,deleted_at=$2,revision=revision+1 WHERE comment_id=$1 RETURNING revision"
        } else {
            "UPDATE task_comments SET body=$3,edited_at=$2,revision=revision+1 WHERE comment_id=$1 RETURNING revision"
        };
        Ok(sqlx::query_scalar(sql)
            .bind(self.comment_id())
            .bind(now)
            .bind(self.body)
            .fetch_one(&mut *tx)
            .await?)
    }

    fn check_revision(&self, row: &sqlx::postgres::PgRow, actor: &Value) -> Result<()> {
        if row.try_get::<Uuid, _>("project_id")? != self.project_id {
            return Err(Error::NotFound);
        }
        if row.try_get::<Value, _>("author_ref")? != *actor {
            return Err(Error::NotAuthor);
        }
        if row
            .try_get::<Option<DateTime<Utc>>, _>("deleted_at")?
            .is_some()
        {
            return Err(Error::Deleted);
        }
        let current: i64 = row.try_get("revision")?;
        if Some(current) != self.expected {
            return Err(Error::Conflict(current));
        }
        Ok(())
    }
}
