//! Project supervisor escalation for durable scheduling holds. A hold comment
//! and a delivery decision are separate records and have different lifetimes.
use crate::{Result, delivery_decisions};
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use std::{fmt::Debug, time::Duration};
use uuid::Uuid;

const PAGE_SIZE: i64 = 16;
const FIRST_NOTICE_MS: i64 = 600_000;
const MAX_REMINDERS: i32 = 3;
const RETRY_CHECK_MS: i64 = 300_000;

const SEED: &str = "
INSERT INTO supervisor_hold_followups
    (project_id,identity_digest,since_ms,task_id,attempt_id,next_check_at_ms,initial_message_id)
SELECT w.project_id,w.key,w.since_ms,w.task_id,w.attempt_id,
       COALESCE(n.notified_at_ms+$1,w.since_ms+$2),n.message_id
FROM supervisor_hold_waits w
LEFT JOIN delivery_notifications n ON n.project_id=w.project_id AND n.identity_digest=w.key
WHERE NOT EXISTS (SELECT 1 FROM supervisor_hold_followups f
 WHERE f.project_id=w.project_id AND f.identity_digest=w.key AND f.since_ms=w.since_ms)
ORDER BY w.since_ms,w.identity LIMIT $3
ON CONFLICT DO NOTHING";

const PRUNE: &str = "
DELETE FROM supervisor_hold_followups f WHERE
 (f.project_id,f.identity_digest,f.since_ms) IN (
  SELECT f.project_id,f.identity_digest,f.since_ms FROM supervisor_hold_followups f
  WHERE NOT EXISTS (SELECT 1 FROM supervisor_hold_waits w
   WHERE w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms)
  ORDER BY f.next_check_at_ms LIMIT $1 FOR UPDATE OF f SKIP LOCKED)";

const DUE: &str = "
SELECT a.attempt_id FROM supervisor_hold_followups f
JOIN supervisor_hold_waits w ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms
JOIN execution_attempts a ON a.attempt_id=w.attempt_id
JOIN supervisor_conversation_ids i ON i.project_id=w.project_id
JOIN supervisor_controls c ON c.conversation_id=i.conversation_id AND c.enabled
WHERE f.closed_at_ms IS NULL AND f.next_check_at_ms<=$1 AND NOT (a.attempt_id=ANY($2))
ORDER BY f.next_check_at_ms,w.identity LIMIT 1 FOR UPDATE OF a SKIP LOCKED";

const CURRENT: &str = "
SELECT w.*,f.next_check_at_ms,f.last_message_id,f.last_reply_id,f.reminders_sent,
       COALESCE(f.initial_message_id,n.message_id) AS initial_message_id,n.notified_at_ms
FROM supervisor_hold_followups f
JOIN supervisor_hold_waits w ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms
JOIN supervisor_conversation_ids i ON i.project_id=w.project_id
JOIN supervisor_controls c ON c.conversation_id=i.conversation_id AND c.enabled
LEFT JOIN delivery_notifications n ON n.project_id=w.project_id AND n.identity_digest=w.key
WHERE f.closed_at_ms IS NULL AND f.next_check_at_ms<=$1 AND w.attempt_id=$2
ORDER BY w.identity LIMIT 1 FOR UPDATE OF f";

struct Wait {
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
    identity: Uuid,
    key: String,
    reason: String,
    report: Option<Uuid>,
    version: Option<String>,
    pr: Option<String>,
    delivery: bool,
    workspace: Option<Uuid>,
    since: i64,
    initial_message: Option<Uuid>,
    initial_notified_at: Option<i64>,
    last_message: Option<Uuid>,
    last_reply: Option<Uuid>,
    reminders_sent: i32,
}

