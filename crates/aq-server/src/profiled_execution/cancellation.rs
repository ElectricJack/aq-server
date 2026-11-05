//! Native cancellation owns authority, never physical stop or source release.
use crate::{
    AppState,
    error::AppError,
    execution::{authority::hot::HotState, profiled::Status},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    LaunchBinding,
    transport::{Cancellation, CancellationReason, WorkerIdentity},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Requested,
    NeverGranted,
    AlreadyAccepted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub cancellation_id: Option<Uuid>,
    pub authority_revision: u64,
    pub disposition: Disposition,
    pub recorded_at_ms: u64,
}
fn db(error: sqlx::Error) -> AppError {
    tracing::error!(%error, "native cancellation storage failure");
    AppError::Internal
}
fn conflict() -> AppError {
    AppError::Conflict("cancellation operation payload differs".into())
}

/// Common domain entrypoint for authenticated owner adapters. Its node →
/// attempt/hot locks also serialize grant issuance and successful evaluation.
pub async fn cancel(
    state: &AppState,
    attempt: Uuid,
    operation: Uuid,
    reason: &str,
) -> Result<Receipt, AppError> {
    if attempt.is_nil() || operation.is_nil() || reason.trim().is_empty() || reason.len() > 4096 {
        return Err(AppError::BadRequest(
            "invalid cancellation identity or reason".into(),
        ));
    }
    let node: Uuid = sqlx::query_scalar(
        "SELECT node_id FROM execution_attempts WHERE attempt_id=$1 AND kind='profiled_v1'",
    )
    .bind(attempt)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| AppError::NotFound("native attempt not found".into()))?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for setting in [
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='5s'",
    ] {
        sqlx::query(setting).execute(&mut *tx).await.map_err(db)?;
    }
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    // Cross-node reuse of an operation id must return a conflict, not a race
    // on the uniqueness constraint. No other owner acquires this lock.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("aq.native-cancel:{operation}"))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let request = json!({"attempt_id":attempt,"reason":reason});
    if let Some(old) =
        sqlx::query("SELECT request,response FROM profiled_cancel_operations WHERE operation_id=$1")
            .bind(operation)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
    {
        if old.get::<Value, _>("request") != request {
            return Err(conflict());
        }
        let receipt =
            serde_json::from_value(old.get("response")).map_err(|_| AppError::Internal)?;
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    let row = sqlx::query("SELECT l.launch_binding,h.state,h.authority_revision FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) JOIN profiled_launches l USING(attempt_id) WHERE a.attempt_id=$1 AND a.node_id=$2 FOR UPDATE OF a,h")
        .bind(attempt).bind(node).fetch_one(&mut *tx).await.map_err(db)?;
    let binding: LaunchBinding =
        serde_json::from_value(row.get("launch_binding")).map_err(|_| AppError::Internal)?;
    let mut hot: HotState =
        serde_json::from_value(row.get("state")).map_err(|_| AppError::Internal)?;
    hot.validate_against(&binding)
        .map_err(|_| AppError::Internal)?;
    let now = u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(AppError::Internal)?;
    let accepted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=$1 AND disposition='accepted')")
        .bind(attempt).fetch_one(&mut *tx).await.map_err(db)?;
    let disposition = if accepted {
        // Accepted evaluations and their outstanding Graph completion remain
        // immutable even if a later owner asks to cancel this exact attempt.
        Disposition::AlreadyAccepted
    } else {
        if hot.cancellation_id.is_none() {
            let revision = row
                .get::<i64, _>("authority_revision")
                .checked_add(1)
                .ok_or(AppError::Internal)?;
            sqlx::query("INSERT INTO profiled_cancellations(attempt_id,cancellation_id,binding,reason,authority_revision,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(attempt).bind(operation).bind(json!(binding)).bind(reason).bind(revision).bind(now as i64).execute(&mut *tx).await.map_err(db)?;
            hot.cancellation_id = Some(operation);
            hot.revoked = true;
            if hot.grant.is_some() && !hot.stop.known_stopped() {
                hot.status = Status::Cancelling;
            }
            hot.validate_against(&binding)
                .map_err(|_| AppError::Internal)?;
            sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
                .bind(attempt)
                .bind(json!(hot))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            // Revoke launch-scoped access without claiming its writer stopped.
            sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=$1")
                .bind(attempt).execute(&mut *tx).await.map_err(db)?;
        }
        if matches!(hot.status, Status::Offered | Status::Preparing) && hot.grant.is_none() {
            super::no_grant::close_locked(state, &mut tx, node, attempt, hot.cancellation_id)
                .await?
                .ok_or_else(|| {
                    AppError::Conflict("cancellation absence proof unavailable".into())
                })?;
            Disposition::NeverGranted
        } else if hot.status == Status::NeverGranted {
            Disposition::NeverGranted
        } else {
            settle_stopped_locked(&mut tx, attempt, now).await?;
            Disposition::Requested
        }
    };
    let revision: i64 = sqlx::query_scalar(
        "SELECT authority_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let receipt = Receipt {
        schema_version: 1,
        operation_id: operation,
        binding,
        cancellation_id: hot.cancellation_id,
        authority_revision: revision as u64,
        disposition,
        recorded_at_ms: now,
    };
    sqlx::query("INSERT INTO profiled_cancel_operations(operation_id,attempt_id,request,response) VALUES($1,$2,$3,$4)")
        .bind(operation).bind(attempt).bind(request).bind(json!(receipt)).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}

