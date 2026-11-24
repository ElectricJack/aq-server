#[cfg(feature = "delivery_playbook")]
use crate::delivery;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;
/// Holding attempt (the agent session attached to each task) for a page of
/// tasks in this project. At most one attempt holds a task.
pub async fn holding_executions(
    pool: &PgPool,
    project: Uuid,
    tasks: &[Uuid],
) -> Result<Vec<Value>, sqlx::Error> {
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT e.task_id,e.attempt_id,e.session_id,COALESCE(h.state->>'status',e.state->>'status','unknown') AS status \
         FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=ANY($1) AND e.kind='profiled_v1' AND e.holds_task \
         AND e.state->'offer'->'manifest'->'bindings'->>'project_id'=$2 \
         ORDER BY e.task_id,e.attempt_id LIMIT 65",
    )
    .bind(tasks)
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    // Spec 0014 §7.1: what each attempt ran with, from its own record.
    let ran_with = aq_role_settings::attempts::ran_with_rows(pool, &rows).await;
    rows.iter()
        .map(|row| {
            let attempt: Uuid = row.try_get("attempt_id")?;
            let mut execution = json!({
                "task_id": row.try_get::<Uuid, _>("task_id")?,
                "attempt_id": attempt,
                "session_id": row.try_get::<Uuid, _>("session_id")?,
                "status": row.try_get::<String, _>("status")?,
            });
            if let Some(summary) = ran_with.get(&attempt) {
                execution["ran_with"] = json!(summary);
            }
            Ok(execution)
        })
        .collect()
}

/// The hold of the attempt named by an execution projection's `attempt_id`
/// (the agent session attached to a task), read from its authority state;
/// None when there is no such attempt.
pub async fn attempt_hold(
    pool: &sqlx::PgPool,
    attempt_id: &Value,
) -> Result<Option<Hold>, sqlx::Error> {
    let Some(attempt) = attempt_id.as_str().and_then(|a| a.parse().ok()) else {
        return Ok(None);
    };
    let state = sqlx::query_scalar::<_, Value>("SELECT COALESCE(h.state,e.state) FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) WHERE e.attempt_id=$1")
        .bind(attempt).fetch_optional(pool).await?;
    Ok(state.map(|state| Hold::from_authority(attempt, &state)))
}

/// Whether the latest confirmed default-policy settlement of `task` found no
/// file change (owner ruling 4b).
pub async fn completed_without_changes(pool: &sqlx::PgPool, task: Uuid) -> bool {
    let quiet = sqlx::query_scalar::<_, Option<String>>(
        "SELECT s.evidence#>>'{delivery,state}' FROM profiled_auto_settlements s JOIN execution_attempts e USING(attempt_id) WHERE e.task_id=$1 AND s.disposition='confirmed' ORDER BY s.updated_at_ms DESC LIMIT 1",
    );
    matches!(quiet.bind(task).fetch_optional(pool).await, Ok(Some(Some(s))) if s == "no_change")
}

/// Add the settled delivery outcome to a completed task's work view.
pub async fn show_no_change_settlement(data: &mut Value, pool: &PgPool, task: Uuid) {
    if data["task_state"] == "completed" && completed_without_changes(pool, task).await {
        data["completed_without_changes"] = json!(true);
    }
}

/// The attempt holding a task, from AQ's execution authority state (the hot
/// authority for adopted native attempts, else the attempt's own state).
pub struct Hold {
    pub attempt_id: Uuid,
    /// `offered`, `preparing`, `granted`, `cancelling`, `finished`, `declined`,
    /// `unknown` or `never_granted`.
    pub status: String,
    /// A B3 cancellation is recorded for the attempt.
    pub cancelled: bool,
    /// The stop proof is a Stopped or NotStarted receipt.
    pub stopped: bool,
}
impl Hold {
    pub fn from_authority(attempt_id: Uuid, state: &Value) -> Self {
        Self {
            attempt_id,
            status: state["status"].as_str().unwrap_or("unknown").to_owned(),
            cancelled: !state["cancellation_id"].is_null(),
            stopped: matches!(
                state["stop"]["kind"].as_str(),
                Some("stopped" | "not_started")
            ),
        }
    }
    /// Still executing, or not provably stopped, while it holds the task.
    pub fn running(&self) -> bool {
        !self.stopped
            && !matches!(
                self.status.as_str(),
                "finished" | "declined" | "never_granted"
            )
    }
    /// Why a task whose attempt no longer runs is still held; None while running.
    pub fn reason(&self) -> Option<Value> {
        if self.running() {
            return None;
        }
        let (code, message) = if !self.stopped {
            (
                "attempt_finished_stop_unconfirmed",
                "The attempt has finished but its stop is not proven yet, so it still holds the task.",
            )
        } else if self.cancelled {
            (
                "cancelled_attempt_holds_task",
                "The attempt was cancelled and has stopped, but it still holds the task: a cancelled attempt keeps its retained source for a replacement (B4). The claim is released when the task is retried or the owner abandons the attempt.",
            )
        } else {
            (
                "attempt_stopped_not_released",
                "The attempt has finished and stopped, but it still holds the task until its result is settled, the task is retried or the owner abandons the attempt.",
            )
        };
        Some(
            json!({"code":code,"attempt_id":self.attempt_id,"status":self.status,"message":message}),
        )
    }
}

