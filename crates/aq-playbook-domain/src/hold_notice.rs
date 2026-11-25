//! What every hold notice to a project supervisor carries (spec 0021 section
//! 5.2), whatever the hold's source: the attempts of the task with their
//! outcomes, the recorded result's pull request and failed check evidence
//! with the log tail, and the commands that apply. The delivery playbook's
//! notice ([`crate::delivery_decisions::notification`]) and the scheduler's
//! ([`crate::scheduling_notices`]) both end with it, so that a delivery hold
//! is one case of the general notice. Mechanism only: it states facts and
//! names commands; what to do is the supervisor's decision.
use crate::{Result, delivery_store};
use sqlx::PgConnection;
use uuid::Uuid;

/// The commands a supervisor can answer a hold with. Names only: each
/// command's own contract is in the command registry.
pub const COMMANDS: &str = "Commands that apply to a held task: task.work (the hold and its reasons), task.history (its attempts), task.retry (a new attempt), task.create (a task that fixes the cause, for every task it holds back), task.dependency.add with \"resume_held\": true (make this task wait on that fix; it resumes when the fix completes), supervisor.escalate (a decision that is not yours to make), attempt.abandon (release a claim whose attempt will not finish), task.edit (change the task) and task.cancel (end it).";

/// One attempt of the task, newest first, as a line of the notice.
fn attempt_line((attempt, outcome, stopped, holds): (Uuid, Option<String>, bool, bool)) -> String {
    let outcome = outcome.as_deref().unwrap_or("no result recorded");
    let state = if stopped { "stopped" } else { "not stopped" };
    let claim = if holds { ", holds the task" } else { "" };
    format!("- attempt {attempt}: outcome {outcome}, {state}{claim}.")
}

/// The attempts of `task` with their recorded outcomes, newest first, cut
/// to `room` bytes. `None` when the task has no attempt.
pub async fn attempts(db: &mut PgConnection, task: Uuid, room: usize) -> Result<Option<String>> {
    let rows: Vec<(Uuid, Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT a.attempt_id,convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome',
                s.attempt_id IS NOT NULL,a.holds_task
         FROM execution_attempts a
         LEFT JOIN profiled_launches l ON l.attempt_id=a.attempt_id
         LEFT JOIN profiled_stopped_receipts s ON s.attempt_id=a.attempt_id
         LEFT JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id
         LEFT JOIN profiled_result_reports r ON r.attempt_id=h.attempt_id AND r.report_id=h.report_id
         WHERE a.task_id=$1
         ORDER BY (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT DESC NULLS LAST,
                  a.attempt_id DESC",
    )
    .bind(task)
    .fetch_all(db)
    .await?;
    let mut text = String::from("Attempts of this task, newest first:");
    let mut listed = 0;
    for row in rows {
        let line = attempt_line(row);
        if text.len() + line.len() + 1 > room {
            break;
        }
        text.push('\n');
        text.push_str(&line);
        listed += 1;
    }
    Ok((listed > 0).then_some(text))
}

/// A notice with the shared facts appended: the attempts, the commands, and
/// last the failed check's redacted log tail, cut to what the supervisor's
/// mailbox accepts so that a long log never costs the notice itself.
/// `result` is the recorded result the hold is about, if any; the caller
/// has put its pull request evidence ([`delivery_store::held_pr_evidence`])
/// in `body`.
pub async fn complete(
    db: &mut PgConnection,
    mut body: String,
    project: Uuid,
    task: Uuid,
    result: Option<Uuid>,
) -> Result<String> {
    let log = match result {
        Some(result) => {
            // result.detail reads the report revision pin, with no profile parameter.
            let revision: Option<String> = sqlx::query_scalar(
                "SELECT p.report_revision FROM result_revision_pins p
                 JOIN profiled_result_reports r ON r.report_id=p.report_id AND r.report_digest=p.report_revision
                 WHERE p.project_id=$1 AND p.task_id=$2 AND p.report_id=$3",
            )
            .bind(project)
            .bind(task)
            .bind(result)
            .fetch_optional(&mut *db)
            .await?;
            if let Some(revision) = revision {
                let request =
                    serde_json::json!({"params":{"result_id":result,"revision":revision}});
                body.push_str(&format!("\n\nRead the exact recorded result and its checks using result.detail with this command request: {request}."));
            }
            delivery_store::held_check_log(db, result).await?
        }
        None => None,
    };
    let room = delivery_store::SUPERVISOR_MESSAGE_BYTES / 4;
    if let Some(list) = attempts(db, task, room).await? {
        body.push_str("\n\n");
        body.push_str(&list);
    }
    body.push_str("\n\n");
    body.push_str(COMMANDS);
    Ok(match log {
        Some(log) => delivery_store::with_log(body, &log),
        None => body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_line_states_its_outcome_stop_and_claim() {
        let id = Uuid::nil();
        assert_eq!(
            attempt_line((id, Some("failed".into()), true, true)),
            format!("- attempt {id}: outcome failed, stopped, holds the task.")
        );
        assert_eq!(
            attempt_line((id, None, false, false)),
            format!("- attempt {id}: outcome no result recorded, not stopped.")
        );
    }

    #[test]
    fn the_commands_name_every_command_that_applies() {
        for name in [
            "task.work",
            "task.history",
            "task.retry",
            "task.create",
            "task.dependency.add with \"resume_held\": true",
            "supervisor.escalate",
            "attempt.abandon",
            "task.edit",
            "task.cancel",
        ] {
            assert!(COMMANDS.contains(name), "{name}");
        }
    }
}
