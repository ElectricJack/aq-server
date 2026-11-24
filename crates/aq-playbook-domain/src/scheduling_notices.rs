//! Project supervisor escalation for durable scheduling holds. A hold comment
//! and a delivery decision are separate records and have different lifetimes.
//! A new hold is told to its project's supervisor once, on its own; after
//! that every open hold of the project appears in one grouped project review
//! per interval, for as long as it lasts.
use crate::{Result, delivery_decisions, delivery_store};
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow};
use std::{fmt::Debug, time::Duration};
use uuid::Uuid;

const PAGE_SIZE: i64 = 16;
const FIRST_NOTICE_MS: i64 = 600_000;
const RETRY_CHECK_MS: i64 = 300_000;

/// What a supervisor's turn about held work ends with.
const DECISION: &str = "Decide, and end the turn with one of these: retry with task.retry; deliver the retained result where task.work shows that path; file a task that fixes the cause with task.create and make the held tasks wait on it with task.dependency.add and \"resume_held\": true; or escalate with supervisor.escalate.";

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
SELECT a.attempt_id,w.project_id FROM supervisor_hold_followups f
JOIN supervisor_hold_waits w ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms
JOIN execution_attempts a ON a.attempt_id=w.attempt_id
JOIN supervisor_conversation_ids i ON i.project_id=w.project_id
JOIN supervisor_controls c ON c.conversation_id=i.conversation_id AND c.enabled
WHERE f.closed_at_ms IS NULL AND f.next_check_at_ms<=$1 AND NOT (a.attempt_id=ANY($2))
ORDER BY f.next_check_at_ms,w.identity LIMIT 1 FOR UPDATE OF a SKIP LOCKED";

const CURRENT: &str = "
SELECT w.*,f.next_check_at_ms,f.last_message_id,
       COALESCE(f.initial_message_id,n.message_id) AS initial_message_id,n.notified_at_ms
FROM supervisor_hold_followups f
JOIN supervisor_hold_waits w ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms
JOIN supervisor_conversation_ids i ON i.project_id=w.project_id
JOIN supervisor_controls c ON c.conversation_id=i.conversation_id AND c.enabled
LEFT JOIN delivery_notifications n ON n.project_id=w.project_id AND n.identity_digest=w.key
WHERE f.closed_at_ms IS NULL AND f.next_check_at_ms<=$1 AND w.attempt_id=$2
ORDER BY w.identity LIMIT 1 FOR UPDATE OF f";

/// Every open hold of the project that its supervisor was already told about,
/// with the cause evidence its notice carries.
const OPEN: &str = "
SELECT f.identity_digest,f.since_ms,w.task_id,w.reason,w.workspace_id,
       CASE WHEN pr.checks_state='failed' THEN
         (SELECT string_agg(c,', ') FROM jsonb_array_elements_text(pr.checks_failed) c) END AS checks,
       CASE WHEN pr.checks_state='failed' THEN left(pr.checks_log_tail,4096) END AS log,
       convert_from(r.report_bytes,'UTF8')::jsonb->>'conclusion' AS conclusion
FROM supervisor_hold_followups f
JOIN supervisor_hold_waits w ON w.project_id=f.project_id AND w.key=f.identity_digest AND w.since_ms=f.since_ms
LEFT JOIN delivery_notifications n ON n.project_id=w.project_id AND n.identity_digest=w.key
LEFT JOIN delivery_pull_requests pr ON pr.task_id=w.task_id
LEFT JOIN profiled_result_reports r ON r.attempt_id=w.attempt_id AND r.report_id=w.report_id
WHERE f.project_id=$1 AND f.closed_at_ms IS NULL AND COALESCE(f.initial_message_id,n.message_id) IS NOT NULL
ORDER BY f.since_ms,f.identity_digest FOR UPDATE OF f";

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
        body.push_str(" Use task.work, task.history and the current result or check evidence to find the cause, and check whether other tasks in this project are held for the same one. ");
        body.push_str(DECISION);
        if can_retry {
            let request = serde_json::json!({
                "operation_id":"<fresh UUID>", "reason":"<why another attempt is justified>",
                "params":{"project_id":self.project,"task_id":self.task,"expected_version":"<current version from task.work>"}
            });
            body.push_str(&format!(" To retry, submit task.retry with this command request: {request}. Replace the placeholders and keep the revision as a decimal string. The command registry supplies the task subject, expected versions, and payload operation ID."));
        }
    }
}

