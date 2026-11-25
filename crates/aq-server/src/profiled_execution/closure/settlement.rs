//! Settlement of a granted launch the worker closed before Started (B5b/AQ-08).
//!
//! The launch-closure routes record `LaunchClosedAwaitingSettlement`: they
//! revoke private permission and the agent binding but keep the task, source
//! and effect holds. This owner settles such a closure from server-owned facts
//! only, so B4 can replace the attempt:
//!
//! - **stop**: `not_started`, with the closure receipt, only when no Started,
//!   Stopped or result record exists and the closure evidence proves that no
//!   harness pane ever existed (a durable suppression, or a host stopped proof
//!   without a pane). Any other closure stays held for an operator.
//! - **retention**: `retained` with the attempt's own verified central source
//!   export. A launch that never started produced no work; its source is
//!   exactly that retained export, which a B4 successor reuses unchanged.
//! - **effects**: `quiescent` only when the attempt has no AQ Graph operation
//!   and no mailbox operation at all.
//!
//! It never releases the claim, completes the task or touches Graph: B4's
//! replacement owner re-assesses everything under its locks and releases.
use super::database;
use crate::machine_api::mapped;
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, hot::HotState},
        profiled::{Effects, Retention, Status, Stop},
    },
};
use aq_admission::{ArtifactRef, Digest, LaunchBinding, Text, granted_closure};
use aq_machine_control::{closed_launch, lapse};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosureSettlement {
    /// Settled now: not started, source retained, effects quiescent.
    Settled,
    /// Settled, and its task claim released through B4 (node freed).
    Released,
    /// Already settled, or no longer a held closed attempt.
    Unchanged,
    /// Kept held: the closure does not prove a never-started harness, or
    /// effects/source are not provably settled. An operator decides.
    Held(&'static str),
}

pub(crate) fn reference(id: String, value: &impl Serialize) -> Result<ArtifactRef, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| AppError::Internal)?,
        version: Text::new("1").map_err(|_| AppError::Internal)?,
        digest: Digest::of(&serde_json::to_vec(&value).map_err(|_| AppError::Internal)?),
    })
}

/// With an owner's `attest`ation (`aq_machine_control::closed_launch`) a closure that does not
/// prove nonstart settles too, when every condition of it holds under these locks.
pub async fn settle(
    state: &AppState,
    attempt: Uuid,
    attest: Option<&closed_launch::Attestation>,
) -> Result<ClosureSettlement, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for sql in [
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(database)?;
    }
    let closure = sqlx::query(
        "SELECT binding,operation_id,request_bytes,receipt_digest FROM \
         profiled_launch_closures WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?;
    // Without a closure only a granted, never-started attempt is settled here, once its grant
    // has expired (`aq_machine_control::lapse`).
    let binding = match &closure {
        Some(closure) => closure.get("binding"),
        None if attest.is_some() => {
            return Ok(ClosureSettlement::Held(closed_launch::NOT_HOST_STOPPED));
        }
        None => match lapse::pending(&mut tx, attempt).await.map_err(mapped)? {
            Some((binding, _)) => binding,
            None => return Ok(ClosureSettlement::Unchanged),
        },
    };
    let binding: LaunchBinding = serde_json::from_value(binding).map_err(|_| AppError::Internal)?;
    if binding.attempt_id != attempt {
        return Err(AppError::Internal);
    }
    // Common node -> attempt/hot locks; a revoked node's history still settles.
    let view = authority::load_locked(
        state,
        &mut tx,
        &binding.session,
        authority::Policy::IdentityOnly,
    )
    .await?;
    let mut hot: HotState = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await
        .map_err(database)?,
    )
    .map_err(|_| AppError::Internal)?;
    if !view.holds_task || !matches!(hot.stop, Stop::Unknown) {
        tx.commit().await.map_err(database)?;
        return Ok(ClosureSettlement::Unchanged);
    }
    if let Some(closure) = &closure {
        if hot.status != Status::Cancelling || !hot.revoked {
            return Ok(ClosureSettlement::Held("closure_authority_not_revoked"));
        }
        let request: granted_closure::Request =
            serde_json::from_slice(&closure.get::<Vec<u8>, _>("request_bytes"))
                .map_err(|_| AppError::Internal)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if request.binding != binding {
            return Err(AppError::Internal);
        }
        if attest.is_some() || !request.evidence.proves_never_started() {
            let now = (state.clock)().timestamp_millis();
            let checked = closed_launch::check(&mut tx, attempt, attest, now).await;
            if let Err(code) = checked.map_err(mapped)? {
                return Ok(ClosureSettlement::Held(code));
            }
        }
    } else {
        // Sampled under the locks Started and renewal take: both refuse an expired grant, so
        // from here on this attempt can never start. A closure recorded meanwhile owns it.
        let now = (state.clock)().timestamp_millis().max(0) as u64;
        let expired = (hot.grant.as_ref()).is_some_and(|g| now >= g.expires_at_ms);
        if !expired
            || !matches!(hot.status, Status::Granted | Status::Cancelling)
            || super::ensure_launch_open_locked(&mut tx, &binding)
                .await
                .is_err()
        {
            return Ok(ClosureSettlement::Unchanged);
        }
    }
    // Neither execution history nor any AQ-mediated effect may exist for a never-started launch.
    let used = closed_launch::used(&mut tx, attempt).await;
    if let Some(code) = used.map_err(mapped)? {
        return Ok(ClosureSettlement::Held(code));
    }
    let source_receipt = source_receipt_locked(&mut tx, attempt).await?;
    let (operation, stop_receipt) =
        stop_receipt_locked(&mut tx, attempt, closure.as_ref(), &mut hot).await?;
    let effects_receipt = reference(
        format!("aq.closure-effects/{operation}"),
        &json!({"attempt_id": attempt, "graph_operations": 0, "mailbox_operations": 0, "closure": stop_receipt}),
    )?;
    hot.status = Status::Finished;
    hot.stop = Stop::NotStarted {
        receipt: stop_receipt,
    };
    hot.retention = Retention::Retained {
        receipt: source_receipt,
    };
    hot.effects = Effects::Quiescent {
        receipt: effects_receipt,
    };
    hot.validate_against(&binding)
        .map_err(|_| AppError::Internal)?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(serde_json::to_value(&hot).map_err(|_| AppError::Internal)?)
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    tx.commit().await.map_err(database)?;
    Ok(ClosureSettlement::Settled)
}

