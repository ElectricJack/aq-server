//! D3 automatic settlement: a policy actor evaluates each recorded result under
//! the retained default completion policy. The AQ row is evidence, never task
//! state; aq-graph owns completion and only its validated receipt confirms it.
//! Attempts with a Git grant are settled by the delivery playbook.
use super::facts;
use super::{completion, owner::*};
use crate::execution::authority::{self, Policy};
use crate::{AppState, error::AppError, profiled_execution::results};
use aq_playbook_domain::delivery_decisions::Settled;
use aq_profiled_state::default_policy::{self as policy, Acceptance, Delivery, Verdict};
use aq_profiled_state::graph_completion::{
    self, GRAPH_PERMISSION, RECEIPT_CONFLICT, VERSION_CONFLICT, terminal_conflict,
};
pub use aq_profiled_state::graph_completion::{AutoDisposition, AutoSettlement};
pub use aq_profiled_state::result_reads::ReplacementFacts;
use serde_json::{Value, json};
use sqlx::{PgConnection, Postgres, Row, Transaction};
use uuid::Uuid;

pub(super) mod publication_retry;

const OWNER: &str = "aq.settlement-owner.auto.1";
const ACTOR: &str = "aq.settlement.default-policy@1";
fn status(row: &sqlx::postgres::PgRow) -> Result<AutoSettlement> {
    graph_completion::status(row).map_err(db)
}
pub(super) async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    graph_completion::fence_transaction(&mut tx, OWNER)
        .await
        .map_err(db)?;
    Ok(tx)
}
// The stopped result remains immutable even when the harness naturally exits
// before receiving the handoff. Record checkpoint rework in the same write.
pub(crate) async fn settle_workspace_checkpoint_locked(
    connection: &mut PgConnection,
    receipt: &aq_admission::result_record::Receipt,
) -> Result<bool> {
    let attempt = receipt.binding.attempt_id;
    if !crate::task_workspace_resume::checkpoint_required(connection, attempt).await? {
        return Ok(false);
    }
    graph_completion::fence_transaction(connection, OWNER)
        .await
        .map_err(db)?;
    graph_completion::record_noncompletion(
        connection,
        receipt.report_id,
        attempt,
        (ACTOR, "rework_required", crate::task_workspace_resume::REQUESTED),
        &serde_json::json!({"decided_by":"workspace_expansion","report_digest":receipt.report_digest}),
        i64::try_from(receipt.recorded_at_ms).map_err(|_| AppError::Internal)?,
    )
    .await
    .map_err(db)?
    .ok_or(AppError::Internal)?;
    Ok(true)
}

/// Only a Success report is acceptable (MS1 review B1). A native worker reports
/// Success only for a harness exit it proved (exit 0 plus an explicit claim);
/// Unknown, Failed, Expired and Cancelled go to rework, never to completion.
fn outcome_acceptable(outcome: aq_admission::transport::ProcessOutcome) -> bool {
    matches!(outcome, aq_admission::transport::ProcessOutcome::Success)
}
/// The retained default-completion policy, or None (no rows are written).
fn applicable(state: &AppState, r: &results::RecordedResult) -> Option<Delivery> {
    if r.outcome.review != aq_admission::outcome::ReviewRequirement::PolicyActor {
        return None;
    }
    let config = state.config.profiled_execution.as_deref()?;
    let definition = config
        .definition_bodies
        .iter()
        .find(|d| d.artifact() == &r.outcome.validation_policy)?;
    policy::parse(definition.body())
}

