//! The live inventory for lifecycle artifact transfers and retained receipts.
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub async fn lock(conn: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(719247032_i64)
        .execute(conn)
        .await?;
    Ok(())
}
pub async fn capacity(
    conn: &mut PgConnection,
    incoming: u64,
    limit: u64,
) -> Result<Option<&'static str>, sqlx::Error> {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM retained_artifacts WHERE NOT released")
            .fetch_one(&mut *conn)
            .await?;
    if count >= 1024 {
        return Ok(Some("artifact transfer count limit exceeded"));
    }
    let used: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(declared_bytes),0)::bigint FROM retained_artifacts WHERE NOT released",
    )
    .fetch_one(conn)
    .await?;
    Ok(((used.max(0) as u64).saturating_add(incoming) > limit)
        .then_some("artifact retention quota exhausted"))
}

pub struct Retained {
    scope: (Uuid, Uuid, Uuid),
    pub bytes: u64,
    state: String,
    released: bool,
    until: u64,
}
impl Retained {
    pub fn active_at(&self, now: u64) -> bool {
        self.state == "retained" && !self.released && self.until > now
    }
    pub fn matches_lineage(&self, project: Uuid, task: Uuid, lineage: Uuid) -> bool {
        self.scope == (project, task, lineage)
    }
}
pub async fn retained(
    conn: &mut PgConnection,
    receipt: Uuid,
) -> Result<Option<Retained>, sqlx::Error> {
    let row = sqlx::query("SELECT project_id,task_id,lineage_id,declared_bytes,state,released,retained_until_ms FROM retained_artifacts WHERE receipt_id=$1").bind(receipt).fetch_optional(conn).await?;
    Ok(row.map(|r| Retained {
        scope: (r.get("project_id"), r.get("task_id"), r.get("lineage_id")),
        bytes: r.get::<i64, _>("declared_bytes").max(0) as u64,
        state: r.get("state"),
        released: r.get("released"),
        until: r
            .get::<Option<i64>, _>("retained_until_ms")
            .unwrap_or(0)
            .max(0) as u64,
    }))
}

pub async fn expired(conn: &mut PgConnection, now: u64) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    sqlx::query_as("SELECT transfer_id,receipt_id FROM retained_artifacts WHERE NOT released AND state='retained' AND retained_until_ms IS NOT NULL AND retained_until_ms <= $1 FOR UPDATE SKIP LOCKED")
        .bind(now.min(i64::MAX as u64) as i64).fetch_all(conn).await
}
pub async fn stale(conn: &mut PgConnection, ttl_secs: u64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT transfer_id FROM retained_artifacts WHERE state='uploading' AND updated_at < now()-($1::bigint * interval '1 second') FOR UPDATE SKIP LOCKED")
        .bind(ttl_secs.min(i64::MAX as u64) as i64).fetch_all(conn).await
}
pub async fn release(conn: &mut PgConnection, transfer: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE retained_artifacts SET released=TRUE,updated_at=now() WHERE transfer_id=$1",
    )
    .bind(transfer)
    .execute(conn)
    .await?;
    Ok(())
}
pub async fn reject(conn: &mut PgConnection, transfer: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE retained_artifacts SET state='rejected',released=TRUE,updated_at=now() WHERE transfer_id=$1")
        .bind(transfer).execute(conn).await?;
    Ok(())
}
