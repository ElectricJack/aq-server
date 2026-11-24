//! Stored source-export identities and capacity checks, shared by reservation and finalization.
use crate::{Kind, staging_used, usage};
use sqlx::{PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

pub async fn set_timeouts(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *conn)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn lock_staging(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(719247031)")
        .execute(conn)
        .await?;
    Ok(())
}

pub struct Staging {
    pub reserved: u64,
    pub limit: u64,
    pub project: Uuid,
    pub request_bytes: usize,
}
impl Staging {
    pub async fn exhausted(
        &self,
        conn: &mut PgConnection,
        now: u64,
    ) -> Result<Option<&'static str>, sqlx::Error> {
        let used = staging_used(conn, now as i64).await?;
        if used < 0
            || (used as u64)
                .checked_add(self.reserved)
                .is_none_or(|n| n > self.limit)
        {
            return Ok(Some("source staging quota exhausted"));
        }
        let capacity = usage(conn, Kind::SourceExports, Some(self.project), now).await?;
        let incoming = self.request_bytes as i64 + 4_194_304;
        let fits = |bytes: i64, limit| bytes.checked_add(incoming).is_some_and(|n| n <= limit);
        let global = capacity.total_rows < 4096 && fits(capacity.total_bytes, 268_435_456);
        let project = capacity.scope_rows < 256 && fits(capacity.scope_bytes, 67_108_864);
        Ok((!global || !project).then_some("source export metadata quota exhausted"))
    }
}

pub struct StoredExport {
    request_bytes: Vec<u8>,
    request_digest: String,
    pub reserved_bytes: u64,
    pub state: String,
    pub verified_bytes: Option<Vec<u8>>,
    pub verified_digest: Option<String>,
}
impl StoredExport {
    pub fn matches(&self, bytes: &[u8], digest: &str) -> bool {
        self.request_bytes == bytes && self.request_digest == digest
    }
    fn from_row(row: PgRow) -> Self {
        let verified_bytes = row.get("verified_bytes");
        let verified_digest = row.get("verified_digest");
        Self {
            verified_bytes,
            verified_digest,
            ..Self::identity(row)
        }
    }
    fn identity(row: PgRow) -> Self {
        Self {
            request_bytes: row.get("request_bytes"),
            request_digest: row.get("request_digest"),
            reserved_bytes: row.get::<i64, _>("reserved_bytes") as u64,
            state: row.get("state"),
            verified_bytes: None,
            verified_digest: None,
        }
    }
}
const IDENTITY: &str = "SELECT request_bytes,request_digest,reserved_bytes,state,verified_bytes,verified_digest FROM profiled_source_exports WHERE operation_id=$1";

pub enum Access {
    Historical,
    ForUpdate,
}
pub async fn load(
    conn: &mut PgConnection,
    operation: Uuid,
    access: Access,
) -> Result<Option<StoredExport>, sqlx::Error> {
    let locked;
    let sql = match access {
        Access::Historical => IDENTITY,
        Access::ForUpdate => {
            locked = format!("{IDENTITY} FOR UPDATE");
            &locked
        }
    };
    Ok(sqlx::query(sql)
        .bind(operation)
        .fetch_optional(conn)
        .await?
        .map(StoredExport::from_row))
}
pub async fn pending_locked(
    conn: &mut PgConnection,
    operation: Uuid,
) -> Result<Option<StoredExport>, sqlx::Error> {
    // Quota reconciliation does not read the potentially large retained payload.
    let query = "SELECT request_bytes,request_digest,reserved_bytes,state FROM profiled_source_exports WHERE operation_id=$1 FOR UPDATE";
    Ok(sqlx::query(query)
        .bind(operation)
        .fetch_optional(conn)
        .await?
        .map(StoredExport::identity))
}

pub struct Verification<'a> {
    pub bytes: &'a [u8],
    pub digest: &'a str,
    pub charged_bytes: u64,
}
impl Verification<'_> {
    pub fn matches(&self, row: &StoredExport) -> bool {
        row.verified_bytes.as_deref() == Some(self.bytes)
            && row.verified_digest.as_deref() == Some(self.digest)
    }
    pub async fn save(&self, conn: &mut PgConnection, operation: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE profiled_source_exports SET state='verified',verified_bytes=$2,verified_digest=$3,charged_bytes=$4 WHERE operation_id=$1")
            .bind(operation).bind(self.bytes).bind(self.digest).bind(self.charged_bytes as i64).execute(conn).await?;
        Ok(())
    }
}
