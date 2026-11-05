//! D3 automatic settlement: a policy actor evaluates each recorded result under
//! the retained default completion policy. The AQ row is evidence, never task
//! state; aq-graph owns completion and only its validated receipt confirms it.
use super::{
    completion,
    default_policy::{self as policy, Acceptance, CheckVerdict, Delivery, DeliveryState, Verdict},
    owner::*,
};
use crate::{AppState, profiled_execution::results};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

const OWNER: &str = "aq.settlement-owner.auto.1";
const ACTOR: &str = "aq.settlement.default-policy@1";
const MAX_DISPATCH: i32 = 8;
const VERSION_CONFLICT: &str = "graph_version_conflict";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoDisposition {
    /// Not completed; waiting on evidence or a re-evaluation.
    Held,
    /// Refused with evidence; terminal for this report.
    ReworkRequired,
    Prepared,
    /// Dispatched; the Graph outcome is not yet known.
    Possible,
    /// A validated Graph receipt completed the task.
    Confirmed,
}
impl AutoDisposition {
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "held" => Self::Held,
            "rework_required" => Self::ReworkRequired,
            "prepared" => Self::Prepared,
            "possible" => Self::Possible,
            "confirmed" => Self::Confirmed,
            _ => return Err(crate::error::AppError::Internal),
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct AutoSettlement {
    pub report_id: Uuid,
    pub disposition: AutoDisposition,
    pub reason: Option<String>,
    pub operation_id: Option<Uuid>,
    pub dispatch_count: u32,
    pub evidence: Value,
}
fn status(row: &sqlx::postgres::PgRow) -> Result<AutoSettlement> {
    Ok(AutoSettlement {
        report_id: row.get("report_id"),
        disposition: AutoDisposition::parse(row.get("disposition"))?,
        reason: row.get("reason"),
        operation_id: row.get("operation_id"),
        dispatch_count: u32::try_from(row.get::<i32, _>("dispatch_count"))
            .map_err(|_| crate::error::AppError::Internal)?,
        evidence: row.get("evidence"),
    })
}
async fn begin(state: &AppState) -> Result<Transaction<'_, Postgres>> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2000ms',true),set_config('aq.settlement_owner',$1,true)")
        .bind(OWNER)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    Ok(tx)
}
async fn lock_row(
    tx: &mut Transaction<'_, Postgres>,
    report_id: Uuid,
) -> Result<Option<sqlx::postgres::PgRow>> {
    sqlx::query("SELECT * FROM profiled_auto_settlements WHERE report_id=$1 FOR UPDATE")
        .bind(report_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)
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
    let Some(delivery) = applicable(state, &r) else {
        return Ok(None);
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report_id}"))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let existing = lock_row(&mut tx, report_id).await?;
    let mut conflict_held = false;
    if let Some(row) = &existing {
        let s = status(row)?;
        match s.disposition {
            AutoDisposition::Confirmed | AutoDisposition::ReworkRequired => {
                tx.commit().await.map_err(db)?;
                return Ok(Some(s));
            }
            // Once possibly dispatched, only the SAME payload is replayed.
            AutoDisposition::Prepared | AutoDisposition::Possible => {
                if row.get::<i32, _>("dispatch_count") >= MAX_DISPATCH {
                    tx.commit().await.map_err(db)?;
                    return Ok(Some(s));
                }
                return dispatch(state, tx, report_id).await.map(Some);
            }
            AutoDisposition::Held => conflict_held = s.reason.as_deref() == Some(VERSION_CONFLICT),
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
    let time = now(state)?;
    let held = |reason: &'static str| ("held", reason);
    let (disposition, reason) = match verdict {
        Verdict::Rework(reason) => ("rework_required", reason),
        // A version conflict re-evaluates but never re-dispatches the stale
        // expected version; only a new report can be settled again.
        Verdict::Hold(_) | Verdict::Complete if conflict_held => held(VERSION_CONFLICT),
        Verdict::Hold(reason) => held(reason),
        Verdict::Complete => {
            prepare_locked(state, &mut tx, &r, evidence, time).await?;
            return dispatch(state, tx, report_id).await.map(Some);
        }
    };
    sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,reason,evidence,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$7) ON CONFLICT(report_id) DO UPDATE SET disposition=EXCLUDED.disposition,reason=EXCLUDED.reason,evidence=EXCLUDED.evidence,updated_at_ms=EXCLUDED.updated_at_ms")
        .bind(report_id).bind(r.report.binding.attempt_id).bind(ACTOR).bind(disposition).bind(reason).bind(evidence).bind(time)
        .execute(&mut *tx).await.map_err(db)?;
    let row = lock_row(&mut tx, report_id)
        .await?
        .ok_or(crate::error::AppError::Internal)?;
    let s = status(&row)?;
    tx.commit().await.map_err(db)?;
    Ok(Some(s))
}

/// Gathers only server-owned facts under the report/attempt locks.
async fn evaluate_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    delivery: Delivery,
) -> Result<(Verdict, Value)> {
    let v = crate::execution::authority::load_locked(
        state,
        tx,
        &r.report.binding.session,
        crate::execution::authority::Policy::IdentityOnly,
    )
    .await?;
    let acceptance = if !r.is_current_report_head
        || !matches!(
            r.report.outcome,
            aq_admission::transport::ProcessOutcome::Success
        )
        || v.revoked
        || v.cancellation_id.is_some()
        || !v.holds_task
    {
        Acceptance::Refused
    } else if !v.stopped {
        Acceptance::NotStopped
    } else {
        Acceptance::Accepted
    };
    // The exact retained source is the central receipt pinned by this report.
    let mut source = None;
    for op in &r.report.source_receipt_ids {
        match crate::source_retention::load_confirmed_locked(tx, *op, r).await {
            Ok(confirmed) => {
                source = Some((*op, confirmed.manifest.checkpoint.hex.clone()));
                break;
            }
            Err(crate::error::AppError::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
    }
    let attempt = r.report.binding.attempt_id;
    let mut checks = Vec::with_capacity(r.outcome.required_checks.len());
    let mut check_evidence = Vec::with_capacity(r.outcome.required_checks.len());
    for check in &r.outcome.required_checks {
        let verdict = match &source {
            None => CheckVerdict::Missing,
            Some((op, checkpoint)) => {
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
        Some((op, _)) => {
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
    let verdict = policy::evaluate(&policy::Gate {
        acceptance,
        source_retained: source.is_some(),
        checks: &checks,
        delivery,
        delivery_state,
    });
    let evidence = json!({
        "policy_actor": ACTOR,
        "policy": r.outcome.validation_policy,
        "report": r.receipt_artifact,
        "accepted": {"current_report_head": r.is_current_report_head, "stopped": v.stopped, "cancelled": v.cancellation_id.is_some(), "revoked": v.revoked, "holds_task": v.holds_task},
        "source": source.as_ref().map(|(op, checkpoint)| json!({"operation_id": op, "checkpoint": checkpoint})),
        "checks": check_evidence,
        "delivery": {"policy": delivery, "state": delivery_state},
    });
    Ok((verdict, evidence))
}

async fn prepare_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
    evidence: Value,
    time: i64,
) -> Result<()> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let task = &r.spec.task;
    crate::agent_access::settlement_service_identity(rt, task.workspace_id)?;
    let op = Uuid::new_v4();
    let payload = bytes(
        &json!({"idempotency_key":format!("aq-auto-settlement-v1:{op}"),"reason":format!("Default policy accepted retained result {}",r.report.report_id),"operations":[{"op":"update_node","id":task.task_id,"expected_version":task.version,"patch":{"task_state":"completed"}}]}),
    )?;
    if payload.len() > 8192 {
        return Err(unavailable());
    }
    sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,reason,evidence,operation_id,workspace_id,task_id,expected_version,expected_content_revision,payload_bytes,created_at_ms,updated_at_ms) VALUES($1,$2,$3,'prepared',NULL,$4,$5,$6,$7,$8,$9,$10,$11,$11) ON CONFLICT(report_id) DO UPDATE SET disposition='prepared',reason=NULL,evidence=EXCLUDED.evidence,operation_id=EXCLUDED.operation_id,workspace_id=EXCLUDED.workspace_id,task_id=EXCLUDED.task_id,expected_version=EXCLUDED.expected_version,expected_content_revision=EXCLUDED.expected_content_revision,payload_bytes=EXCLUDED.payload_bytes,updated_at_ms=EXCLUDED.updated_at_ms")
        .bind(r.report.report_id).bind(r.report.binding.attempt_id).bind(ACTOR).bind(evidence).bind(op)
        .bind(task.workspace_id).bind(task.task_id)
        .bind(i64::try_from(task.version).map_err(|_| conflict())?)
        .bind(i64::try_from(task.content_revision).map_err(|_| conflict())?)
        .bind(&payload).bind(time)
        .execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

/// Marks the row possible and commits BEFORE the Graph write, so a crash can
/// only replay the identical idempotent payload, never mint a second one.
async fn dispatch(
    state: &AppState,
    mut tx: Transaction<'_, Postgres>,
    report_id: Uuid,
) -> Result<AutoSettlement> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let time = now(state)?;
    let row = sqlx::query("UPDATE profiled_auto_settlements SET disposition='possible',dispatch_count=dispatch_count+1,updated_at_ms=$2 WHERE report_id=$1 AND disposition IN ('prepared','possible') RETURNING workspace_id,payload_bytes")
        .bind(report_id).bind(time).fetch_one(&mut *tx).await.map_err(db)?;
    let workspace: Uuid = row.get("workspace_id");
    let payload: Vec<u8> = row.get("payload_bytes");
    tx.commit().await.map_err(db)?;
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::agent_access::settlement_post(rt, workspace, payload),
    )
    .await;
    let mut tx = begin(state).await?;
    let current = lock_row(&mut tx, report_id)
        .await?
        .ok_or(crate::error::AppError::Internal)?;
    if current.get::<String, _>("disposition") != "possible" {
        let s = status(&current)?;
        tx.commit().await.map_err(db)?;
        return Ok(s);
    }
    let time = now(state)?;
    match outcome {
        Ok(Ok((http, raw))) if http.is_success() => {
            if let Ok(receipt) = completion::confirmed(&raw, &current) {
                let receipt =
                    serde_json::to_value(receipt).map_err(|_| crate::error::AppError::Internal)?;
                sqlx::query("UPDATE profiled_auto_settlements SET disposition='confirmed',reason=NULL,receipt=$2,updated_at_ms=$3 WHERE report_id=$1")
                    .bind(report_id).bind(receipt).bind(time).execute(&mut *tx).await.map_err(db)?;
                sqlx::query(
                    "UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1 AND holds_task",
                )
                .bind(current.get::<Uuid, _>("attempt_id"))
                .execute(&mut *tx)
                .await
                .map_err(db)?;
            }
        }
        Ok(Ok((http, _))) if http == axum::http::StatusCode::CONFLICT => {
            sqlx::query("UPDATE profiled_auto_settlements SET disposition='held',reason=$2,updated_at_ms=$3 WHERE report_id=$1")
                .bind(report_id).bind(VERSION_CONFLICT).bind(time).execute(&mut *tx).await.map_err(db)?;
        }
        // Lost or invalid response: stays possible for identical replay.
        _ => {}
    }
    let row = lock_row(&mut tx, report_id)
        .await?
        .ok_or(crate::error::AppError::Internal)?;
    let s = status(&row)?;
    tx.commit().await.map_err(db)?;
    Ok(s)
}
