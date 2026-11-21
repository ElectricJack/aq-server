//! Project supervisor escalation for durable scheduling holds. A hold comment
//! and a delivery decision are separate records and have different lifetimes.
use crate::{Result, delivery_decisions};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const DUE: &str = "
WITH waits AS (
 SELECT h.project_id, h.task_id, h.attempt_id, h.comment_id AS identity, h.reason,
        (extract(epoch FROM h.held_at)*1000)::bigint AS since_ms
 FROM task_holds_current h
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, s.report_id,
        CASE WHEN hot.state#>>'{effects,kind}'<>'quiescent' THEN 'replacement_effects_unreconciled'
             ELSE 'replacement_source_retention_unknown' END, s.updated_at_ms
 FROM profiled_auto_settlements s JOIN execution_attempts a USING(attempt_id)
 JOIN profiled_result_heads current ON current.attempt_id=a.attempt_id AND current.report_id=s.report_id
 JOIN profiled_authority_hot hot ON hot.attempt_id=a.attempt_id
 JOIN profiled_launches l ON l.attempt_id=a.attempt_id
 WHERE a.holds_task AND s.disposition='rework_required' AND hot.state#>>'{stop,kind}'='stopped'
 AND (hot.state#>>'{effects,kind}'<>'quiescent' OR hot.state#>>'{retention,kind}'<>'retained')
 AND NOT EXISTS(SELECT 1 FROM task_holds_current h WHERE h.task_id=a.task_id)
), keyed AS (
 SELECT w.*, 'sha256:'||encode(sha256(convert_to('aq.scheduler.hold/'||identity::text,'UTF8')),'hex') AS key
 FROM waits w WHERE since_ms<=$1 AND NOT (identity=ANY($2))
 UNION ALL
 SELECT (l.execution_spec#>>'{lifecycle,project_id}')::uuid, a.task_id, a.attempt_id, h.report_id,
        d.reason, d.decided_at_ms,
        'sha256:'||encode(sha256(convert_to('['||to_json('aq.delivery.supervisor_notify'::text)::text||','||
             to_json(h.report_id::text||':'||d.reason)::text||']','UTF8')),'hex')
 FROM delivery_decisions d JOIN profiled_result_heads h ON h.report_id=d.result_id
 JOIN execution_attempts a ON a.attempt_id=h.attempt_id AND a.task_id=d.task_id
 JOIN profiled_launches l ON l.attempt_id=a.attempt_id
 WHERE d.decision='held' AND d.reason='returns_exhausted' AND a.holds_task
       AND d.decided_at_ms<=$1 AND NOT (h.report_id=ANY($2))
)
SELECT k.project_id,k.task_id,k.attempt_id,k.identity,k.key,k.reason,h.report_id,
       l.execution_spec#>>'{task,version}',p.url FROM keyed k JOIN execution_attempts a USING(attempt_id)
JOIN supervisor_conversation_ids i ON i.project_id=k.project_id
JOIN supervisor_controls c ON c.conversation_id=i.conversation_id AND c.enabled
LEFT JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id
LEFT JOIN profiled_launches l ON l.attempt_id=a.attempt_id
LEFT JOIN delivery_pull_requests p ON p.task_id=k.task_id
WHERE (k.reason<>'returns_exhausted' OR (a.holds_task AND h.report_id=k.identity))
AND NOT EXISTS(SELECT 1 FROM delivery_notifications d WHERE d.project_id=k.project_id AND d.identity_digest=k.key)
ORDER BY k.since_ms,k.identity LIMIT 1 FOR UPDATE OF a SKIP LOCKED";

/// Re-check waits older than ten minutes. Enqueuing the message and recording
/// its deduplication key commit together. A full mailbox does not starve the
/// remaining projects. Before the required tables have landed there is no work.
pub async fn notify<E: std::fmt::Debug>(
    pool: &PgPool,
    now: i64,
    send: impl AsyncFn(
        &mut Transaction<'_, Postgres>,
        Option<Uuid>,
        &str,
        u64,
    ) -> std::result::Result<Uuid, E>,
) -> Result<()> {
    let supported: bool = sqlx::query_scalar("SELECT to_regclass('delivery_notifications') IS NOT NULL AND to_regclass('supervisor_controls') IS NOT NULL")
        .fetch_one(pool).await?;
    if !supported {
        return Ok(());
    }
    let mut seen = Vec::<Uuid>::new();
    for _ in 0..16 {
        let mut tx = pool.begin().await?;
        type Wait = (
            Uuid,
            Uuid,
            Uuid,
            Uuid,
            String,
            String,
            Option<Uuid>,
            Option<String>,
            Option<String>,
        );
        let wait: Option<Wait> = sqlx::query_as(DUE)
            .bind(now - 600_000)
            .bind(&seen)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((project, task, attempt, identity, key, reason, report, version, pr)) = wait
        else {
            break;
        };
        seen.push(identity);
        let mut body = format!(
            "AQ scheduling needs attention. Project {project}, task {task}, attempt {attempt}. Hold reason: {reason}."
        );
        if let Some(v) = version {
            body.push_str(&format!(" Attempt task revision: {v}."));
        }
        if let Some(id) = report {
            body.push_str(&format!(" Result: {id}."));
        }
        if reason == "returns_exhausted" {
            // This is the same identity the policy uses after work.hold. Both
            // senders lock the attempt and commit their receipt with the mail.
            if delivery_decisions::told(&mut tx, project, &key).await? {
                continue;
            }
            body.push_str(" Delivery reached its repair-return limit without a recorded supervisor notification. Inspect the latest result and its PR checks before authorizing another attempt.");
            if let Some(url) = pr {
                body.push_str(&format!(" Pull request: {url}."));
            }
        }
        body.push_str(" Use task.work and task.history with the project and task IDs above. Check the current claim, readiness and prior attempts. Resolve the reported cause or escalate the decision with these identifiers.");
        if aq_machine_control::holds::budget_code(&reason).is_some()
            || reason == "returns_exhausted"
        {
            let request = serde_json::json!({
                "operation_id":"<fresh UUID>", "reason":"<why another attempt is justified>",
                "params":{"project_id":project,"task_id":task,"expected_version":"<current version from task.work>"}
            });
            body.push_str(&format!(" If a replacement attempt is justified, submit task.retry with this command request: {request}. Replace the placeholders and keep the revision as a decimal string. The command registry supplies the task subject, expected versions, and payload operation ID."));
        }
        match send(&mut tx, Some(project), &body, now as u64).await {
            Ok(message) => {
                delivery_decisions::record_told(&mut tx, project, &key, message, now).await?;
                tx.commit().await?;
            }
            Err(e) => {
                tracing::warn!(%project,%task,error=?e,"scheduler supervisor notice deferred");
                tx.rollback().await?;
            }
        }
    }
    Ok(())
}