/// Runs after a result is durably recorded (new or duplicate POST) and on
/// explicit replay. Idempotent per report; a confirmed or rework row is final.
pub async fn settle_after_record(
    state: &AppState,
    report_id: Uuid,
) -> Result<Option<AutoSettlement>> {
    let mut tx = begin(state).await?;
    let r = results::load_locked(state, &mut tx, report_id).await?;
    if super::super::has_delivery_grant(&mut *tx, r.report.binding.attempt_id).await? {
        return Ok(None);
    }
    let Some(delivery) = applicable(state, &r) else {
        return Ok(None);
    };
    aq_profiled_state::result_reads::lock_settlement(&mut tx, report_id)
        .await
        .map_err(db)?;
    let existing = graph_completion::lock_row(&mut tx, report_id)
        .await
        .map_err(db)?;
    let mut conflict = None;
    if let Some(row) = &existing {
        let s = status(row)?;
        match s.disposition {
            AutoDisposition::Confirmed => {
                tx.commit().await.map_err(db)?;
                return Ok(Some(s));
            }
            // A replay of a final rework also (re)records the B4 replacement
            // facts, which a first evaluation may have found incomplete.
            AutoDisposition::ReworkRequired => {
                tx.commit().await.map_err(db)?;
                Box::pin(replacement_facts_after_rework(state, report_id)).await;
                return Ok(Some(s));
            }
            // Once possibly dispatched, only the SAME payload is replayed.
            AutoDisposition::Prepared | AutoDisposition::Possible => {
                return dispatch(state, tx, row, &r).await.map(Some);
            }
            AutoDisposition::Held => conflict = s.reason.filter(|reason| terminal_conflict(reason)),
        }
    }
    // H5: a parked (Held / RetainedSuspended) predecessor is suspended, never
    // success or failure; the hold survives until the successor's BootstrapAck.
    let suspended =
        crate::hibernation::suspended_hold(&mut tx, r.report.binding.attempt_id).await?;
    let (verdict, evidence) = match suspended {
        Some((reason, evidence)) => (Verdict::Hold(reason), evidence),
        None => evaluate_locked(state, &mut tx, &r, delivery).await?,
    };
    let verdict = if matches!(verdict, Verdict::Complete)
        && !crate::task_workspace_delivery::gate(state, &mut tx, &r, true).await?
    {
        Verdict::Hold("workspace_delivery_pending")
    } else {
        verdict
    };
    let time = now(state)?;
    let (disposition, reason) = match verdict {
        Verdict::Rework(reason) => ("rework_required", reason),
        // A version conflict re-evaluates but never re-dispatches the stale
        // expected version; only a new report can be settled again.
        Verdict::Hold(_) | Verdict::Complete if conflict.is_some() => {
            ("held", conflict.as_deref().ok_or_else(unavailable)?)
        }
        Verdict::Hold(reason) => ("held", reason),
        Verdict::Complete => {
            let row = prepare_locked(state, &mut tx, &r, evidence, time, "Default policy").await?;
            return dispatch(state, tx, &row, &r).await.map(Some);
        }
    };
    let s = graph_completion::record_noncompletion(
        &mut tx,
        report_id,
        r.report.binding.attempt_id,
        (ACTOR, disposition, reason),
        &evidence,
        time,
    )
    .await
    .map_err(db)?
    .ok_or(crate::error::AppError::Internal)?;
    tx.commit().await.map_err(db)?;
    if s.disposition == AutoDisposition::ReworkRequired {
        Box::pin(replacement_facts_after_rework(state, report_id)).await;
    }
    Ok(Some(s))
}

/// Best effort after the rework row is durable: a failure here never changes
/// or fails the settlement; the replay and the sweep record the facts later.
async fn replacement_facts_after_rework(state: &AppState, report_id: Uuid) {
    match record_replacement_facts(state, report_id).await {
        Ok(facts) if facts.changed => {
            tracing::info!(%report_id, source_retained = facts.source_retained, effects_quiescent = facts.effects_quiescent, "replacement facts recorded for reworked attempt");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%report_id, ?error, "replacement facts deferred");
        }
    }
}

/// Prove retained source and quiescent effects before replacement can release
/// the claim, including after a verified platform interruption or a delivery hold.
pub async fn record_replacement_facts(
    state: &AppState,
    report_id: Uuid,
) -> Result<ReplacementFacts> {
    replacement_facts(state, report_id, false, None).await
}