/// Format the current scheduling hold and any recorded result for that same attempt.
pub async fn recorded_hold(
    _pool: &PgPool,
    _project: Uuid,
    _task: Uuid,
    current: Option<&aq_machine_control::holds::Current>,
) -> Option<Value> {
    let h = current?;
    let phrase = aq_machine_control::holds::phrase(&h.reason);
    let shown = task_hold(h.attempt_id, &h.status, &h.reason, &phrase);
    #[cfg(feature = "delivery_playbook")]
    if aq_machine_control::holds::is_budget_hold(&h.reason)
        && let Some(evidence) =
            delivery::budget_evidence(_pool, _project, _task, h.attempt_id).await
    {
        let mut shown = shown;
        if let Some(Value::String(message)) = shown.get_mut("message") {
            message.push(' ');
            message.push_str(&evidence);
        }
        return Some(shown);
    }
    Some(shown)
}

/// A server-side hold of the task itself (R6): AQ does not offer it, for `code` (a server reason
/// code, `phrase` its fixed description), about `attempt_id` (with its authority `status`).
pub fn task_hold(attempt_id: Uuid, status: &str, code: &str, phrase: &str) -> Value {
    let message = format!("AQ does not offer this task: {phrase} (hold reason: {code}).");
    let mut reason = json!({"code":"task_held","attempt_id":attempt_id,"status":status,"message":message,"hold_reason":code});
    if aq_machine_control::holds::is_budget_hold(code) {
        reason["blocker_owner"] = json!("project_owner");
        reason["wake_condition"] = json!("owner_retry_authorized");
        reason["message"] = json!(format!(
            "{message} The project owner can authorize a retry."
        ));
    }
    reason
}

/// A delivery hold has a result and reason even after the playbook has ended.
#[cfg(feature = "delivery_playbook")]
pub fn delivery_hold(hold: &Hold, result: Uuid, reason: &str, evidence: Option<&str>) -> Value {
    let why = match reason {
        "returns_exhausted" => "the delivery playbook reached its repair-return limit",
        "publish_conflict" => "the forge branch could not be published under the recorded lease",
        _ => "the delivery playbook held the recorded result",
    };
    let mut shown = task_hold(
        hold.attempt_id,
        &hold.status,
        reason,
        &format!("{why} for result {result}"),
    );
    shown["result_id"] = json!(result);
    if reason == "returns_exhausted" {
        shown["blocker_owner"] = json!("project_owner");
        shown["wake_condition"] = json!("owner_retry_authorized");
        shown["message"] = json!(format!(
            "Delivery reached its repair-return limit for result {result}. The project owner can authorize a retry."
        ));
    } else if reason == "publish_conflict" {
        shown["blocker_owner"] = json!("repository_maintainer");
        shown["wake_condition"] = json!("forge_branch_matches_result_or_publish_lease");
        shown["message"] = json!(format!(
            "Delivery is holding result {result} after a branch publication conflict. AQ rechecks the forge branch on its delivery interval and resumes when the result is present or the lease is usable."
        ));
    }
    if let Some(evidence) = evidence {
        shown["message"] = json!(format!(
            "{} {evidence}",
            shown["message"].as_str().unwrap_or_default()
        ));
    }
    shown
}