async fn source_receipt_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<ArtifactRef, AppError> {
    // Retention: the attempt's own verified central export (its whole input).
    let linked = crate::profiled_execution::source_exports::load_linked_locked(tx, attempt).await?;
    let export = linked.token().request().operation_id;
    let manifest = linked.manifest();
    reference(
        format!("aq.source-export/{export}"),
        &json!({"operation_id": export, "manifest_digest": manifest.manifest_digest, "bundle_digest": manifest.bundle_digest, "checkpoint": manifest.checkpoint}),
    )
}

async fn stop_receipt_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    closure: Option<&PgRow>,
    hot: &mut HotState,
) -> Result<(Uuid, ArtifactRef), AppError> {
    match closure {
        Some(closure) => {
            let operation: Uuid = closure.get("operation_id");
            let stop = ArtifactRef {
                id: Text::new(format!("aq.launch-closure/{operation}"))
                    .map_err(|_| AppError::Internal)?,
                version: Text::new("1").map_err(|_| AppError::Internal)?,
                digest: serde_json::from_value(json!(closure.get::<String, _>("receipt_digest")))
                    .map_err(|_| AppError::Internal)?,
            };
            Ok((operation, stop))
        }
        None => {
            tracing::warn!(%attempt, reason = lapse::REASON, "granted attempt never started and its grant expired; closed by the server");
            crate::profiled_execution::journal::revoke(tx, attempt).await?;
            hot.revoked = true;
            let record =
                json!({"attempt_id": attempt, "reason": lapse::REASON, "grant": hot.grant});
            let stop = reference(format!("{}{attempt}", lapse::RECEIPT), &record)?;
            Ok((attempt, stop))
        }
    }
}

/// Settles a closed launch, then releases its task claim through B4 when B4
/// admits a replacement: the claim of a never-started attempt is what keeps
/// its node from re-registering (the incarnation guard only accepts stopped
/// compute), so it is released at once rather than at a later poll that this
/// node could never make. If B4 blocks (for example an exhausted budget) the
/// claim stays held and the reason is returned.
pub async fn settle_and_release(
    state: &AppState,
    attempt: Uuid,
    attest: Option<&closed_launch::Attestation>,
) -> Result<ClosureSettlement, AppError> {
    use crate::profiled_execution::replacement::{self, Assessment};
    let settled = settle(state, attempt, attest).await?;
    if matches!(settled, ClosureSettlement::Held(_)) {
        return Ok(settled);
    }
    let row: Option<(Uuid, bool, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT a.task_id,a.holds_task,h.state#>>'{stop,kind}',h.state#>>'{stop,receipt,id}' \
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE \
         a.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(database)?;
    let Some((task, true, Some(stop), receipt)) = row else {
        return Ok(settled);
    };
    if stop != "not_started" {
        return Ok(settled);
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    // A lapsed attempt always frees its task and node; a later offer is assessed by B4.
    if receipt.is_some_and(|id| id.starts_with(lapse::RECEIPT)) {
        let freed = lapse::release(&state.store.pool, attempt, now as i64).await;
        return Ok(match freed.map_err(mapped)? {
            true => ClosureSettlement::Released,
            false => settled,
        });
    }
    match replacement::assess_task(state, task, now).await? {
        Assessment::Replace(r) if r.predecessor == attempt => {
            replacement::release(state, task, &r).await?;
            Ok(ClosureSettlement::Released)
        }
        Assessment::Blocked(reason) => Ok(ClosureSettlement::Held(reason)),
        _ => Ok(ClosureSettlement::Held("closure_release_not_admitted")),
    }
}

/// Held closed or lapsed attempts awaiting this owner, after `after`. Bounded cursor.
pub async fn awaiting(
    state: &AppState,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, AppError> {
    let now = (state.clock)().timestamp_millis();
    (lapse::awaiting(&state.store.pool, after, now, limit).await).map_err(mapped)
}