/// Reconcile the current delivery hold supported by its recorded worker and PR outcomes.
/// Resolve the current report from the task so an earlier result cannot authorize a retry.
pub async fn recover_held_for_retry(state: &AppState, task: Uuid) -> Result<()> {
    recover_held_for_retry_exact(state, task, None, None).await
}
pub(crate) async fn recover_held_for_retry_exact(
    state: &AppState,
    task: Uuid,
    predecessor: Option<Uuid>,
    owner_operation: Option<Uuid>,
) -> Result<()> {
    let report = aq_delivery::git_grant::held_report(&state.store.pool, task)
        .await
        .map_err(db)?;
    let report = match report {
        Some(_) => report,
        None => aq_delivery::git_grant::cancelled_failed_report(&state.store.pool, task)
            .await
            .map_err(db)?,
    };
    let report = match (report, predecessor, owner_operation) {
        (None, Some(attempt), Some(_)) => sqlx::query_scalar(
            "SELECT h.report_id FROM profiled_result_heads h JOIN execution_attempts a USING(attempt_id)
             WHERE h.attempt_id=$1 AND a.task_id=$2 AND a.holds_task",
        )
        .bind(attempt)
        .bind(task)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(db)?,
        _ => report,
    };
    if let Some(report) = report
        && aq_delivery::git_grant::is_retry_report(&state.store.pool, report, predecessor)
            .await
            .map_err(db)?
    {
        Box::pin(replacement_facts(state, report, true, owner_operation)).await?;
    }
    Ok(())
}

