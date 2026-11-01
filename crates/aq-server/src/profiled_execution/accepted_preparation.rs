//! Default-inactive separately owned no-launch preparation. No execution issuer.
mod accept;
mod api;
mod authority;
pub use api::routes;
pub use authority::PreparationAuthority;
pub(crate) use authority::load_preparing_locked;
pub(super) mod policy;
pub use accept::accept;
mod proposal;
use crate::{AppState, error::AppError};
pub use policy::{PolicyDefinition, policy_for};
pub use proposal::{ProposalRequest, propose};
use sqlx::{Postgres, Transaction};
fn refused() -> AppError {
    AppError::Conflict("accepted preparation v2 is not currently available".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(refused)
}
fn value(v: &impl serde::Serialize) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(v).map_err(|_| AppError::Internal)
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for s in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='3s'",
    ] {
        sqlx::query(s).execute(&mut *tx).await.map_err(db)?;
    }
    Ok(tx)
}
async fn quota(
    tx: &mut Transaction<'_, Postgres>,
    node: uuid::Uuid,
    incoming: u64,
    new_proposal: bool,
) -> Result<(), AppError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-preparation-quota-v1',0))",
    )
    .execute(&mut **tx)
    .await
    .map_err(db)?;
    let q = super::preparation::retained_quota_locked(tx, node).await?;
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT count(*),count(*) FILTER(WHERE node_id=$1) FROM profiled_v2_proposals",
    )
    .bind(node)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if (new_proposal && (counts.0 >= 4096 || counts.1 >= 256))
        || q.0 >= 4096
        || q.1 >= 256
        || u64::try_from(q.2)
            .ok()
            .and_then(|v| v.checked_add(incoming))
            .is_none_or(|v| v > 67_108_864)
        || u64::try_from(q.3)
            .ok()
            .and_then(|v| v.checked_add(incoming))
            .is_none_or(|v| v > 4_194_304)
    {
        return Err(refused());
    }
    Ok(())
}

/// Candidate030 can be absent on a valid older schema; activation routes still
/// require actual tables. Once030 is registered the normal migrator owns presence.
pub(super) async fn retained_bytes_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: uuid::Uuid,
) -> Result<(i64, i64), AppError> {
    if !schema_available(tx).await? {
        return Ok((0, 0));
    }
    sqlx::query_as("SELECT COALESCE(sum(bytes),0)::BIGINT,COALESCE(sum(bytes) FILTER(WHERE node_id=$1),0)::BIGINT FROM (SELECT node_id,octet_length(proposal::text)+octet_length(policy::text) AS bytes FROM profiled_v2_proposals UNION ALL SELECT node_id,octet_length(request::text)+octet_length(receipt::text) AS bytes FROM profiled_v2_accepts) retained").bind(node).fetch_one(&mut **tx).await.map_err(db)
}

async fn schema_available(tx: &mut Transaction<'_, Postgres>) -> Result<bool, AppError> {
    let (proposal,accept,version):(bool,bool,String)=sqlx::query_as("SELECT to_regclass('profiled_v2_proposals') IS NOT NULL,to_regclass('profiled_v2_accepts') IS NOT NULL,(SELECT value FROM meta WHERE key='state_format_version')").fetch_one(&mut **tx).await.map_err(db)?;
    let version: u32 = version.parse().map_err(|_| refused())?;
    match (proposal, accept) {
        (true, true) => Ok(true),
        (false, false) if (28..=29).contains(&version) => Ok(false),
        _ => Err(refused()),
    }
}
async fn require_schema(tx: &mut Transaction<'_, Postgres>) -> Result<(), AppError> {
    if schema_available(tx).await? {
        Ok(())
    } else {
        Err(refused())
    }
}

fn request_permit() -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
    static REQUESTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    REQUESTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())
}
pub(super) async fn reject_legacy_operation_collision(
    tx: &mut Transaction<'_, Postgres>,
    node: uuid::Uuid,
    operation: uuid::Uuid,
) -> Result<(), AppError> {
    if !schema_available(tx).await? {
        return Ok(());
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE node_id=$1 AND operation_id=$2)",
    )
    .bind(node)
    .bind(operation)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if exists { Err(refused()) } else { Ok(()) }
}
