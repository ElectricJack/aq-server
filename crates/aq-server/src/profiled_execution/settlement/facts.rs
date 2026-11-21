//! The server-owned facts of one recorded result: the attempt's fences, the
//! central source receipt it pins (changed against the lineage's base?), the
//! check and delivery evidence. The D3 policy evaluates legacy reports. For
//! attempts with a Git grant the server stores facts for the delivery playbook.
use super::owner::*;
use crate::{AppState, execution::authority, profiled_execution::results};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(super) use aq_profiled_state::result_facts::{CheckVerdict, DeliveryState};

/// What is known of a recorded result under the report/attempt locks.
pub(super) struct Gathered {
    /// The attempt's fences: stopped, cancelled, revoked, holds the task.
    pub view: authority::AuthorityView,
    /// The pinned central receipt: (import operation, checkpoint, changed).
    pub source: Option<(Uuid, String, bool)>,
    pub lineage_unknown: bool,
    /// One verdict per offered `required_checks` entry, and as evidence.
    pub checks: Vec<CheckVerdict>,
    pub check_evidence: Vec<Value>,
    pub delivery_state: DeliveryState,
}

/// Gathers only server-owned facts. The caller holds the report lock.
pub(super) async fn gather_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    enrolled: bool,
) -> Result<Gathered> {
    let view = authority::load_locked(
        state,
        tx,
        &r.report.binding.session,
        authority::Policy::IdentityOnly,
    )
    .await?;
    // The exact retained source is the central receipt pinned by this report.
    let (mut source, mut lineage_unknown) = (None, false);
    // Spec 0016 §5.1: the source is the task ref, confirmed at the reported
    // head when the report was recorded; the report id keys its evidence.
    if enrolled
        && let Some(pushed) = aq_delivery::git_grant::pushed_of(&mut **tx, r.report.report_id)
            .await
            .map_err(db)?
    {
        let start = &r.spec.source.resolved_object.hex;
        let base = aq_machine_control::lineage_base(tx, r.spec.task.task_id, start).await?;
        lineage_unknown = base.is_none();
        let changed = base.is_none_or(|base| pushed.head != base);
        source = Some((r.report.report_id, pushed.head, changed));
    }
    if !enrolled {
        for op in &r.report.source_receipt_ids {
            match crate::source_retention::load_confirmed_locked(tx, *op, r).await {
                Ok(confirmed) => {
                    // Review C-1: judged against the lineage's original base, never a handoff
                    // successor's own starting commit (its predecessor's saved edits).
                    let m = &confirmed.manifest;
                    let task = r.spec.task.task_id;
                    let base = aq_machine_control::lineage_base(tx, task, &m.source.base.hex);
                    // R1-M1, R2-M1: past the hop bound the base is unknown (never guessed);
                    // the D3 gate holds only a report that would otherwise succeed.
                    let base = base.await?;
                    lineage_unknown = base.is_none();
                    let changed = base.is_none_or(|base| m.checkpoint.hex != base);
                    source = Some((*op, m.checkpoint.hex.clone(), changed));
                    break;
                }
                Err(crate::error::AppError::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    let attempt = r.report.binding.attempt_id;
    let mut checks = Vec::with_capacity(r.outcome.required_checks.len());
    let mut check_evidence = Vec::with_capacity(r.outcome.required_checks.len());
    for check in &r.outcome.required_checks {
        let verdict = match &source {
            None => CheckVerdict::Missing,
            Some((op, checkpoint, _)) => {
                let found: Option<String> = sqlx::query_scalar("SELECT verdict FROM profiled_check_evidence WHERE attempt_id=$1 AND check_id=$2 AND source_operation_id=$3 AND checkpoint=$4 AND validator_digest=$5")
                    .bind(attempt).bind(check.id.as_str()).bind(op).bind(checkpoint).bind(check.validator.digest.as_str())
                    .fetch_optional(&mut **tx).await.map_err(db)?;
                match found.as_deref() {
                    Some("passed") => CheckVerdict::Passed,
                    Some("failed") => CheckVerdict::Failed,
                    Some(_) => return Err(crate::error::AppError::Internal),
                    None => CheckVerdict::Missing,
                }
            }
        };
        checks.push(verdict);
        check_evidence.push(json!({"id":check.id.as_str(),"validator_digest":check.validator.digest.as_str(),"verdict":verdict}));
    }
    let delivery_state = match &source {
        None => DeliveryState::Missing,
        // An unchanged source has nothing to publish (owner decision 2026-11-13).
        Some((_, _, false)) => DeliveryState::NoChange,
        Some((op, _, true)) => {
            let found: Option<String> = sqlx::query_scalar(
                "SELECT state FROM profiled_delivery_evidence WHERE attempt_id=$1 AND source_operation_id=$2",
            )
            .bind(attempt)
            .bind(op)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
            match found.as_deref() {
                None => DeliveryState::Missing,
                Some("published") => DeliveryState::Published,
                Some("unpublished") => DeliveryState::Unpublished,
                Some("indeterminate") => DeliveryState::Indeterminate,
                Some(_) => return Err(crate::error::AppError::Internal),
            }
        }
    };
    Ok(Gathered {
        view,
        source,
        lineage_unknown,
        checks,
        check_evidence,
        delivery_state,
    })
}

impl Gathered {
    /// The attempt's fences as evidence (the shape the D3 evidence records).
    pub(super) fn accepted(&self, r: &results::RecordedResult) -> Value {
        let v = &self.view;
        json!({"current_report_head": r.is_current_report_head, "stopped": v.stopped, "cancelled": v.cancellation_id.is_some(), "revoked": v.revoked, "holds_task": v.holds_task})
    }
    /// The retained source as evidence, or null.
    pub(super) fn source_evidence(&self) -> Value {
        match &self.source {
            Some((op, checkpoint, changed)) => {
                json!({"operation_id": op, "checkpoint": checkpoint, "changed": changed})
            }
            None => Value::Null,
        }
    }
}

/// Spec 0016 section 7: the result path's settlement step. Under the report
/// lock it stores what [`gather_locked`] reads as the result's facts (replaced
/// on a duplicate record) and returns them. No settlement row, publication,
/// rework or completion: the result waits for a playbook's decision.
pub async fn record_facts(state: &AppState, report_id: Uuid) -> Result<Value> {
    let mut tx = super::automatic::begin(state).await?;
    let r = results::load_locked(state, &mut tx, report_id).await?;
    // The same per-report lock as a playbook's decision on this result.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report_id}"))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if !super::super::has_delivery_grant(&mut *tx, r.report.binding.attempt_id).await? {
        return Err(conflict());
    }
    let g = gather_locked(state, &mut tx, &r, true).await?;
    // H5: a parked predecessor is suspended, its report neither success nor failure.
    let suspended =
        crate::hibernation::suspended_hold(&mut tx, r.report.binding.attempt_id).await?;
    let suspended = suspended
        .map(|(reason, evidence)| json!({"reason": reason, "suspension": evidence["suspension"]}));
    let facts = json!({
        "suspended": suspended,
        "report": r.receipt_artifact,
        "outcome": r.report.outcome,
        "accepted": g.accepted(&r),
        "source": g.source_evidence(),
        "lineage_unknown": g.lineage_unknown,
        "checks": g.check_evidence,
        "delivery": {"state": g.delivery_state},
    });
    let (task, time) = (r.spec.task.task_id, now(state)?);
    sqlx::query("INSERT INTO delivery_result_facts(report_id,attempt_id,task_id,facts,recorded_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,$5) ON CONFLICT(report_id) DO UPDATE SET facts=EXCLUDED.facts,updated_at_ms=GREATEST(delivery_result_facts.updated_at_ms,EXCLUDED.updated_at_ms)")
        .bind(report_id)
        .bind(r.report.binding.attempt_id)
        .bind(task)
        .bind(&facts)
        .bind(time)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(facts)
}
