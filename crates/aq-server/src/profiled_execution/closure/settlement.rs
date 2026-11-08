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
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, hot::HotState},
        profiled::{Effects, Retention, Status, Stop},
    },
};
use aq_admission::{ArtifactRef, Digest, LaunchBinding, Text, granted_closure};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::Row;
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

/// Whether the worker's retained closure evidence shows that no harness pane
/// ever existed. Pure; the closure route already verified its correlation.
pub fn proves_never_started(evidence: &granted_closure::Evidence) -> bool {
    match evidence {
        granted_closure::Evidence::Suppressed { .. } => true,
        granted_closure::Evidence::HostStopped { proof, .. } => paneless(&proof.observation),
    }
}
/// A host whose stopped proof shows that no process ever ran under it: no
/// pane root status, no reaped process, and a kernel wait that sees no
/// children. This is the "host started, harness never launched" case (the
/// guardian stopped before its gate created a pane). A host that created a
/// pane always reaped at least its tmux processes, so any pane fails this
/// rule and the attempt stays held for an operator.
fn paneless(o: &granted_closure::StoppedObservation) -> bool {
    o.kernel_wait_echild && o.root_status.is_none() && o.reaped_count == 0
}

pub async fn settle(state: &AppState, attempt: Uuid) -> Result<ClosureSettlement, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(database)?;
    for sql in [
        "SET LOCAL lock_timeout='500ms'",
        "SET LOCAL statement_timeout='2s'",
    ] {
        sqlx::query(sql).execute(&mut *tx).await.map_err(database)?;
    }
    let Some(closure) = sqlx::query(
        "SELECT binding,operation_id,request_bytes,receipt_digest FROM profiled_launch_closures WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await
    .map_err(database)?
    else {
        return Ok(ClosureSettlement::Unchanged);
    };
    let binding: LaunchBinding =
        serde_json::from_value(closure.get("binding")).map_err(|_| AppError::Internal)?;
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
    if !proves_never_started(&request.evidence) {
        return Ok(ClosureSettlement::Held("closure_does_not_prove_nonstart"));
    }
    let history: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_started_receipts WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM profiled_stopped_receipts WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM profiled_result_reports WHERE attempt_id=$1) \
         OR EXISTS(SELECT 1 FROM source_transfers WHERE attempt_id=$1)",
    )
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await
    .map_err(database)?;
    if history {
        return Ok(ClosureSettlement::Held("closure_has_execution_history"));
    }
    // Effects: no AQ-mediated effect may exist at all for a never-started launch.
    if agent_effects_present(&mut tx, attempt).await? {
        return Ok(ClosureSettlement::Held("closure_effects_present"));
    }
    // Retention: the attempt's own verified central export (its whole input).
    let linked =
        crate::profiled_execution::source_exports::load_linked_locked(&mut tx, attempt).await?;
    let export = linked.token().request().operation_id;
    let manifest = linked.manifest();
    let source_receipt = reference(
        format!("aq.source-export/{export}"),
        &json!({"operation_id": export, "manifest_digest": manifest.manifest_digest, "bundle_digest": manifest.bundle_digest, "checkpoint": manifest.checkpoint}),
    )?;
    let operation: Uuid = closure.get("operation_id");
    let stop_receipt = ArtifactRef {
        id: Text::new(format!("aq.launch-closure/{operation}")).map_err(|_| AppError::Internal)?,
        version: Text::new("1").map_err(|_| AppError::Internal)?,
        digest: serde_json::from_value(json!(closure.get::<String, _>("receipt_digest")))
            .map_err(|_| AppError::Internal)?,
    };
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