async fn notice_body(db: &mut PgConnection, wait: &Wait) -> Result<String> {
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
        body.push_str(" Check the remote, task ref and publisher permissions. Escalate a permission change it needs with supervisor.escalate. Delivery can continue with the same result after publication recovers.");
    }
    if reason == "stopped_without_result" {
        body.push_str(" The worker recorded a physical stop and still holds the task without a result. Inspect the worker's recovery status and retained workspace. Escalate missing evidence with supervisor.escalate.");
    }
    if reason == "launch_closure_pending" {
        body.push_str(" The attempt recorded a closed launch and still owns the claim. Inspect the original start and stop evidence, source retention and possible effects. Its stop cannot be proven, so a new attempt could run alongside it: escalate ambiguous launch evidence with supervisor.escalate.");
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
    // The facts every hold notice carries; the log tail goes last, so
    // that the instructions above are never what is cut.
    crate::hold_notice::complete(db, body, *project, *task, *report).await
}

impl Wait {
    async fn postpone(
        &self,
        db: &mut PgConnection,
        next: i64,
        closed_at: Option<i64>,
    ) -> Result<()> {
        schedule(db, self.project, (&self.key, self.since), next, closed_at).await
    }

    /// The first notice for this hold was sent: it claims the playbook's
    /// deduplication key, and the hold joins the project review from now on.
    async fn delivered(
        &self,
        db: &mut PgConnection,
        message: Uuid,
        now: i64,
        next: i64,
    ) -> Result<()> {
        delivery_decisions::record_told(db, self.project, &self.key, message, now).await?;
        sqlx::query(
            "UPDATE supervisor_hold_followups SET initial_message_id=$4,last_message_id=$4,next_check_at_ms=$5 \
             WHERE project_id=$1 AND identity_digest=$2 AND since_ms=$3",
        )
        .bind(self.project)
        .bind(&self.key)
        .bind(self.since)
        .bind(message)
        .bind(next)
        .execute(db)
        .await?;
        Ok(())
    }
}

async fn schedule(
    db: &mut PgConnection,
    project: Uuid,
    (key, since): (&str, i64),
    next: i64,
    closed_at: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE supervisor_hold_followups SET next_check_at_ms=$4,closed_at_ms=$5 \
         WHERE project_id=$1 AND identity_digest=$2 AND since_ms=$3",
    )
    .bind(project)
    .bind(key)
    .bind(since)
    .bind(next)
    .bind(closed_at)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether Graph reports the task closed; `None` when that is unknown.
async fn graph_closed<G>(
    check: Option<impl Future<Output = std::result::Result<bool, G>>>,
) -> Option<bool> {
    tokio::time::timeout(Duration::from_secs(2), check?)
        .await
        .ok()?
        .ok()
}

/// One open hold in a project review.
struct Held {
    key: String,
    since: i64,
    task: Uuid,
    reason: String,
    /// What groups it beyond its reason: the failed check, or an agent's blocked report.
    cause: Option<String>,
    /// The evidence behind the cause, quoted as data.
    sample: Option<String>,
}

/// Untrusted text as a short JSON string.
fn quoted(text: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).take(160).collect();
    serde_json::to_string(&text).unwrap_or_default()
}