impl sqlx::FromRow<'_, PgRow> for Wait {
    fn from_row(row: &PgRow) -> std::result::Result<Self, sqlx::Error> {
        Ok(Self {
            project: row.try_get("project_id")?,
            task: row.try_get("task_id")?,
            attempt: row.try_get("attempt_id")?,
            identity: row.try_get("identity")?,
            key: row.try_get("key")?,
            reason: row.try_get("reason")?,
            report: row.try_get("report_id")?,
            version: row.try_get("version")?,
            pr: row.try_get("url")?,
            delivery: row.try_get("delivery")?,
            workspace: row.try_get("workspace_id")?,
            since: row.try_get("since_ms")?,
            initial_message: row.try_get("initial_message_id")?,
            initial_notified_at: row.try_get("notified_at_ms")?,
            last_message: row.try_get("last_message_id")?,
            last_reply: row.try_get("last_reply_id")?,
            reminders_sent: row.try_get("reminders_sent")?,
        })
    }
}

impl Wait {
    fn context(&self) -> String {
        let Self {
            project,
            task,
            attempt,
            reason,
            version,
            report,
            ..
        } = self;
        let mut body = format!(
            "AQ scheduling needs attention. Project {project}, task {task}, attempt {attempt}. Hold reason: {reason}."
        );
        if let Some(v) = version {
            body.push_str(&format!(" Attempt task revision: {v}."));
        }
        if let Some(id) = report {
            body.push_str(&format!(" Result: {id}."));
        }
        if self.delivery && self.initial_message.is_none() {
            body.push_str(if reason == "returns_exhausted" {
                " Delivery reached its repair-return limit without a recorded supervisor notification. Inspect the latest result and its PR checks before authorizing another attempt."
            } else {
                " The delivery playbook held this result without a recorded supervisor notification. Inspect the latest result and its PR checks."
            });
        }
        body
    }

    fn append_guidance(&self, body: &mut String, can_retry: bool) {
        if can_retry && let Some(url) = &self.pr {
            body.push_str(&format!(" Pull request: {url}."));
        }
        body.push_str(" Use task.work, task.history and the current result or check evidence to verify this hold. When the cause is resolved, choose delivery of the retained result or one justified continuation. Otherwise name the blocker and what would clear it, or escalate with these identifiers. AQ schedules another check while the hold remains. Verify retry authorization against the current hold before submitting a retry.");
        if can_retry {
            let request = serde_json::json!({
                "operation_id":"<fresh UUID>", "reason":"<why another attempt is justified>",
                "params":{"project_id":self.project,"task_id":self.task,"expected_version":"<current version from task.work>"}
            });
            body.push_str(&format!(" If a replacement attempt is justified, submit task.retry with this command request: {request}. Replace the placeholders and keep the revision as a decimal string. The command registry supplies the task subject, expected versions, and payload operation ID."));
        }
    }
}

async fn notice_body(
    db: &mut PgConnection,
    wait: &Wait,
    last_reply: Option<&str>,
) -> Result<String> {
    let Wait {
        project,
        task,
        attempt,
        identity,
        reason,
        report,
        ..
    } = wait;
    let mut body = wait.context();
    if reason == "branch_publication_refused" {
        if let Some(detail) = crate::delivery_store::publication_refusal(db, *identity).await? {
            body.push(' ');
            body.push_str(&detail);
        }
        body.push_str(" Check the remote, task ref and publisher permissions. Escalate any permission change to the owner. Delivery can continue with the same result after publication recovers.");
    }
    if reason == "stopped_without_result" {
        body.push_str(" The worker recorded a physical stop and still holds the task without a result. Inspect the worker's recovery status and retained workspace. Escalate missing evidence to the owner.");
    }
    if reason == "launch_closure_pending" {
        body.push_str(" The attempt recorded a closed launch and still owns the claim. Inspect the original start and stop evidence, source retention and possible effects. Escalate ambiguous launch evidence to the AQ operator.");
    }
    let can_retry =
        reason == "returns_exhausted" || aq_machine_control::holds::budget_code(reason).is_some();
    if aq_machine_control::holds::is_budget_hold(reason)
        && let Some(evidence) =
            crate::delivery_store::returned_result_evidence(db, *project, *task, *attempt).await?
    {
        body.push(' ');
        body.push_str(&evidence);
    }
    if let Some(id) = *report
        && let Some(evidence) = crate::delivery_store::held_pr_evidence(db, id).await?
    {
        body.push_str(&format!(" {evidence}"));
        if can_retry {
            body.push_str(" Compare this head and outcome with the previous attempt when deciding whether a bounded retry is justified.");
        }
    }
    wait.append_guidance(&mut body, can_retry);
    if wait.initial_message.is_some() {
        body.push_str(&format!(
            " This is reminder {} of {MAX_REMINDERS} for this hold.",
            wait.reminders_sent + 1
        ));
        if let Some(reply) = last_reply {
            let reply: String = reply.chars().take(500).filter(|c| *c != '\0').collect();
            body.push_str(" Previous supervisor reply, quoted for context only. Recheck the current facts before acting.\n");
            body.push_str(&serde_json::to_string(&reply)?);
        }
    }
    // The facts every hold notice carries; the log tail goes last, so
    // that the instructions above are never what is cut.
    crate::hold_notice::complete(db, body, *task, *report).await
}