/// Whether the attempt's agent made any AQ-mediated effect: a graph operation, or mail sent
/// from its session (a retry or closure would leave or repeat it).
pub(crate) async fn agent_effects_present(
    conn: &mut sqlx::PgConnection,
    attempt: Uuid,
) -> Result<bool, AppError> {
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_graph_operations WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut *conn)
            .await
            .map_err(database)?;
    let agent: Option<Value> =
        sqlx::query_scalar("SELECT binding FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_optional(&mut *conn)
            .await
            .map_err(database)?;
    let mailed = match &agent {
        None => false,
        Some(binding) => {
            let key = aq_agent_api::digest(&json!({"kind":"session","binding":binding}))
                .map_err(|_| AppError::Internal)?;
            sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mailbox_operations WHERE sender_key=$1)",
            )
            .bind(key)
            .fetch_one(&mut *conn)
            .await
            .map_err(database)?
        }
    };
    Ok(operations != 0 || mailed)
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
) -> Result<ClosureSettlement, AppError> {
    use crate::profiled_execution::replacement::{self, Assessment};
    let settled = settle(state, attempt).await?;
    if matches!(settled, ClosureSettlement::Held(_)) {
        return Ok(settled);
    }
    let row: Option<(Uuid, bool, Option<String>)> = sqlx::query_as(
        "SELECT a.task_id,a.holds_task,h.state#>>'{stop,kind}' FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) JOIN profiled_launch_closures c USING(attempt_id) WHERE a.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(database)?;
    let Some((task, true, Some(stop))) = row else {
        return Ok(settled);
    };
    if stop != "not_started" {
        return Ok(settled);
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| AppError::Internal)?;
    match replacement::assess_task(state, task, now).await? {
        Assessment::Replace(r) if r.predecessor == attempt => {
            replacement::release(state, task, &r).await?;
            Ok(ClosureSettlement::Released)
        }
        Assessment::Blocked(reason) => Ok(ClosureSettlement::Held(reason)),
        _ => Ok(ClosureSettlement::Held("closure_release_not_admitted")),
    }
}

/// Held closed attempts with no known stop, after `after`. Bounded cursor.
pub async fn awaiting(
    state: &AppState,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar(
        "SELECT c.attempt_id FROM profiled_launch_closures c JOIN execution_attempts a USING(attempt_id) \
         JOIN profiled_authority_hot h USING(attempt_id) WHERE a.kind='profiled_v1' AND a.holds_task \
         AND h.state#>>'{stop,kind}' IN ('unknown','not_started') AND ($1::UUID IS NULL OR c.attempt_id>$1) \
         ORDER BY c.attempt_id LIMIT $2",
    )
    .bind(after)
    .bind(limit)
    .fetch_all(&state.store.pool)
    .await
    .map_err(database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use granted_closure::StoppedObservation;
    fn observation(pane: bool) -> StoppedObservation {
        let session = json!({"kind":"remote","node_id":Uuid::new_v4(),"boot_id":Uuid::new_v4(),"session_id":Uuid::new_v4(),"attempt_id":Uuid::new_v4(),"assignment_generation":1});
        let guardian = json!({"intent":{"launch_id":Uuid::new_v4(),"session":session,"manifest_digest":"a".repeat(64),"configuration_digest":"b".repeat(64)},"process":{"pid":10,"start_ticks":5},"nonce":Uuid::new_v4(),"machine_boot_id":Uuid::new_v4(),"pid_namespace":"pid:[4026531836]"});
        let p = pane.then(|| json!({"server":{"pid":11,"start_ticks":6},"pane":{"pid":12,"start_ticks":7},"pane_id":"%1","session_name":"aq"}));
        serde_json::from_value(json!({"guardian":guardian,"phase":"stopped","pane":p,"permit_sequence":1,"permit_deadline_boot_ms":1,"observed_boot_ms":1,"reaped_count":u64::from(pane),"kernel_wait_echild":true,"root_status":if pane {json!(0)} else {Value::Null},"stop_reason":null})).unwrap()
    }
    #[test]
    fn only_a_host_stopped_before_any_pane_proves_nonstart() {
        let none = observation(false);
        none.validate().unwrap();
        assert!(paneless(&none));
        let ran = observation(true);
        ran.validate().unwrap();
        assert!(
            !paneless(&ran),
            "a pane that existed may have run the harness"
        );
        let mut reaped = observation(false);
        reaped.reaped_count = 1;
        assert!(!paneless(&reaped));
        // A pane that ran and was reaped, even without a root status, holds.
        let mut paned = observation(true);
        paned.root_status = None;
        assert!(!paneless(&paned));
        // Children still waited on (no ECHILD) is not a stop proof of nothing.
        let mut waiting = observation(false);
        waiting.kernel_wait_echild = false;
        assert!(!paneless(&waiting));
    }
}