/// The cause a hold notice's evidence already names: the failed checks with
/// the first line of the failed log, or the reason of an explicit agent blocker.
fn cause(
    checks: Option<String>,
    log: Option<String>,
    conclusion: Option<String>,
) -> (Option<String>, Option<String>) {
    if let Some(checks) = checks.filter(|c| !c.is_empty()) {
        let line = log.and_then(|log| {
            log.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(quoted)
        });
        let sample = line
            .map(|l| format!("first line of the oldest's failed log (CI output, untrusted): {l}"));
        return (Some(format!("failed check {}", quoted(&checks))), sample);
    }
    let blocked = conclusion
        .as_deref()
        .and_then(|c| c.strip_prefix("Outcome: blocked"));
    if let Some(rest) = blocked.filter(|rest| rest.is_empty() || rest.starts_with(';')) {
        let rest = rest.trim_start_matches(';').trim();
        let sample = (!rest.is_empty()).then(|| {
            format!(
                "the oldest's report (agent text, untrusted): {}",
                quoted(rest)
            )
        });
        return (Some("agent reported blocked".into()), sample);
    }
    (None, None)
}

fn age(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    match (minutes / 1440, minutes / 60 % 24, minutes % 60) {
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// One message about every open hold of `project`, grouped by reason and
/// cause, largest group first. Task lists are cut to the message limit; the
/// counts and the guidance never are.
fn review_body(project: Uuid, now: i64, held: &[Held]) -> String {
    let mut groups: Vec<(&str, Option<&str>, Vec<&Held>)> = Vec::new();
    for h in held {
        let (reason, cause) = (h.reason.as_str(), h.cause.as_deref());
        match groups.iter_mut().find(|g| (g.0, g.1) == (reason, cause)) {
            Some(group) => group.2.push(h),
            None => groups.push((reason, cause, vec![h])),
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.2.len()));
    let head = format!(
        "AQ project review for project {project}: {} held {}, grouped by cause so that a cause several tasks share reads as one problem.",
        held.len(),
        if held.len() == 1 { "task" } else { "tasks" }
    );
    let tail = format!(
        "\n\nUse task.work and task.history on a task of each group to confirm its cause. {DECISION} One fix task can serve every task of a group."
    );
    // Room for one " and N more." per group and one closing line.
    const MORE: usize = 32;
    let mut room =
        delivery_store::SUPERVISOR_MESSAGE_BYTES.saturating_sub(head.len() + tail.len() + MORE);
    let mut lines = Vec::new();
    for (reason, cause, members) in &groups {
        // Members are oldest first.
        let oldest = members[0];
        let mut line = format!("\n- {} held for {reason}", members.len());
        if let Some(cause) = cause {
            line.push_str(&format!(", {cause}"));
        }
        line.push_str(&format!(", oldest held {}", age(now - oldest.since)));
        if let Some(sample) = &oldest.sample {
            line.push_str(&format!("; {sample}"));
        }
        line.push_str(". Tasks:");
        if line.len() + MORE > room {
            break;
        }
        room -= line.len() + MORE;
        lines.push(line);
    }
    let mut body = head;
    for (line, (_, _, members)) in lines.iter().zip(&groups) {
        body.push_str(line);
        let mut listed = 0;
        for h in members {
            let item = format!("{} {}", if listed == 0 { "" } else { "," }, h.task);
            if item.len() > room {
                break;
            }
            room -= item.len();
            body.push_str(&item);
            listed += 1;
        }
        if listed < members.len() {
            body.push_str(&format!(" and {} more", members.len() - listed));
        }
        body.push('.');
    }
    if lines.len() < groups.len() {
        body.push_str(&format!("\n- {} more groups.", groups.len() - lines.len()));
    }
    body + &tail
}

/// The project's open holds for a review, locked. A hold Graph reports closed
/// is closed instead of listed; `wait` was checked already.
async fn open_holds<G, GraphCheck>(
    db: &mut PgConnection,
    wait: &Wait,
    now: i64,
    closed: &impl Fn(Uuid, Uuid) -> GraphCheck,
) -> Result<Vec<Held>>
where
    GraphCheck: Future<Output = std::result::Result<bool, G>>,
{
    let rows = sqlx::query(OPEN)
        .bind(wait.project)
        .fetch_all(&mut *db)
        .await?;
    let mut held = Vec::new();
    for row in rows {
        let (key, since): (String, i64) =
            (row.try_get("identity_digest")?, row.try_get("since_ms")?);
        let task: Uuid = row.try_get("task_id")?;
        let workspace: Option<Uuid> = row.try_get("workspace_id")?;
        let current = (key.as_str(), since) == (wait.key.as_str(), wait.since);
        if !current && graph_closed(workspace.map(|w| closed(w, task))).await == Some(true) {
            schedule(db, wait.project, (&key, since), now, Some(now)).await?;
            continue;
        }
        let (cause, sample) = cause(
            row.try_get("checks")?,
            row.try_get("log")?,
            row.try_get("conclusion")?,
        );
        held.push(Held {
            key,
            since,
            task,
            reason: row.try_get("reason")?,
            cause,
            sample,
        });
    }
    Ok(held)
}

/// The review was sent: every hold in it waits one interval for the next.
async fn reviewed(
    db: &mut PgConnection,
    project: Uuid,
    held: &[Held],
    message: Uuid,
    next: i64,
) -> Result<()> {
    let keys: Vec<&str> = held.iter().map(|h| h.key.as_str()).collect();
    let since: Vec<i64> = held.iter().map(|h| h.since).collect();
    sqlx::query(
        "UPDATE supervisor_hold_followups SET last_message_id=$2,next_check_at_ms=$3 \
         WHERE project_id=$1 AND (identity_digest,since_ms) IN (SELECT * FROM unnest($4::text[],$5::bigint[]))",
    )
    .bind(project)
    .bind(message)
    .bind(next)
    .bind(&keys)
    .bind(&since)
    .execute(db)
    .await?;
    Ok(())
}

/// Re-check only the current exact hold. The caller reads live Graph before
/// sending when the predecessor has a known workspace. Delivery and the
/// follow-up receipt commit together, and a retry of the sweep reads that receipt.
/// A hold's first notice is its own; a due hold already told about sends
/// the project review instead.
pub async fn notify_with_graph<E, G, GraphCheck>(
    pool: &PgPool,
    now: i64,
    closed: impl Fn(Uuid, Uuid) -> GraphCheck + Send,
    send: impl AsyncFn(
        &mut Transaction<'_, Postgres>,
        Option<Uuid>,
        &str,
        u64,
    ) -> std::result::Result<Uuid, E>,
) -> Result<()>
where
    E: Debug,
    GraphCheck: Future<Output = std::result::Result<bool, G>> + Send,
{
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
        let due: Option<(Uuid, Uuid)> = sqlx::query_as(DUE)
            .bind(now)
            .bind(&seen)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((attempt, project)) = due else {
            break;
        };
        seen.push(attempt);
        // One sweep at a time tells a project, since a review covers all of it.
        let free: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended('aq.hold-review/'||$1::text,0))",
        )
        .bind(project)
        .fetch_one(&mut *tx)
        .await?;
        if !free {
            continue;
        }
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
        match graph_closed(wait.workspace.map(|w| closed(w, wait.task))).await {
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
        // A newer first notice from the playbook resets the cadence.
        let first_next = wait
            .initial_notified_at
            .map(|time| time.saturating_add(interval))
            .filter(|due| wait.last_message.is_none() && *due > now);
        if let Some(first_next) = first_next {
            wait.postpone(&mut tx, first_next, None).await?;
            tx.commit().await?;
            continue;
        }
        let (body, review) = if wait.initial_message.is_none() {
            (notice_body(&mut tx, &wait).await?, None)
        } else {
            let held = open_holds(&mut tx, &wait, now, &closed).await?;
            (review_body(project, now, &held), Some(held))
        };
        let mut mail = tx.begin().await?;
        match send(&mut mail, Some(wait.project), &body, now as u64).await {
            Ok(message) => {
                mail.commit().await?;
                match &review {
                    None => wait.delivered(&mut tx, message, now, next).await?,
                    Some(held) => reviewed(&mut tx, project, held, message, next).await?,
                }
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
    notify_with_graph(pool, now, |_, _| async { Ok::<_, ()>(false) }, send).await
}

/// Current supervisor follow-up, including the evidence and scheduled time.
/// The project and current hold are joined again so a released result cannot
/// keep the Work view held.
pub async fn followup(pool: &PgPool, project: Uuid, task: Uuid) -> Option<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('attempt_id',w.attempt_id,'result_id',w.report_id, \
          'status',CASE WHEN NOT COALESCE(c.enabled,FALSE) THEN 'awaiting_supervisor' \
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
        let guidance = " Use task.work, task.history and the current result or check evidence to find the cause, and check whether other tasks in this project are held for the same one. Decide, and end the turn with one of these: retry with task.retry; deliver the retained result where task.work shows that path; file a task that fixes the cause with task.create and make the held tasks wait on it with task.dependency.add and \"resume_held\": true; or escalate with supervisor.escalate.";
        let mut body = "Recorded evidence.".to_owned();
        wait.append_guidance(&mut body, false);
        assert_eq!(body, format!("Recorded evidence.{guidance}"));
        for pr in [Some("https://example.test/pr/7"), None] {
            wait.pr = pr.map(str::to_owned);
            let mut body = "Recorded evidence.".to_owned();
            wait.append_guidance(&mut body, true);
            let (prefix, request) = body
                .split_once(" To retry, submit task.retry with this command request: ")
                .unwrap();
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

    fn held(task: u128, reason: &str, since: i64, cause: Option<&str>) -> Held {
        Held {
            key: format!("k{task}"),
            since,
            task: Uuid::from_u128(task),
            reason: reason.into(),
            cause: cause.map(str::to_owned),
            sample: cause.map(|_| "a sample".to_owned()),
        }
    }

    #[test]
    fn a_review_groups_holds_by_reason_and_cause_with_count_and_oldest_age() {
        let now = 3 * 86_400_000;
        let check = Some("failed check \"tests\"");
        let holds = [
            held(1, "returns_exhausted", 0, check),
            held(2, "lineage_retry_budget_exhausted", 86_400_000, None),
            held(3, "returns_exhausted", 2 * 86_400_000, check),
        ];
        let body = review_body(Uuid::from_u128(9), now, &holds);
        assert!(body.starts_with(&format!(
            "AQ project review for project {}: 3 held tasks",
            Uuid::from_u128(9)
        )));
        let (one, three) = (Uuid::from_u128(1), Uuid::from_u128(3));
        assert!(body.contains(&format!(
            "\n- 2 held for returns_exhausted, failed check \"tests\", oldest held 3d 0h; a sample. Tasks: {one}, {three}."
        )), "{body}");
        assert!(
            body.contains(&format!(
                "\n- 1 held for lineage_retry_budget_exhausted, oldest held 2d 0h. Tasks: {}.",
                Uuid::from_u128(2)
            )),
            "{body}"
        );
        assert!(body.ends_with(&format!(
            "{DECISION} One fix task can serve every task of a group."
        )));
    }

    #[test]
    fn a_long_review_cuts_task_lists_and_keeps_the_guidance() {
        let holds: Vec<Held> = (0..2000)
            .map(|n| held(n, "returns_exhausted", 0, None))
            .collect();
        let body = review_body(Uuid::nil(), 60_000, &holds);
        assert!(body.len() <= delivery_store::SUPERVISOR_MESSAGE_BYTES);
        assert!(body.contains("\n- 2000 held for returns_exhausted, oldest held 1m. Tasks:"));
        assert!(body.contains(" more."), "{body}");
        assert!(body.ends_with("One fix task can serve every task of a group."));
    }

    #[test]
    fn the_cause_is_the_failed_check_or_an_explicit_agent_blocker() {
        let (cause_text, sample) = cause(
            Some("tests (shard 3/6)".into()),
            Some("\n  error: no database\nmore".into()),
            None,
        );
        assert_eq!(cause_text.unwrap(), "failed check \"tests (shard 3/6)\"");
        assert!(
            sample
                .unwrap()
                .ends_with("(CI output, untrusted): \"error: no database\"")
        );
        let (cause_text, sample) = cause(
            None,
            None,
            Some("Outcome: blocked; needs PostgreSQL".into()),
        );
        assert_eq!(cause_text.unwrap(), "agent reported blocked");
        assert!(sample.unwrap().ends_with("\"needs PostgreSQL\""));
        assert_eq!(
            cause(
                Some(String::new()),
                None,
                Some("Outcome: blockedish".into())
            ),
            (None, None)
        );
    }
}
