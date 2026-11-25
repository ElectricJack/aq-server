use crate::{
    AppState,
    error::{AppError, db},
    profiled_execution::on_heap,
};
use aq_admission::{ArtifactRef, Digest};
use serde::Serialize;
use sqlx::{Postgres, Transaction};
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("phase execution authority is not currently available".into())
}
fn execution_clock(
    time: u64,
    expiry: u64,
    policy_version: u32,
) -> Result<aq_admission::transport::DeliveryClock, AppError> {
    use aq_admission::transport::DeliveryClock;
    if policy_version == 3 {
        DeliveryClock::new_interactive_session(time, expiry)
    } else {
        DeliveryClock::new(time, expiry)
    }
    .map_err(|_| refused())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| refused())
}
fn canonical(v: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut v = serde_json::to_value(v).map_err(|_| AppError::Internal)?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| AppError::Internal)
}
fn digest(v: &impl Serialize) -> Result<Digest, AppError> {
    Ok(Digest::of(&canonical(v)?))
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool, AppError> {
    Ok(canonical(a)? == canonical(b)?)
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='3s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(db)?;
    }
    Ok(tx)
}
/// Same shared retained byte budget as original Accept/Grant. A still-running
/// current-profile owner refuses missing storage rather than undercharging.
pub(super) async fn retained_bytes_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: uuid::Uuid,
) -> Result<(i64, i64), AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    let version = version.parse::<u32>().map_err(|_| refused())?;
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if version < 32 && !present {
        return Ok((0, 0));
    }
    if version < 32 || !present {
        return Err(refused());
    }
    sqlx::query_as("SELECT COALESCE(sum(octet_length(request_bytes)::BIGINT+octet_length(receipt_bytes)+octet_length(provenance_bytes)),0)::BIGINT,COALESCE(sum(octet_length(request_bytes)::BIGINT+octet_length(receipt_bytes)+octet_length(provenance_bytes)) FILTER(WHERE node_id=$1),0)::BIGINT FROM profiled_phase_execution_grants").bind(node).fetch_one(&mut **tx).await.map_err(db)
}
async fn require_schema(tx: &mut Transaction<'_, Postgres>) -> Result<(), AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if version.parse::<u32>().map_err(|_| refused())? < 32 {
        return Err(refused());
    }
    let _: (i64, i64) = retained_bytes_locked(tx, uuid::Uuid::nil()).await?;
    Ok(())
}

/// Owning issuer entry point. Transport independently gates the qualified profile.
/// A new execution receipt is issued only from actual current owning facts.
pub async fn prepare(
    state: &AppState,
    auth: &crate::worker_api::AuthenticatedNode,
    request: &phase::PrepareRequest,
    nonce: uuid::Uuid,
) -> Result<phase::ExecutionDelivery, AppError> {
    use aq_admission::transport::TransportDocument;
    TransportDocument::Prepare(request.original.clone())
        .validate()
        .map_err(|_| refused())?;
    request.negotiation.validate().map_err(|_| refused())?;
    if nonce.is_nil() || auth.id() != request.original.identity.node_id {
        return Err(refused());
    }
    let aq_admission::SessionIdentity::Remote {
        node_id, boot_id, ..
    } = request.original.binding.session
    else {
        return Err(refused());
    };
    if (node_id, boot_id) != (auth.id(), request.original.identity.boot_id) {
        return Err(refused());
    }
    let _permit = super::phase_execution::request_permit()?;
    let preparation = PhasePreparation {
        state,
        auth,
        request,
        nonce,
    };
    // The initial-owner poll frame must fully return before replay observation
    // work begins. The same request permit covers both bounded branches.
    match Box::pin(prepare_inner(&preparation)).await? {
        PreparationOutcome::Issued(reply) => Ok(*reply),
        PreparationOutcome::Replay(candidate) => {
            Box::pin(finish_replay(&preparation, *candidate)).await
        }
    }
}

struct PhasePreparation<'a> {
    state: &'a AppState,
    auth: &'a crate::worker_api::AuthenticatedNode,
    request: &'a phase::PrepareRequest,
    nonce: uuid::Uuid,
}

enum PreparationOutcome {
    Issued(Box<phase::ExecutionDelivery>),
    Replay(Box<ReplayCandidate>),
}

include!("owner_issue.rs");
include!("owner_replay.rs");
include!("owner_fences.rs");
include!("owner_replay_fences.rs");
