//! Swarm phase 1, D2 ruling C: a restarted node releases an attempt whose host was lost (its
//! guardian cannot prove the stop), with the outside proof that every process of the host is
//! dead (aq-protocol `StopProof`). The attempt fails as `lost_after_reboot` (not charged to the
//! lineage budgets) or `lost_after_crash` (charged); its task is retried from the attempt's
//! own retained input. Nothing of its work is salvaged.
use super::{
    closure::{database, settlement as closure},
    replacement,
};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::hot::HotState,
        profiled::{Effects, Retention, Status, Stop},
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::LaunchBinding;
use aq_machine_control::lost;
use aq_protocol::{LostAttemptRelease, LostAttemptReleased};
use serde_json::{Value, json};
use uuid::Uuid;

fn refused(code: &str) -> AppError {
    AppError::Conflict(code.into())
}
pub async fn release(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &LostAttemptRelease,
) -> Result<LostAttemptReleased, AppError> {
    let attempt = request.attempt_id;
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    crate::worker_api::lock_current_node(&mut tx, node).await?;
    let mc = super::super::machine_api::mapped;
    // Ownership first: a node learns nothing of another node's attempt, a replay included.
    lost::owned_locked(&mut tx, node.id(), request)
        .await
        .map_err(mc)?;
    if let Some((proof, reason, charged)) = lost::release_of(&mut tx, attempt).await.map_err(mc)? {
        // A replay retries the release; another proof for the same attempt is refused.
        if proof != request.proof {
            return Err(refused("lost_attempt_released_with_another_proof"));
        }
        tx.commit().await.map_err(database)?;
        return retry(state, attempt, reason, charged).await;
    }
    let found = lost::check_locked(&mut tx, node.id(), request)
        .await
        .map_err(mc)?;
    if closure::agent_effects_present(&mut tx, attempt).await? {
        return Err(refused(
            "lost_attempt_effects_present: an operator settles its effects",
        ));
    }
    let binding: LaunchBinding = serde_json::from_value(
        sqlx::query_scalar::<_, Value>(
            "SELECT state->'offer'->'binding' FROM execution_attempts WHERE attempt_id=$1",
        )
        .bind(attempt)
        .fetch_one(&mut *tx)
        .await
        .map_err(database)?,
    )
    .map_err(|_| AppError::Internal)?;
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
    // Retention: the attempt's own verified central export, which the retry starts from.
    let linked = super::source_exports::load_linked_locked(&mut tx, attempt)
        .await
        .map_err(|_| refused("lost_attempt_source_unknown: no verified export of its input"))?;
    let export = linked.token().request().operation_id;
    let manifest = linked.manifest();
    let source = closure::reference(
        format!("aq.source-export/{export}"),
        &json!({"operation_id": export, "manifest_digest": manifest.manifest_digest, "bundle_digest": manifest.bundle_digest, "checkpoint": manifest.checkpoint}),
    )?;
    let record = json!({"attempt_id": attempt, "node_id": node.id(), "proof": request.proof,
        "reason": found.reason, "charged": found.charged});
    let stop = closure::reference(format!("aq.lost-attempt-release/{attempt}"), &record)?;
    let effects = closure::reference(
        format!("aq.lost-attempt-effects/{attempt}"),
        &json!({"attempt_id": attempt, "graph_operations": 0, "mailbox_operations": 0, "release": stop}),
    )?;
    let now = (state.clock)();
    lost::record(
        &mut tx,
        node.id(),
        request,
        &found,
        stop.digest.as_str(),
        now,
    )
    .await
    .map_err(mc)?;
    tracing::warn!(%attempt, proof = ?request.proof, reason = ?found.reason, charged = found.charged,
        "lost attempt released by its node; its task is retried without its unsaved work");
    (hot.status, hot.stop) = (Status::Finished, Stop::Stopped { receipt: stop });
    hot.retention = Retention::Retained { receipt: source };
    hot.effects = Effects::Quiescent { receipt: effects };
    hot.validate_against(&binding)
        .map_err(|_| AppError::Internal)?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(serde_json::to_value(&hot).map_err(|_| AppError::Internal)?)
        .execute(&mut *tx)
        .await
        .map_err(database)?;
    let now_ms = u64::try_from(now.timestamp_millis()).map_err(|_| AppError::Internal)?;
    super::cancellation::settle_stopped_locked(&mut tx, attempt, now_ms).await?;
    tx.commit().await.map_err(database)?;
    retry(state, attempt, found.reason, found.charged).await
}
/// Releases the failed attempt's task claim for a retry, when B4 admits it.
async fn retry(
    state: &AppState,
    attempt: Uuid,
    reason: aq_protocol::LostReason,
    charged: bool,
) -> Result<LostAttemptReleased, AppError> {
    let (task, holds): (Uuid, bool) =
        sqlx::query_as("SELECT task_id,holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&state.store.pool)
            .await
            .map_err(database)?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    let held = match holds {
        false => None,
        true => match replacement::assess_task(state, task, now).await? {
            replacement::Assessment::Replace(r) if r.predecessor == attempt => {
                replacement::release(state, task, &r).await?;
                None
            }
            replacement::Assessment::Blocked(why) => Some(why.to_owned()),
            _ => Some("lost_attempt_release_not_admitted".to_owned()),
        },
    };
    // The node stays busy while B4 holds the task (`compute::busy_locked`); the reason is
    // recorded for the owner.
    let mut conn = state.store.pool.acquire().await.map_err(database)?;
    let mc = super::super::machine_api::mapped;
    lost::record_held(&mut conn, attempt, held.as_deref())
        .await
        .map_err(mc)?;
    if let Some(why) = &held {
        tracing::warn!(%attempt, held = why, "lost attempt failed; its task stays held");
    }
    Ok(LostAttemptReleased {
        schema_version: 1,
        attempt_id: attempt,
        reason,
        charged,
        released: held.is_none(),
        held,
    })
}