/// Called only with the common node/attempt locks held, after the actual stop
/// owner committed its verified row and hot projection in this transaction.
/// Also covers cancellation arriving after a previously recorded actual stop.
pub(crate) async fn settle_stopped_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    now: u64,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO profiled_cancelled_settlements(attempt_id,cancellation_id,stop_receipt_digest,recorded_at_ms) SELECT c.attempt_id,c.cancellation_id,h.state#>>'{stop,receipt,digest}',$2 FROM profiled_cancellations c JOIN profiled_authority_hot h USING(attempt_id) JOIN profiled_stopped_receipts s USING(attempt_id) WHERE c.attempt_id=$1 AND h.state#>>'{stop,kind}'='stopped' ON CONFLICT(attempt_id) DO NOTHING")
        .bind(attempt).bind(now as i64).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

/// Cancellation delivery requires the current credential and exact admitted
/// boot, but remains available when scheduling configuration/inventory expires.
pub(crate) async fn pending(
    state: &AppState,
    auth: &AuthenticatedNode,
    identity: &WorkerIdentity,
) -> Result<Vec<Cancellation>, AppError> {
    if identity.node_id != auth.id() {
        return Err(AppError::Forbidden("cancellation node differs".into()));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    super::registration::admitted_runtime_locked(&mut tx, auth.id(), identity.boot_id).await?;
    // Frozen fixture binaries predate cancellation delivery. Only explicit
    // historical construction selects this path; current startup still refuses
    // missing cancellation storage, including in test-support builds.
    #[cfg(feature = "test-support")]
    if state
        .store
        .fixture_profile
        .is_some_and(|version| version < 43)
    {
        tx.commit().await.map_err(db)?;
        return Ok(Vec::new());
    }
    let rows=sqlx::query("SELECT c.cancellation_id,c.binding,h.authority_revision FROM profiled_cancellations c JOIN execution_attempts a USING(attempt_id) JOIN profiled_authority_hot h USING(attempt_id) WHERE a.node_id=$1 AND c.binding#>>'{session,boot_id}'=$2 AND NOT EXISTS(SELECT 1 FROM profiled_cancelled_settlements s WHERE s.attempt_id=c.attempt_id) ORDER BY (h.state->>'status'='never_granted'),c.recorded_at_ms DESC,c.attempt_id LIMIT 32")
        .bind(auth.id()).bind(identity.boot_id.to_string()).fetch_all(&mut *tx).await.map_err(db)?;
    let directives = rows
        .into_iter()
        .map(|r| {
            Ok(Cancellation {
                cancellation_id: r.get("cancellation_id"),
                binding: serde_json::from_value(r.get("binding"))
                    .map_err(|_| AppError::Internal)?,
                authority_revision: r.get::<i64, _>("authority_revision") as u64,
                reason: CancellationReason::OwnerRequest,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    tx.commit().await.map_err(db)?;
    Ok(directives)
}