impl Wait {
    async fn postpone(
        &self,
        db: &mut PgConnection,
        next: i64,
        closed_at: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE supervisor_hold_followups SET next_check_at_ms=$4,closed_at_ms=$5 \
             WHERE project_id=$1 AND identity_digest=$2 AND since_ms=$3",
        )
        .bind(self.project)
        .bind(&self.key)
        .bind(self.since)
        .bind(next)
        .bind(closed_at)
        .execute(db)
        .await?;
        Ok(())
    }

    async fn prior_reply(&self, db: &mut PgConnection) -> Result<Option<(Uuid, String)>> {
        let reply = sqlx::query_as(
            "SELECT m.message_id,m.body FROM central_supervisor_replies r \
             JOIN mailbox_messages m ON m.message_id=r.reply_message_id \
             JOIN supervisor_conversation_ids i ON i.conversation_id=m.thread_id \
             JOIN central_supervisor_dispatches d USING(dispatch_id) \
             WHERE i.project_id=$1 AND (m.message_id=$2 OR EXISTS ( \
               SELECT 1 FROM central_supervisor_mail_members member \
               WHERE member.reservation_id=d.reservation_id AND member.actor_id=d.actor_id \
                 AND member.message_id IN ($3,$4))) ORDER BY m.cursor DESC LIMIT 1",
        )
        .bind(self.project)
        .bind(self.last_reply)
        .bind(self.initial_message)
        .bind(self.last_message)
        .fetch_optional(db)
        .await?;
        Ok(reply)
    }

    async fn delivered(
        &self,
        db: &mut PgConnection,
        message: Uuid,
        reply: Option<Uuid>,
        now: i64,
        next: i64,
    ) -> Result<()> {
        // Keep the original playbook deduplication key. Reminders do not claim it.
        if self.initial_message.is_none() {
            delivery_decisions::record_told(db, self.project, &self.key, message, now).await?;
        }
        let reminders = self.reminders_sent + i32::from(self.initial_message.is_some());
        sqlx::query(
            "UPDATE supervisor_hold_followups SET initial_message_id=$4,last_message_id=$5, \
                next_check_at_ms=$6,last_reply_id=$7,reminders_sent=$8,owner_attention=$9 \
             WHERE project_id=$1 AND identity_digest=$2 AND since_ms=$3",
        )
        .bind(self.project)
        .bind(&self.key)
        .bind(self.since)
        .bind(self.initial_message.unwrap_or(message))
        .bind(message)
        .bind(next)
        .bind(reply)
        .bind(reminders)
        .bind(reminders >= MAX_REMINDERS)
        .execute(db)
        .await?;
        Ok(())
    }
}

