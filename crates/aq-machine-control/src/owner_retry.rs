//! Durable identity and release evidence for one approved task retry.
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

pub struct Anchor {
    pub project_revision: i64,
    pub predecessor: Option<Uuid>,
    pub released: bool,
    pub overridden: bool,
}

pub async fn available(conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT to_regclass('owner_task_retry_intents') IS NOT NULL")
        .fetch_one(conn)
        .await
}

pub async fn admit(
    conn: &mut PgConnection,
    operation: Uuid,
    project: Uuid,
    task: &str,
    request: Value,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    if !available(conn).await? {
        return Ok(());
    }
    let task = Uuid::parse_str(task).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    let predecessor = super::holds::latest_attempt(conn, task)
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    sqlx::query(
        "INSERT INTO owner_task_retry_intents(operation_id,project_id,project_revision,task_id,predecessor,request,next_check_at)
         SELECT $1,project_id,revision,$3,$4,$5,$6+interval '10 seconds'
         FROM project_execution_bindings WHERE project_id=$2",
    )
    .bind(operation)
    .bind(project)
    .bind(task)
    .bind(predecessor)
    .bind(request)
    .bind(now)
    .execute(conn)
    .await
    .and_then(|result| {
        if result.rows_affected() == 1 {
            Ok(())
        } else {
            Err(sqlx::Error::RowNotFound)
        }
    })
}

pub async fn anchor(pool: &PgPool, operation: Uuid) -> Result<Option<Anchor>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    if !available(&mut conn).await? {
        return Ok(None);
    }
    sqlx::query(
        "SELECT project_revision,predecessor,released_at IS NOT NULL AS released,
           EXISTS(SELECT 1 FROM task_limit_overrides o WHERE o.operation_id=i.operation_id
             AND o.task_id=i.task_id AND o.attempt_id=i.predecessor) AS overridden
         FROM owner_task_retry_intents i WHERE i.operation_id=$1",
    )
    .bind(operation)
    .try_map(|row| {
        Ok(Anchor {
            project_revision: row.try_get("project_revision")?,
            predecessor: row.try_get("predecessor")?,
            released: row.try_get("released")?,
            overridden: row.try_get("overridden")?,
        })
    })
    .fetch_optional(&mut *conn)
    .await
}

pub async fn authorized(
    conn: &mut PgConnection,
    operation: Uuid,
    task: Uuid,
    predecessor: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(
           SELECT 1 FROM project_execution_bindings b
           JOIN owner_task_retry_intents i ON i.project_id=b.project_id AND i.project_revision=b.revision
           JOIN dashboard_operations d ON d.operation_id=i.operation_id
           JOIN users u ON u.id=d.principal_id AND u.role='owner'
           WHERE i.operation_id=$1 AND i.task_id=$2 AND i.predecessor=$3 FOR SHARE OF b,u
         )",
    )
    .bind(operation)
    .bind(task)
    .bind(predecessor)
    .fetch_one(conn)
    .await
}

pub async fn released(
    conn: &mut PgConnection,
    operation: Uuid,
    task: Uuid,
    predecessor: Uuid,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    if !authorized(conn, operation, task, predecessor).await? {
        return Err(sqlx::Error::RowNotFound);
    }
    let result = sqlx::query(
        "UPDATE owner_task_retry_intents SET released_at=COALESCE(released_at,$4)
         WHERE operation_id=$1 AND task_id=$2 AND predecessor=$3",
    )
    .bind(operation)
    .bind(task)
    .bind(predecessor)
    .bind(now)
    .execute(conn)
    .await?;
    if result.rows_affected() != 1 {
        return Err(sqlx::Error::RowNotFound);
    }
    Ok(())
}

impl Anchor {
    pub async fn matches(
        &self,
        pool: &PgPool,
        task: Uuid,
        revision: u64,
    ) -> Result<bool, super::Error> {
        Ok(self.project_revision as u64 == revision
            && self.predecessor
                == super::holds::latest_attempt(&mut *pool.acquire().await?, task).await?)
    }
}

pub struct Due<Request, Receipt> {
    pub principal_id: Uuid,
    pub request: Request,
    pub response: Receipt,
}

pub async fn claim_due<Request: DeserializeOwned, Receipt: DeserializeOwned>(
    pool: &PgPool,
    now: DateTime<Utc>,
) -> Result<Option<Due<Request, Receipt>>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    if !available(&mut conn).await? {
        return Ok(None);
    }
    let row = sqlx::query(
        "WITH due AS (
           SELECT i.operation_id FROM owner_task_retry_intents i JOIN dashboard_operations d USING(operation_id)
           WHERE i.next_check_at <= $1 AND d.disposition IN ('pending','unknown')
           ORDER BY i.next_check_at,i.operation_id LIMIT 1 FOR UPDATE OF i SKIP LOCKED
         ), claimed AS (
           UPDATE owner_task_retry_intents i
           SET checks=LEAST(checks+1,7),next_check_at=$1+make_interval(secs=>LEAST(300,10 << checks))
           FROM due WHERE i.operation_id=due.operation_id RETURNING i.operation_id,i.request
         )
         SELECT d.principal_id,c.request,d.response FROM claimed c JOIN dashboard_operations d USING(operation_id)
         JOIN users u ON u.id=d.principal_id AND u.role='owner' ",
    )
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok(Due {
            principal_id: row.try_get("principal_id")?,
            request: row.try_get::<sqlx::types::Json<_>, _>("request")?.0,
            response: row.try_get::<sqlx::types::Json<_>, _>("response")?.0,
        })
    })
    .transpose()
}

pub async fn due_terminations(
    pool: &PgPool,
    after: Option<Uuid>,
) -> Result<Vec<(Uuid, Uuid, Uuid, Uuid, Uuid)>, sqlx::Error> {
    sqlx::query_as(
            "SELECT DISTINCT ON(a.attempt_id) a.attempt_id,a.task_id,d.operation_id,d.principal_id,i.workspace_id \
             FROM execution_attempts a JOIN dashboard_operations d ON d.response->'subject_refs'->0->>'id'=a.task_id::text \
             JOIN dashboard_graph_intents i USING(operation_id) \
             WHERE a.kind='profiled_v1' AND a.holds_task AND d.kind IN ('task.cancel','task.delete') \
             AND d.disposition IN ('pending','unknown','confirmed') \
             AND ($1::uuid IS NULL OR a.attempt_id>$1) ORDER BY a.attempt_id,d.created_at DESC LIMIT 8",
    )
    .bind(after)
    .fetch_all(pool)
    .await
}
