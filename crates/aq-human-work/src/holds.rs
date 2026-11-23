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
    json!({"code":"task_held","attempt_id":attempt_id,"status":status,"message":message,"hold_reason":code})
}

/// A delivery hold has a result and reason even after the playbook has ended.
#[cfg(feature = "delivery_playbook")]
pub fn delivery_hold(hold: &Hold, result: Uuid, reason: &str, evidence: Option<&str>) -> Value {
    let why = match reason {
        "returns_exhausted" => "the delivery playbook reached its repair-return limit",
        _ => "the delivery playbook held the recorded result",
    };
    let mut shown = task_hold(
        hold.attempt_id,
        &hold.status,
        reason,
        &format!("{why} for result {result}"),
    );
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
    let (code, message) = match delivery_policy {
        false => (
            "result_ready_no_delivery_policy",
            "Result ready, no delivery policy: the attempt has stopped with a result, and no delivery playbook is active to decide it. AQ does not complete, publish or send back a result by itself, so the result waits.",
        ),
        true => (
            "result_ready",
            "Result ready: the attempt has stopped with a result, which waits for the delivery playbook's decision.",
        ),
    };
    json!({"code": code, "attempt_id": attempt, "result_id": result, "message": message})
}