async fn replacement_facts(
    state: &AppState,
    report_id: Uuid,
    owner_retry: bool,
    _owner_operation: Option<Uuid>,
) -> Result<ReplacementFacts> {
    use crate::execution::{
        authority::{self, hot::HotState},
        profiled::{Effects, Retention},
    };
    let mut tx = begin(state).await?;
    let r = results::load_locked(state, &mut tx, report_id).await?;
    aq_profiled_state::result_reads::lock_settlement(&mut tx, report_id)
        .await
        .map_err(db)?;
    let view = authority::load_locked(
        state,
        &mut tx,
        &r.report.binding.session,
        authority::Policy::IdentityOnly,
    )
    .await?;
    // An owner can request a replacement of a proven stopped cancellation with
    // a pinned failed result. Recheck the exact eligible report under the attempt locks.
    let cancelled_retry = owner_retry
        && view.cancellation_id.is_some()
        && aq_delivery::git_grant::cancelled_failed_report(&mut *tx, r.spec.task.task_id)
            .await
            .map_err(db)?
            == Some(report_id);
    let settlement = graph_completion::lock_row(&mut tx, report_id)
        .await
        .map_err(db)?
        .as_ref()
        .map(status)
        .transpose()?;
    let platform_retry = r.report.platform_interrupted()
        && !view.revoked
        && view.cancellation_id.is_none()
        && settlement
            .as_ref()
            .is_none_or(|s| s.disposition == AutoDisposition::Held && s.operation_id.is_none())
        && crate::hibernation::suspended_hold(&mut tx, r.report.binding.attempt_id)
            .await?
            .is_none();
    let reworked = match settlement {
        Some(ref s) => s.disposition == AutoDisposition::ReworkRequired,
        None if owner_retry || (!view.revoked && view.cancellation_id.is_none()) => {
            cancelled_retry
                || aq_delivery::git_grant::held_report(&mut *tx, r.spec.task.task_id)
                    .await
                    .map_err(db)?
                    == Some(report_id)
        }
        None => false,
    };
    let publication_retry = match _owner_operation {
        Some(operation) if !reworked && !platform_retry && settlement.is_none() => {
            publication_retry::evidence(&mut tx, &r, operation).await?
        }
        _ => None,
    };
    let attempt = r.report.binding.attempt_id;
    let owner_settled = aq_profiled_state::result_reads::owner_settled(&mut tx, attempt)
        .await
        .map_err(db)?;
    let mut hot = aq_profiled_state::result_reads::lock_hot_state(&mut tx, attempt)
        .await
        .map_err(db)?;
    let current = |hot: &HotState| ReplacementFacts {
        source_retained: matches!(hot.retention, Retention::Retained { .. }),
        effects_quiescent: matches!(hot.effects, Effects::Quiescent { .. }),
        changed: false,
    };
    if !(reworked || platform_retry || publication_retry.is_some())
        || !r.is_current_report_head
        || !view.holds_task
        || !view.stopped
        || (owner_retry && (view.revoked || view.cancellation_id.is_some()) && !cancelled_retry)
        || !hot.stop.known_stopped()
        || owner_settled
    {
        let facts = current(&hot);
        tx.commit().await.map_err(db)?;
        return Ok(facts);
    }
    let mut changed = retain_replacement_source_locked(&mut tx, &r, &mut hot).await?;
    changed |= settle_replacement_effects_locked(state, &mut tx, &r, &mut hot).await?;
    if changed {
        hot.validate_against(&r.report.binding)
            .map_err(|_| crate::error::AppError::Internal)?;
        aq_profiled_state::result_reads::save_hot_state(&mut tx, attempt, &hot)
            .await
            .map_err(db)?;
    }
    let facts = ReplacementFacts {
        changed,
        ..current(&hot)
    };
    if let Some(evidence) = publication_retry
        && facts.source_retained
        && facts.effects_quiescent
    {
        graph_completion::record_noncompletion(
            &mut tx,
            report_id,
            attempt,
            (ACTOR, "rework_required", publication_retry::REASON),
            &evidence,
            now(state)?,
        )
        .await
        .map_err(db)?
        .ok_or(AppError::Internal)?;
    }
    if platform_retry && facts.source_retained && facts.effects_quiescent {
        let reason = aq_machine_control::platform_retry::REASON;
        let evidence =
            json!({"policy_actor":ACTOR,"decided_by":"platform_retry","report":r.receipt_artifact});
        let time = now(state)?;
        let _ = graph_completion::record_noncompletion(
            &mut tx,
            report_id,
            attempt,
            (ACTOR, "rework_required", reason),
            &evidence,
            time,
        )
        .await
        .map_err(db)?;
        if super::super::has_delivery_grant(&mut *tx, attempt).await? {
            aq_playbook_domain::delivery_decisions::record_platform_return(
                &mut tx,
                r.spec.task.task_id,
                report_id,
                time,
            )
            .await
            .map_err(|_| crate::error::AppError::Internal)?;
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(facts)
}

async fn retain_replacement_source_locked(
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    hot: &mut crate::execution::authority::hot::HotState,
) -> Result<bool> {
    use crate::execution::profiled::Retention;
    let attempt = r.report.binding.attempt_id;
    let report_id = r.report.report_id;
    if matches!(hot.retention, Retention::Retained { .. }) {
        return Ok(false);
    }
    let enrolled = super::super::has_delivery_grant(&mut **tx, attempt).await?;
    if enrolled
        && let Some(pushed) = aq_delivery::git_grant::pushed_of(&mut **tx, report_id)
            .await
            .map_err(db)?
    {
        let reference = reference(format!("aq.git-task-ref/{report_id}"), &json!(pushed))?;
        hot.retention = Retention::Retained { receipt: reference };
        return Ok(true);
    }
    if !enrolled {
        for op in &r.report.source_receipt_ids {
            match crate::source_retention::load_confirmed_locked(tx, *op, r).await {
                Ok(confirmed) => {
                    hot.retention = Retention::Retained {
                        receipt: reference(
                            format!("aq.git-import-receipt/{}", confirmed.receipt.receipt_id),
                            &confirmed.acknowledgement,
                        )?,
                    };
                    return Ok(true);
                }
                Err(crate::error::AppError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(false)
}
async fn settle_replacement_effects_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    hot: &mut crate::execution::authority::hot::HotState,
) -> Result<bool> {
    use crate::execution::profiled::Effects;
    let report_id = r.report.report_id;
    if matches!(hot.effects, Effects::Quiescent { .. }) {
        return Ok(false);
    }
    let ledger = super::effects::inspect_aq_effects_locked(state, tx, r).await?;
    // Quiescence (every operation final), not the read-only acceptance verdict: a
    // messaging or write profile's finished, returned attempt is replaceable too.
    if !ledger.quiescent() {
        return Ok(false);
    }
    let mut fact = json!({"report": r.receipt_artifact, "stopped": r.receipt.stopped_receipt, "ledger": ledger.digest(), "operations": ledger.operation_count(), "policy_actor": ACTOR});
    if ledger.disposition() != super::effects::LedgerDisposition::Settled {
        fact["ledger_disposition"] = json!(ledger.disposition().name());
    }
    let id = format!("aq.replacement-effects/{report_id}");
    hot.effects = Effects::Quiescent {
        receipt: reference(id, &fact)?,
    };
    Ok(true)
}

/// Reworked attempts that still hold their task without both replacement
/// facts, in report order after `after`. Bounded; the cursor is disposable.
pub async fn reworked_without_facts(
    state: &AppState,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>> {
    aq_profiled_state::result_reads::reworked_without_facts(&state.store.pool, after, limit)
        .await
        .map_err(db)
}

/// Judges the server-owned facts ([`facts::gather_locked`]) under the
/// report/attempt locks with the default policy's gate.
async fn evaluate_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    delivery: Delivery,
) -> Result<(Verdict, Value)> {
    let g = facts::gather_locked(state, tx, r, false).await?;
    let v = &g.view;
    let acceptance = result_acceptance(r, v);
    let (source, lineage_unknown) = (&g.source, g.lineage_unknown);
    let unchanged = matches!(source, Some((_, _, false)));
    let conclusion = r.report.conclusion.as_str();
    let claimed = aq_profiled_state::claim::claims_changes(conclusion);
    let failed = matches!(
        r.report.outcome,
        aq_admission::transport::ProcessOutcome::Failed
    );
    let attributable = r.is_current_report_head && v.cancellation_id.is_none() && !v.revoked;
    let agent_blocked = failed && attributable && aq_profiled_state::claim::blocked(conclusion);
    let verdict = policy::evaluate(&policy::Gate {
        acceptance,
        source_retained: source.is_some(),
        claimed_changes_missing: unchanged && claimed,
        agent_blocked,
        lineage_unknown,
        checks: &g.checks,
        delivery,
        delivery_state: g.delivery_state,
    });
    let evidence = json!({
        "policy_actor": ACTOR,
        "policy": r.outcome.validation_policy,
        "report": r.receipt_artifact,
        "accepted": g.accepted(r),
        "source": g.source_evidence(),
        "checks": g.check_evidence,
        "delivery": {"policy": delivery, "state": g.delivery_state},
    });
    Ok((verdict, evidence))
}

fn result_acceptance(
    r: &results::RecordedResult,
    v: &crate::execution::authority::AuthorityView,
) -> Acceptance {
    if !r.is_current_report_head
        || !outcome_acceptable(r.report.outcome)
        || v.revoked
        || v.cancellation_id.is_some()
        || !v.holds_task
    {
        Acceptance::Refused
    } else if !v.stopped {
        Acceptance::NotStopped
    } else {
        Acceptance::Accepted
    }
}

async fn prepare_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    evidence: Value,
    time: i64,
    accepted_by: &str,
) -> Result<sqlx::postgres::PgRow> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let task = &r.spec.task;
    let graph = graph_identity(rt, task.workspace_id)?;
    let op = Uuid::new_v4();
    let payload = bytes(
        &json!({"idempotency_key":format!("aq-auto-settlement-v1:{op}"),"reason":format!("{accepted_by} accepted retained result {}",r.report.report_id),"operations":[{"op":"update_node","id":task.task_id,"expected_version":task.version,"patch":{"task_state":"completed"}}]}),
    )?;
    if payload.len() > 8192 {
        return Err(unavailable());
    }
    sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,reason,evidence,operation_id,workspace_id,task_id,expected_version,expected_content_revision,payload_bytes,created_at_ms,updated_at_ms,graph_service) VALUES($1,$2,$3,'prepared',NULL,$4,$5,$6,$7,$8,$9,$10,$11,$11,$12) ON CONFLICT(report_id) DO UPDATE SET disposition='prepared',reason=NULL,evidence=EXCLUDED.evidence,operation_id=EXCLUDED.operation_id,workspace_id=EXCLUDED.workspace_id,task_id=EXCLUDED.task_id,expected_version=EXCLUDED.expected_version,expected_content_revision=EXCLUDED.expected_content_revision,payload_bytes=EXCLUDED.payload_bytes,updated_at_ms=EXCLUDED.updated_at_ms,graph_service=EXCLUDED.graph_service RETURNING *")
        .bind(r.report.report_id).bind(r.report.binding.attempt_id).bind(ACTOR).bind(evidence).bind(op)
        .bind(task.workspace_id).bind(task.task_id)
        .bind(i64::try_from(task.version).map_err(|_| conflict())?)
        .bind(i64::try_from(task.content_revision).map_err(|_| conflict())?)
        .bind(&payload).bind(time).bind(graph)
        .fetch_one(&mut **tx).await.map_err(db)
}

fn graph_identity(rt: &crate::agent_access::Runtime, workspace: Uuid) -> Result<Value> {
    let identity = crate::agent_access::settlement_service_identity(rt, workspace)?;
    Ok(json!({"origin": identity["origin"], "actor": identity["actor"]}))
}

/// Persist the next check before crossing the network. After any possible
/// write, first read the exact receipt. Only an authorized absence can
/// schedule a repeat of the identical request.
async fn dispatch(
    state: &AppState,
    mut tx: Transaction<'_, Postgres>,
    row: &sqlx::postgres::PgRow,
    result: &results::RecordedResult,
) -> Result<AutoSettlement> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let time = now(state)?;
    let report_id: Uuid = row.get("report_id");
    let workspace: Uuid = row.get("workspace_id");
    let payload: Vec<u8> = row.get("payload_bytes");
    let first_send = row.get::<i32, _>("dispatch_count") == 0;
    let pinned: Option<Value> = row.get("graph_service");
    let matches_actor = graph_identity(rt, workspace)
        .is_ok_and(|identity| pinned.is_none_or(|pin| pin == identity));
    let (settlement, check) = graph_completion::schedule(&mut tx, row, time, matches_actor)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    let Some(next) = check else {
        return Ok(settlement);
    };
    let payload = payload.as_slice();
    let request = |lookup| async move {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            crate::agent_access::settlement_graph_request(rt, workspace, payload.to_vec(), lookup),
        )
        .await
    };
    let mut lookup = !first_send;
    let mut outcome = request(lookup).await;
    if !first_send && matches!(&outcome, Ok(Ok((code, raw))) if code.is_success() && raw.is_null())
    {
        let mut tx = begin(state).await?;
        let permitted = graph_completion::authorize_resend(&mut tx, report_id, next)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        if permitted {
            lookup = false;
            outcome = request(lookup).await;
        }
    }
    let mut tx = begin(state).await?;
    // The result admission path also takes these locks before the settlement row.
    authority::load_locked(
        state,
        &mut tx,
        &result.report.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    let current = graph_completion::lock_row(&mut tx, report_id)
        .await
        .map_err(db)?
        .ok_or(crate::error::AppError::Internal)?;
    if current.get::<String, _>("disposition") != "possible"
        || current.get::<Option<i64>, _>("next_graph_check_at_ms") != Some(next)
    {
        let s = status(&current)?;
        tx.commit().await.map_err(db)?;
        return Ok(s);
    }
    let time = now(state)?;
    match outcome {
        Ok(Ok((http, raw))) if http.is_success() => {
            confirm_dispatch_locked(&mut tx, report_id, &current, &raw, time).await?;
        }
        Ok(Ok((http, _))) if http == axum::http::StatusCode::CONFLICT => {
            let reason = if lookup {
                RECEIPT_CONFLICT
            } else {
                VERSION_CONFLICT
            };
            graph_completion::hold(&mut tx, report_id, time, reason)
                .await
                .map_err(db)?;
        }
        Ok(Ok((http, _))) if matches!(http.as_u16(), 401 | 403) => {
            sqlx::query("UPDATE profiled_auto_settlements SET reason=$2 WHERE report_id=$1")
                .bind(report_id)
                .bind(GRAPH_PERMISSION)
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        // Lost or invalid response: stays possible for identical replay.
        _ => {}
    }
    let row = graph_completion::lock_row(&mut tx, report_id)
        .await
        .map_err(db)?
        .ok_or(crate::error::AppError::Internal)?;
    let s = status(&row)?;
    tx.commit().await.map_err(db)?;
    Ok(s)
}

async fn confirm_dispatch_locked(
    tx: &mut Transaction<'_, Postgres>,
    report_id: Uuid,
    current: &sqlx::postgres::PgRow,
    raw: &Value,
    time: i64,
) -> Result<()> {
    if let Ok(receipt) = completion::confirmed(raw, current) {
        let receipt =
            serde_json::to_value(receipt).map_err(|_| crate::error::AppError::Internal)?;
        graph_completion::confirm(tx, report_id, current.get("attempt_id"), time, receipt)
            .await
            .map_err(db)?;
    }
    Ok(())
}

/// Epic 0 (spec 0016 section 6.2): the locked prelude of a playbook's
/// decision on `report_id`, as `settle_after_record`'s, with the existing
/// settlement row and whether the fences hold. A playbook decides only on the
/// attempt's current report head, while the attempt still holds the task,
/// after its proven stop, and when it was neither revoked nor cancelled.
async fn playbook_locked<'a>(
    state: &'a AppState,
    report_id: Uuid,
) -> Result<(
    Transaction<'a, Postgres>,
    results::RecordedResult,
    Option<sqlx::postgres::PgRow>,
    bool,
)> {
    let mut tx = begin(state).await?;
    let r = results::load_locked(state, &mut tx, report_id).await?;
    if !super::super::has_delivery_grant(&mut *tx, r.report.binding.attempt_id).await? {
        return Err(conflict());
    }
    aq_profiled_state::result_reads::lock_settlement(&mut tx, report_id)
        .await
        .map_err(db)?;
    let existing = graph_completion::lock_row(&mut tx, report_id)
        .await
        .map_err(db)?;
    let v = authority::load_locked(
        state,
        &mut tx,
        &r.report.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    let hot = aq_profiled_state::result_reads::lock_hot_state(&mut tx, r.report.binding.attempt_id)
        .await
        .map_err(db)?;
    let fenced = r.is_current_report_head
        && !r.report.platform_interrupted()
        && v.holds_task
        && v.stopped
        && hot.stop.known_stopped()
        && !v.revoked
        && v.cancellation_id.is_none();
    Ok((tx, r, existing, fenced))
}

/// Catalog 2.3.0 `work.complete`: completes the task with `report_id` through
/// `prepare_locked` / `dispatch` behind [`playbook_locked`]'s fences; the
/// claim is released only on a validated Graph receipt. Idempotent per
/// report: a confirmed result is `Completed` again, a dispatched one replays
/// the same payload, and a Graph version conflict is never dispatched again.
pub async fn complete_by_playbook(state: &AppState, report_id: Uuid) -> Result<Settled> {
    let settled = |s: &AutoSettlement| match s.disposition {
        AutoDisposition::Confirmed => Settled::Completed,
        AutoDisposition::Prepared | AutoDisposition::Possible => Settled::Pending,
        AutoDisposition::Held | AutoDisposition::ReworkRequired => Settled::Stale,
    };
    let (mut tx, r, existing, fenced) = playbook_locked(state, report_id).await?;
    let fenced = fenced
        && !crate::task_workspace_resume::checkpoint_required(&mut tx, r.report.binding.attempt_id)
            .await?;
    let prior = existing.as_ref().map(status).transpose()?;
    match prior {
        Some(s)
            if s.disposition == AutoDisposition::Possible
                || (s.disposition == AutoDisposition::Prepared && fenced) =>
        {
            return Ok(settled(
                &dispatch(state, tx, existing.as_ref().ok_or_else(unavailable)?, &r).await?,
            ));
        }
        Some(s)
            if s.disposition != AutoDisposition::Held
                || s.reason.as_deref().is_some_and(terminal_conflict) =>
        {
            tx.commit().await.map_err(db)?;
            return Ok(settled(&s));
        }
        _ if !fenced => {
            tx.commit().await.map_err(db)?;
            return Ok(Settled::Stale);
        }
        _ => {}
    }
    if !crate::task_workspace_delivery::gate(state, &mut tx, &r, false).await? {
        return Ok(Settled::Pending);
    }
    let evidence = json!({"policy_actor": ACTOR, "decided_by": "playbook", "decision": "work.complete", "report": r.receipt_artifact});
    let row = prepare_locked(state, &mut tx, &r, evidence, now(state)?, "Playbook").await?;
    Ok(settled(&dispatch(state, tx, &row, &r).await?))
}

/// Catalog 2.3.0 `work.return`: refuses `report_id` for rework with the
/// playbook's `reason` code behind [`playbook_locked`]'s fences, then records
/// the replacement facts ([`record_replacement_facts`]) so that the
/// replacement owner admits a new attempt. Idempotent per report: a result
/// already refused is `Returned` again (and its facts are recorded again); a
/// completed or completing one is `Stale`.
pub async fn return_by_playbook(
    state: &AppState,
    report_id: Uuid,
    reason: &str,
) -> Result<Settled> {
    for delay_ms in [100, 250, 500] {
        match return_once(state, report_id, reason).await {
            // The contended transaction has rolled back. Reacquire the locks
            // and check the same report and current authority again.
            Err(crate::error::AppError::Busy) => {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            outcome => return outcome,
        }
    }
    return_once(state, report_id, reason).await
}

async fn return_once(state: &AppState, report_id: Uuid, reason: &str) -> Result<Settled> {
    let (mut tx, r, existing, fenced) = playbook_locked(state, report_id).await?;
    let existing = existing.as_ref().map(status).transpose()?;
    let held = existing
        .as_ref()
        .is_none_or(|s| s.disposition == AutoDisposition::Held);
    let reworked = existing.is_some_and(|s| s.disposition == AutoDisposition::ReworkRequired);
    if !reworked && (!held || !fenced) {
        tx.commit().await.map_err(db)?;
        return Ok(Settled::Stale);
    }
    if !reworked {
        let evidence = json!({"policy_actor": ACTOR, "decided_by": "playbook", "decision": "work.return", "reason": reason, "report": r.receipt_artifact});
        sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,reason,evidence,created_at_ms,updated_at_ms) VALUES($1,$2,$3,'rework_required',$4,$5,$6,$6) ON CONFLICT(report_id) DO UPDATE SET disposition=EXCLUDED.disposition,reason=EXCLUDED.reason,evidence=EXCLUDED.evidence,updated_at_ms=EXCLUDED.updated_at_ms")
            .bind(report_id).bind(r.report.binding.attempt_id).bind(ACTOR).bind(reason).bind(evidence).bind(now(state)?)
            .execute(&mut *tx).await.map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    // Before the replacement facts, so that the next attempt's startup reads it.
    let thread = aq_task_comments::Thread {
        project_id: r.spec.lifecycle.project_id,
        task_id: r.spec.task.task_id,
    };
    post_failed_checks(state, report_id, thread).await;
    Box::pin(replacement_facts_after_rework(state, report_id)).await;
    Ok(Settled::Returned)
}

/// The system comment naming what the result's failed required checks
/// showed, once per result. Best effort: it never fails the return.
#[cfg(feature = "delivery_playbook")]
async fn post_failed_checks(state: &AppState, report_id: Uuid, thread: aq_task_comments::Thread) {
    let pool = &state.store.pool;
    let max = aq_task_comments::MAX_BODY_BYTES;
    // A bounded read (bridge timeouts), so that a slow database cannot hold the return.
    let body = async {
        let mut tx = aq_playbook_domain::bridge_tx(pool).await?;
        aq_playbook_domain::delivery_store::failed_checks_comment(&mut tx, report_id, max).await
    };
    let body = match body.await {
        Ok(Some(body)) => body,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%report_id, ?error, "failed checks comment not read");
            return;
        }
    };
    let author = aq_task_comments::Author::System {
        component: "aq.swarm".into(),
    };
    let id = Uuid::new_v5(
        &thread.task_id,
        format!("aq.checks-failed/{report_id}").as_bytes(),
    );
    match aq_task_comments::add(pool, id, thread, &author, &body, (state.clock)()).await {
        Ok(_) | Err(aq_task_comments::Error::IdConflict) => {}
        Err(error) => tracing::warn!(%report_id, ?error, "failed checks comment not posted"),
    }
}

#[cfg(test)]
mod outcome_tests {
    use aq_admission::transport::ProcessOutcome;
    #[test]
    fn only_a_success_outcome_is_acceptable() {
        assert!(super::outcome_acceptable(ProcessOutcome::Success));
        for refused in [
            ProcessOutcome::Unknown,
            ProcessOutcome::Failed,
            ProcessOutcome::Expired,
            ProcessOutcome::Cancelled,
        ] {
            assert!(!super::outcome_acceptable(refused));
        }
    }
}