/// Epic 0 (spec 0016 section 7): the task read's word on a stopped attempt
/// that still holds its task with a recorded result no playbook decided:
/// while no delivery playbook is active, the result waits and nothing else
/// completes, publishes or reworks it. `delivery_policy` says whether a
/// delivery playbook is active for the task's project.
#[cfg(feature = "delivery_playbook")]
pub fn result_waiting(attempt: Uuid, result: Uuid, delivery_policy: bool) -> Value {
    let (code, owner, wake, message) = match delivery_policy {
        false => (
            "result_ready_no_delivery_policy",
            "delivery_policy_owner",
            "delivery_policy_activated",
            "Result ready, no delivery policy is active. AQ will recheck the result after the owner activates the project's delivery playbook.",
        ),
        true => (
            "result_ready",
            "aq_delivery",
            "delivery_reconciliation",
            "Result ready: the attempt has stopped with a result, which waits for the delivery playbook's decision.",
        ),
    };
    json!({"code": code, "attempt_id": attempt, "result_id": result, "message": message,
        "blocker_owner": owner, "wake_condition": wake})
}

/// Recovery evidence for the exact attempt that still holds the task.
pub async fn attempt_recovery_wait(pool: &PgPool, h: &Hold) -> Option<Value> {
    if !h.stopped && h.status != "cancelling" {
        return None;
    }
    let (stop, closure, has_result): (Option<String>, Option<Vec<u8>>, bool) = sqlx::query_as(
        "SELECT a.state#>>'{stop,kind}', c.request_bytes,
            EXISTS(SELECT 1 FROM profiled_result_reports r WHERE r.attempt_id=e.attempt_id)
         FROM execution_attempts e JOIN profiled_authority_hot a USING(attempt_id)
         LEFT JOIN profiled_launch_closures c USING(attempt_id)
         WHERE e.attempt_id=$1 AND e.holds_task",
    )
    .bind(h.attempt_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let proof = closure.and_then(|bytes| {
        serde_json::from_slice::<aq_admission::granted_closure::Request>(&bytes)
            .ok()
            .filter(|request| request.binding.attempt_id == h.attempt_id)
            .map(|request| request.evidence.proves_never_started())
    });
    recovery_reason(h.attempt_id, stop.as_deref(), proof, has_result)
}

pub(crate) fn recovery_reason(
    attempt: Uuid,
    stop: Option<&str>,
    proof: Option<bool>,
    has_result: bool,
) -> Option<Value> {
    let (code, owner, wake, message) = match (stop, proof, has_result) {
        (Some("unknown"), Some(false), _) => (
            "closure_does_not_prove_nonstart",
            "aq_operator",
            "original_launch_evidence_reconciled",
            "The host stopped and the harness may have run. The AQ operator needs to reconcile the original attempt's result and effects before its claim can be released.",
        ),
        (Some("unknown" | "not_started"), Some(true), _) | (Some("not_started"), None, _) => (
            "closed_launch_settlement_pending",
            "aq_scheduler",
            "closed_launch_settlement",
            "AQ will check the original launch closure, retained source, and effects before freeing the claim.",
        ),
        (Some("stopped"), _, false) => (
            "stopped_result_recovery_pending",
            "aq_worker",
            "stopped_result_reconciled",
            "The attempt stopped without a recorded result. The worker needs to reconcile the retained workspace and the stopped result.",
        ),
        _ => return None,
    };
    Some(
        json!({"code":code,"attempt_id":attempt,"blocker_owner":owner,"wake_condition":wake,"message":message}),
    )
}

/// The original Graph operation stays visible while AQ verifies its receipt.
pub async fn graph_completion_wait(pool: &PgPool, hold: &Hold) -> Option<Value> {
    let (result, operation, reason, next): (Uuid, Uuid, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT report_id,operation_id,reason,next_graph_check_at_ms FROM profiled_auto_settlements \
         WHERE attempt_id=$1 AND (disposition IN ('prepared','possible') \
           OR (disposition='held' AND reason IN ('graph_version_conflict','graph_completion_receipt_conflict'))) \
         ORDER BY created_at_ms DESC,report_id LIMIT 1",
    )
    .bind(hold.attempt_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    let (code, owner, wake, message) = match reason.as_deref() {
        Some("graph_completion_identity_required") => (
            "graph_completion_identity_required",
            "aq_operator",
            "original_graph_service_restored",
            "Restore the Graph origin and actor pinned to this completion. AQ will recheck the original operation.",
        ),
        Some("graph_completion_permission_required") => (
            "graph_completion_permission_required",
            "graph_workspace_admin",
            "graph_mutation_permission_restored",
            "Restore mutation permission for the configured Graph service actor. AQ will recheck the original completion operation.",
        ),
        Some("graph_version_conflict") => (
            "graph_version_conflict",
            "project_owner",
            "current_task_version_reconciled",
            "Graph refused this completion at the task version AQ originally admitted. Review the current task and result.",
        ),
        Some("graph_completion_receipt_conflict") => (
            "graph_completion_receipt_conflict",
            "graph_operator",
            "original_graph_receipt_reconciled",
            "Graph recorded a different request for this operation key. Reconcile the original Graph receipt.",
        ),
        _ => (
            "graph_completion_receipt_pending",
            "aq_graph_settlement",
            "scheduled_graph_receipt_check",
            "AQ will check Graph for the original completion receipt before resending the same request.",
        ),
    };
    Some(
        json!({"code":code,"attempt_id":hold.attempt_id,"result_id":result,"operation_id":operation,
        "message":message,"blocker_owner":owner,"wake_condition":wake,"next_check_at_ms":next}),
    )
}

/// The retry approval has a durable check date even if its request was interrupted.
pub async fn owner_retry_wait(pool: &PgPool, project: Uuid, task: Uuid) -> Option<Value> {
    let (operation, next, owner_available, predecessor, reason): (
        Uuid,
        i64,
        bool,
        Option<Uuid>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT i.operation_id,floor(extract(epoch FROM i.next_check_at)*1000)::bigint,
          COALESCE(u.role='owner',false),i.predecessor,
          CASE WHEN i.released_at IS NULL THEN to_jsonb(i)->>'wait_reason' END
         FROM owner_task_retry_intents i JOIN dashboard_operations d USING(operation_id)
         LEFT JOIN users u ON u.id=d.principal_id
         WHERE i.project_id=$1 AND i.task_id=$2 AND d.disposition IN ('pending','unknown')
         ORDER BY d.created_at DESC,i.operation_id DESC LIMIT 1",
    )
    .bind(project)
    .bind(task)
    .fetch_optional(pool)
    .await
    .ok()??;
    let (owner, wake, message) = match (owner_available, reason.as_deref()) {
        (false, _) => (
            "project_owner",
            "original_retry_authority_restored",
            "Restore owner permission for the principal who approved this retry. AQ will recheck the original approval.",
        ),
        (true, Some("replacement_prior_execution_uncertain")) => (
            "aq_worker",
            "prior_execution_verified",
            "AQ will recheck the approved retry once the original attempt's execution and stop evidence is verified.",
        ),
        (true, Some("replacement_effects_unreconciled")) => (
            "aq_effects",
            "original_effects_reconciled",
            "AQ will recheck the approved retry once the original attempt's effects have settled.",
        ),
        (true, Some("replacement_source_retention_unknown")) => (
            "aq_worker",
            "original_source_retained",
            "AQ will recheck the approved retry once it can verify retention of the original attempt's source.",
        ),
        (true, Some("replacement_lineage_history_unknown")) => (
            "aq_operator",
            "original_lineage_history_restored",
            "Restore the original attempt history. AQ will recheck the approved retry.",
        ),
        _ => (
            "aq_scheduler",
            "scheduled_owner_retry_check",
            "AQ will resume the original retry approval at its scheduled check.",
        ),
    };
    Some(
        json!({"code":"owner_retry_pending","operation_id":operation,"attempt_id":predecessor,
        "hold_reason":reason,"message":message,"blocker_owner":owner,
        "wake_condition":wake,"next_check_at_ms":next}),
    )
}

/// The scheduler releases a verified nonstart before waiting to reoffer.
pub async fn launch_retry_wait(pool: &PgPool, task: Uuid, now_ms: i64) -> Option<Value> {
    use aq_profiled_state::{replacement, replacement_sql};
    let now = u64::try_from(now_ms).ok()?;
    let mut conn = pool.acquire().await.ok()?;
    let priors = replacement_sql::load(&mut conn, task).await.ok()?;
    if priors.len() < 2 {
        return None;
    }
    let settings = aq_role_settings::selection_defaults(&mut conn).await.ok()?;
    let (attempt, next) = replacement::launch_retry_after(&priors, settings.launch_retry_backoff)?;
    (now < next).then(|| {
        json!({
            "code": replacement::LAUNCH_BACKOFF,
            "attempt_id": attempt,
            "message": "Repeated verified launch failures released the task's previous claim. AQ will recheck worker placement after the backoff.",
            "blocker_owner": "aq_scheduler",
            "wake_condition": "launch_retry_backoff_elapsed",
            "next_check_at_ms": next,
        })
    })
}
