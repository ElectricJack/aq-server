//! Default-inactive separately owned no-launch preparation. No execution issuer.
// Shared owner files intentionally instantiate distinct nominal protocol types.
// A module alias would cross the old/new contract boundary instead.
#![allow(clippy::duplicate_mod)]
mod accept;
mod api;
mod authority;
pub use api::routes;
pub use authority::PreparationAuthority;
pub(crate) use authority::load_preparing_locked;
pub(super) mod policy;
pub use accept::accept;
mod history;
mod proposal;
use crate::{AppState, error::AppError};
use aq_admission::preparation_v2 as phase;
const POLICY_PREFIX: &str = "aq.accepted-preparation-policy/";
pub(crate) use history::{
    AcceptedHistory, load_accepted_history_locked, require_live_accepted_locked,
};
pub use policy::{PolicyDefinition, policy_for};
pub use proposal::{ProposalRequest, propose};
use sqlx::{Postgres, Transaction};
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("accepted preparation v2 is not currently available".into())
}
pub(super) fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
pub(super) fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| refused())
}
pub(super) fn value(v: &impl serde::Serialize) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(v).map_err(|_| AppError::Internal)
}
pub(super) async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
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
pub(super) async fn quota(
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
    let kind = aq_history_caps::Kind::V2Proposals;
    let p = aq_history_caps::usage(tx, kind, Some(node), 0)
        .await
        .map_err(db)?;
    let counts = (p.total_rows, p.scope_rows);
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
pub(super) async fn require_schema(tx: &mut Transaction<'_, Postgres>) -> Result<(), AppError> {
    if schema_available(tx).await? {
        Ok(())
    } else {
        Err(refused())
    }
}

pub(super) fn request_permit() -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
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

fn validate_registry_at_proposal(
    _: &crate::intelligence::resolver::Decision,
    _: &aq_admission::ArtifactRef,
) -> Result<(), AppError> {
    Ok(())
}
struct FrozenRegistryAtAccept;
async fn registry_at_accept(
    _: &AppState,
    _: &mut Transaction<'_, Postgres>,
    _: &phase::Proposal,
) -> Result<FrozenRegistryAtAccept, AppError> {
    Ok(FrozenRegistryAtAccept)
}
fn accepted_lease(
    q: &phase::AcceptRequest,
    p: &phase::Proposal,
    material: &aq_admission::MaterialLease,
    time: u64,
    expiry: u64,
    lease_id: uuid::Uuid,
    _: FrozenRegistryAtAccept,
) -> Result<phase::AcceptedPreparationLease, AppError> {
    Ok(phase::AcceptedPreparationLease {
        lease_id: uuid::Uuid::new_v5(&lease_id, b"accepted-preparation-v2"),
        revision: 1,
        identity: q.original.identity.clone(),
        binding: q.original.acceptance.binding.clone(),
        guardian_id: q.original.guardian_id,
        launch_id: q.original.launch_id,
        hold: q.hold.clone(),
        proposal_digest: q.proposal_digest.clone(),
        policy_digest: p.policy.digest().map_err(|_| refused())?,
        choice_digest: p.choice.digest().map_err(|_| refused())?,
        issued_at_ms: time,
        expires_at_ms: expiry,
        materials: vec![material.clone()],
    })
}

async fn validate_live_registry(
    _: &AppState,
    _: &mut Transaction<'_, Postgres>,
    _: &phase::Proposal,
    _: &phase::AcceptedReceipt,
) -> Result<(), AppError> {
    Ok(())
}

/// Existing installed backing remains checkable after retrieval expires. The
/// caller must independently own a current, still-live original execution grant.
pub(super) async fn require_installed_material_locked(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    tx: &mut Transaction<'_, Postgres>,
    history: &AcceptedHistory,
) -> Result<(), AppError> {
    authority::current_policy_locked(state, tx, auth, &history.proposal).await?;
    authority::material_locked(tx, &history.request, &history.receipt).await
}

/// One actual blocking read budget across nominal profiles. The spawned read
/// retains this permit through completion even when its HTTP waiter disappears.
pub(super) fn material_read_permit() -> Result<tokio::sync::OwnedSemaphorePermit, ()> {
    static READS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    READS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ())
}
