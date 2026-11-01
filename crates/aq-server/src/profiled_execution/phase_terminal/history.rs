//! Authenticated immutable phase identity lookup, including lost first delivery.
//! A missing row is a projection snapshot, never proof that issuance cannot occur.
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Kind, Policy},
    profiled_execution::{accepted_preparation, closure, phase_execution},
    worker_api::{self, AuthenticatedNode},
};
use aq_admission::{SessionIdentity, granted_closure::JournalCheckpoint};
use aq_phase_terminal_protocol::{
    Context, HistoricalGrantIdentity, HistoryReceipt, HistoryRequest,
};
use sqlx::Row;
use std::time::Duration;
use tokio::sync::Semaphore;
use uuid::Uuid;

static REQUESTS: Semaphore = Semaphore::const_new(2);
fn conflict() -> AppError {
    AppError::Conflict("phase history identity mismatch".into())
}
fn database(_: sqlx::Error) -> AppError {
    AppError::Internal
}

/// Explicit owner API only; no production route is mounted by this module.
/// Current node authentication authorizes inspection, not fresh execution.
pub async fn read(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &HistoryRequest,
) -> Result<HistoryReceipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid phase history request".into()))?;
    let SessionIdentity::Remote { node_id, .. } = request.binding.session else {
        return Err(conflict());
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("phase history node mismatch".into()));
    }
    let _permit = REQUESTS
        .try_acquire()
        .map_err(|_| AppError::Maintenance("phase history capacity unavailable".into()))?;
    // General pool only. History cannot occupy reserved terminal/renewal pools,
    // and slow acquisition or a chain of reads has a finite total budget.
    tokio::time::timeout(Duration::from_secs(3), read_inner(state, node, request))
        .await
        .map_err(|_| AppError::Maintenance("phase history read timed out".into()))?
}

async fn read_inner(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &HistoryRequest,
) -> Result<HistoryReceipt, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for sql in [
        "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(database)?;
    }
    worker_api::lock_current_node(&mut tx, node).await?;
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != request.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
    {
        return Err(conflict());
    }
    let accepted =
        accepted_preparation::load_accepted_history_locked(&mut tx, &request.binding).await?;
    if accepted.request.original.identity.node_id != node.id()
        || accepted.receipt.digest().map_err(|_| conflict())? != request.accepted_receipt_digest
    {
        return Err(conflict());
    }
    let row = sqlx::query("SELECT node_id,grant_id,operation_id,accepted_receipt_digest FROM profiled_phase_execution_grants WHERE attempt_id=$1 FOR SHARE")
        .bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(database)?;
    let mut not_before_ms = accepted.receipt.lease.issued_at_ms;
    let identity = if let Some(row) = row {
        if row.get::<Uuid, _>("node_id") != node.id()
            || row.get::<Uuid, _>("operation_id") != request.prepare_operation_id
            || row.get::<String, _>("accepted_receipt_digest")
                != request.accepted_receipt_digest.as_str()
        {
            return Err(conflict());
        }
        let history = phase_execution::history::load_optional_locked(
            &mut tx,
            node.id(),
            &request.binding,
            row.get("grant_id"),
        )
        .await?
        .ok_or_else(conflict)?;
        not_before_ms = not_before_ms.max(history.receipt.issued_at_ms);
        let registration =
            closure::history::load_locked(&mut tx, history.provenance.registration_id).await?;
        let prepared = JournalCheckpoint {
            revision: history.request.original.journal_revision,
            digest: history.request.original.journal_digest.clone(),
        };
        let context = Context {
            execution: &history.receipt,
            prepare: &history.request,
            accepted: &history.accepted.receipt,
            acceptance: &history.accepted.request,
            proposal: &history.accepted.proposal,
            runtime: &registration.metadata.request.runtime,
            runtime_qualification: &registration.metadata.qualification,
            prepared: &prepared,
        };
        Some(HistoricalGrantIdentity::for_context(&context).map_err(|_| conflict())?)
    } else {
        None
    };
    let receipt = HistoryReceipt {
        schema_version: 1,
        negotiation: request.negotiation.clone(),
        request_digest: request.digest().map_err(|_| conflict())?,
        observed_at_ms: u64::try_from((state.clock)().timestamp_millis())
            .map_err(|_| conflict())?,
        identity,
    };
    receipt.validate_for(request).map_err(|_| conflict())?;
    if receipt.observed_at_ms < not_before_ms {
        return Err(conflict());
    }
    tx.commit().await.map_err(database)?;
    Ok(receipt)
}