/// Re-check only the current exact hold. The caller reads live Graph before
/// sending when the predecessor has a known workspace. Delivery and the
/// follow-up receipt commit together, and a retry of the sweep reads that receipt.
pub async fn notify_with_graph<E: Debug, G: Debug>(
    pool: &PgPool,
    now: i64,
    closed: impl AsyncFn(Uuid, Uuid) -> std::result::Result<bool, G>,
    send: impl AsyncFn(
        &mut Transaction<'_, Postgres>,
        Option<Uuid>,
        &str,
        u64,
    ) -> std::result::Result<Uuid, E>,
) -> Result<()> {
    let supported: bool =
        sqlx::query_scalar("SELECT to_regclass('supervisor_hold_followups') IS NOT NULL")
            .fetch_one(pool)
            .await?;
    if !supported {
        return Ok(());
    }
    let (limits, _) = aq_role_settings::delivery_limits(pool)
        .await
        .map_err(|_| crate::Error::Unknown)?;
    let interval = limits.hold_followup_interval_ms as i64;
    sqlx::query(PRUNE).bind(PAGE_SIZE).execute(pool).await?;
    sqlx::query(SEED)
        .bind(interval)
        .bind(FIRST_NOTICE_MS)
        .bind(PAGE_SIZE)
        .execute(pool)
        .await?;
    let mut seen = Vec::<Uuid>::new();
    for _ in 0..PAGE_SIZE {
        let mut tx = crate::bridge_tx(pool).await?;
        let attempt: Option<Uuid> = sqlx::query_scalar(DUE)
            .bind(now)
            .bind(&seen)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(attempt) = attempt else {
            break;
        };
        seen.push(attempt);
        // A simultaneous delivery or scheduler transaction may have committed
        // while DUE took the attempt lock. Refresh the candidate in a new statement.
        let wait: Option<Wait> = sqlx::query_as(CURRENT)
            .bind(now)
            .bind(attempt)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(wait) = wait else {
            continue;
        };
        let next = now.saturating_add(interval);
        let graph_closed = match wait.workspace {
            Some(workspace) => {
                tokio::time::timeout(Duration::from_secs(2), closed(workspace, wait.task))
                    .await
                    .ok()
                    .and_then(|result| result.ok())
            }
            None => None,
        };
        match graph_closed {
            Some(false) => {}
            Some(true) => {
                wait.postpone(&mut tx, next, Some(now)).await?;
                tx.commit().await?;
                continue;
            }
            None => {
                wait.postpone(&mut tx, now.saturating_add(RETRY_CHECK_MS), None)
                    .await?;
                tx.commit().await?;
                continue;
            }
        }
        // A newer first notice from the playbook resets the cadence. The
        // owner-attention state remains scheduled for observation after the last reminder.
        let first_next = wait
            .initial_notified_at
            .map(|time| time.saturating_add(interval))
            .filter(|due| wait.last_message.is_none() && *due > now);
        if first_next.is_some() || wait.reminders_sent >= MAX_REMINDERS {
            wait.postpone(&mut tx, first_next.unwrap_or(next), None)
                .await?;
            tx.commit().await?;
            continue;
        }
        let reply = wait.prior_reply(&mut tx).await?;
        let body = notice_body(&mut tx, &wait, reply.as_ref().map(|(_, s)| s.as_str())).await?;
        let mut mail = tx.begin().await?;
        match send(&mut mail, Some(wait.project), &body, now as u64).await {
            Ok(message) => {
                mail.commit().await?;
                wait.delivered(&mut tx, message, reply.map(|(id, _)| id), now, next)
                    .await?;
            }
            Err(error) => {
                mail.rollback().await?;
                tracing::warn!(project=%wait.project, task=%wait.task, ?error, "scheduler supervisor notice deferred");
                wait.postpone(&mut tx, now.saturating_add(RETRY_CHECK_MS), None)
                    .await?;
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Run an AQ-local observation. Integration tests use a supplied Graph check in
/// `notify_with_graph` when exercising a closed Graph task.
pub async fn notify<E: Debug>(
    pool: &PgPool,
    now: i64,
    send: impl AsyncFn(
        &mut Transaction<'_, Postgres>,
        Option<Uuid>,
        &str,
        u64,
    ) -> std::result::Result<Uuid, E>,
) -> Result<()> {
    notify_with_graph(pool, now, async |_, _| Ok::<_, ()>(false), send).await
}

/// Current supervisor follow-up, including the evidence and scheduled time.
/// The project and current hold are joined again so a released result cannot
/// keep the Work view held.
pub async fn followup(pool: &PgPool, project: Uuid, task: Uuid) -> Option<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('attempt_id',w.attempt_id,'result_id',w.report_id, \
          'status',CASE WHEN f.owner_attention THEN 'owner_attention' \
                        WHEN NOT COALESCE(c.enabled,FALSE) THEN 'awaiting_supervisor' \
                        WHEN w.workspace_id IS NULL THEN 'awaiting_scope' ELSE 'scheduled' END, \
          'hold_reason',w.reason,'next_check_at_ms',f.next_check_at_ms,'reminders_sent',f.reminders_sent) \
         FROM supervisor_hold_followups f JOIN supervisor_hold_waits w \
           ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms \
         LEFT JOIN supervisor_conversation_ids i ON i.project_id=w.project_id \
         LEFT JOIN supervisor_controls c ON c.conversation_id=i.conversation_id \
         WHERE w.project_id=$1 AND w.task_id=$2 AND f.closed_at_ms IS NULL",
    )
    .bind(project)
    .bind(task)
    .fetch_optional(pool)
    .await
    .ok()?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait() -> Wait {
        Wait {
            project: Uuid::from_u128(1),
            task: Uuid::from_u128(2),
            attempt: Uuid::from_u128(3),
            identity: Uuid::from_u128(4),
            key: String::new(),
            reason: "returns_exhausted".into(),
            report: None,
            version: None,
            pr: Some("https://example.test/pr/7".into()),
            delivery: false,
            workspace: None,
            since: 0,
            initial_message: None,
            initial_notified_at: None,
            last_message: None,
            last_reply: None,
            reminders_sent: 0,
        }
    }

    #[test]
    fn notice_context_preserves_identifiers_and_delivery_explanations() {
        let mut wait = wait();
        let context = format!(
            "AQ scheduling needs attention. Project {}, task {}, attempt {}. Hold reason: returns_exhausted.",
            wait.project, wait.task, wait.attempt
        );
        assert_eq!(wait.context(), context);
        wait.version = Some("12".into());
        wait.report = Some(Uuid::from_u128(5));
        let context = format!(
            "{context} Attempt task revision: 12. Result: {}.",
            wait.report.unwrap()
        );
        assert_eq!(wait.context(), context);
        wait.delivery = true;
        assert_eq!(
            wait.context(),
            format!(
                "{context} Delivery reached its repair-return limit without a recorded supervisor notification. Inspect the latest result and its PR checks before authorizing another attempt."
            )
        );
        wait.reason = "policy_hold".into();
        assert_eq!(
            wait.context(),
            format!(
                "{} The delivery playbook held this result without a recorded supervisor notification. Inspect the latest result and its PR checks.",
                context.replace("returns_exhausted", "policy_hold")
            )
        );
    }

    #[test]
    fn retry_guidance_preserves_evidence_and_the_command_request() {
        let mut wait = wait();
        let guidance = " Use task.work, task.history and the current result or check evidence to verify this hold. When the cause is resolved, choose delivery of the retained result or one justified continuation. Otherwise name the blocker and what would clear it, or escalate with these identifiers. AQ schedules another check while the hold remains. Verify retry authorization against the current hold before submitting a retry.";
        let mut body = "Recorded evidence.".to_owned();
        wait.append_guidance(&mut body, false);
        assert_eq!(body, format!("Recorded evidence.{guidance}"));
        for pr in [Some("https://example.test/pr/7"), None] {
            wait.pr = pr.map(str::to_owned);
            let mut body = "Recorded evidence.".to_owned();
            wait.append_guidance(&mut body, true);
            let (prefix, request) = body.split_once(" If a replacement attempt is justified, submit task.retry with this command request: ").unwrap();
            let pr = pr.map_or(String::new(), |url| format!(" Pull request: {url}."));
            assert_eq!(prefix, format!("Recorded evidence.{pr}{guidance}"));
            let (request, suffix) = request.split_once(". Replace the placeholders").unwrap();
            let request: serde_json::Value = serde_json::from_str(request).unwrap();
            assert_eq!(
                request,
                serde_json::json!({
                    "operation_id": "<fresh UUID>",
                    "reason": "<why another attempt is justified>",
                    "params": {
                        "project_id": wait.project,
                        "task_id": wait.task,
                        "expected_version": "<current version from task.work>"
                    }
                })
            );
            assert_eq!(
                suffix,
                " and keep the revision as a decimal string. The command registry supplies the task subject, expected versions, and payload operation ID."
            );
        }
    }
}
