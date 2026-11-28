//! Task work readiness for a Home merge that waits on workspace evidence.
use super::*;
use serde_json::{Value, json};

pub(super) const CODE: &str = "workspace_integrated_checks_required";

/// While delivery's `pull_request.merge` for the holding attempt's current
/// result waits on the owner's integrated-checks attestation, say so instead
/// of leaving a silently running operation. The read is optional: when it
/// cannot be computed the task read still succeeds without it.
pub(crate) async fn merge_wait(
    state: &AppState,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Option<Value> {
    match waiting(state, project, task, attempt).await {
        Ok(reason) => reason,
        Err(error) => {
            tracing::warn!(%task, ?error, "workspace merge readiness omitted");
            None
        }
    }
}

async fn waiting(
    state: &AppState,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Result<Option<Value>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    if !has_scope(state, &mut tx, attempt, project, task).await? {
        return Ok(None);
    }
    let merge: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT o.operation_id,h.report_id FROM profiled_result_heads h
         JOIN execution_attempts a USING(attempt_id)
         JOIN playbook_host_operations o ON o.subject_id=a.task_id::text
         WHERE h.attempt_id=$1 AND a.task_id=$2 AND a.holds_task
           AND o.command_name='pull_request.merge' AND o.status='running'
           AND o.request#>>'{result_ready,result_id}'=h.report_id::text
           AND o.request#>>'{result_ready,attempt_id}'=h.attempt_id::text
         ORDER BY o.created_at_ms DESC,o.operation_id LIMIT 1",
    )
    .bind(attempt)
    .bind(task)
    .fetch_optional(&mut *tx)
    .await
    .map_err(crate::error::db)?;
    let Some((operation, report)) = merge else {
        return Ok(None);
    };
    let result = crate::profiled_execution::results::load_locked(state, &mut tx, report).await?;
    if !result.is_current_report_head || result.spec.lifecycle.project_id != project {
        return Ok(None);
    }
    let Some(plan) = load(state, &mut tx, &result).await? else {
        return Ok(None);
    };
    let reason = reason(&mut tx, &plan, (attempt, report, operation)).await?;
    tx.rollback().await.map_err(crate::error::db)?;
    Ok(reason)
}

/// The reason for a complete plan whose changed siblings have no passing
/// integrated check at the exact checkout set.
pub(super) async fn reason(
    db: &mut PgConnection,
    plan: &Plan,
    (attempt, report, operation): (Uuid, Uuid, Uuid),
) -> Result<Option<Value>, AppError> {
    if !plan.complete() || integrated(db, plan).await?.is_some() {
        return Ok(None);
    }
    let route = format!(
        "/api/projects/{}/tasks/{}/results/{report}/workspace-delivery",
        plan.home, plan.task
    );
    let message = format!(
        "Delivery's pull_request.merge for result {report} waits for the integrated_checks attestation: \
         a sibling workspace changed, so checks must run on the exact checkout set. The human owner \
         runs them and records the outcome with action integrated_checks on POST {route}."
    );
    Ok(Some(json!({
        "code": CODE,
        "attempt_id": attempt,
        "result_id": report,
        "operation_id": operation,
        "missing_attestation": "integrated_checks",
        "owner_action": route,
        "blocker_owner": "installation_owner",
        "wake_condition": "workspace_integrated_checks_recorded",
        "message": message,
    })))
}
