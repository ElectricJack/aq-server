//! Owner-authorized replacement after AQ Git definitively refused publication.
use super::*;

pub(crate) const REASON: &str = "branch_publication_rejected";

pub(super) async fn evidence(
    tx: &mut Transaction<'_, Postgres>,
    result: &results::RecordedResult,
    owner_operation: Uuid,
) -> Result<Option<Value>> {
    let task = result.spec.task.task_id;
    let attempt = result.report.binding.attempt_id;
    if !result.report.pending_effect_ids.is_empty()
        || !aq_machine_control::owner_retry::authorized(tx, owner_operation, task, attempt)
            .await
            .map_err(db)?
    {
        return Ok(None);
    }
    let revision: Option<i64> = sqlx::query_scalar(
        "SELECT i.project_revision FROM owner_task_retry_intents i JOIN dashboard_operations d USING(operation_id)
         WHERE i.operation_id=$1 AND i.project_id=$2 AND i.released_at IS NULL AND d.disposition IN ('pending','unknown')",
    ).bind(owner_operation).bind(result.spec.lifecycle.project_id).fetch_optional(&mut **tx).await.map_err(db)?;
    let (Some(revision), Some((publication, head))) =
        (revision, terminal_refusal(tx, result).await?)
    else {
        return Ok(None);
    };
    if !aq_profiled_state::replacement_sql::pinned_binding_current(
        &mut **tx,
        attempt,
        u64::try_from(revision).map_err(|_| AppError::Internal)?,
    )
    .await
    .map_err(|_| AppError::Internal)?
    {
        return Ok(None);
    }
    Ok(Some(json!({
            "decided_by": "owner_publication_retry",
            "owner_operation_id": owner_operation,
            "publication_operation_id": publication,
            "head": head,
            "report": result.receipt_artifact,
    })))
}

/// Exact terminal publication rejection, under the same attempt lock as host dispatch.
pub(crate) async fn terminal_refusal(
    tx: &mut Transaction<'_, Postgres>,
    result: &results::RecordedResult,
) -> Result<Option<(Uuid, String)>> {
    let task = result.spec.task.task_id;
    let attempt = result.report.binding.attempt_id;
    let report = result.report.report_id;
    sqlx::query_as(
        "SELECT o.operation_id,p.head
         FROM delivery_pushed_results p JOIN profiled_result_reports r USING(report_id,attempt_id)
         JOIN playbook_host_operations o ON o.subject_id=$1::uuid::text AND o.command_name='branch.publish'
           AND o.request#>>'{result_ready,result_id}'=p.report_id::text
           AND o.request#>>'{result_ready,attempt_id}'=p.attempt_id::text
           AND o.request#>>'{result_ready,project_id}'=$4::uuid::text
           AND o.request#>>'{result_ready,task_id}'=$1::uuid::text
           AND o.request->>'result_head'=p.head
         WHERE p.report_id=$2 AND p.attempt_id=$3
           AND convert_from(r.report_bytes,'UTF8')::jsonb->>'outcome'='success'
           AND o.status='refused' AND o.completed_at_ms IS NOT NULL
           AND o.output->>'reason'='unreachable' AND o.request->>'answer_note'='remote_rejected'
           AND NOT EXISTS(SELECT 1 FROM delivery_decisions x WHERE x.result_id=$2)
           AND NOT EXISTS(SELECT 1 FROM playbook_host_operations x WHERE x.subject_id=o.subject_id
             AND x.request#>>'{result_ready,result_id}'=p.report_id::text
             AND (x.status='running' OR x.command_name IN ('pull_request.open','pull_request.merge')
               OR (x.command_name='branch.publish' AND (x.status<>'refused' OR x.completed_at_ms IS NULL
                 OR x.output->>'reason' IS DISTINCT FROM 'unreachable'
                 OR x.request->>'answer_note' IS DISTINCT FROM 'remote_rejected'))))
           AND NOT EXISTS(SELECT 1 FROM playbook_effects x
             WHERE x.receipt->>'result_id'=p.report_id::text AND x.status='pending')
         ORDER BY o.completed_at_ms DESC,o.operation_id DESC LIMIT 1",
    )
    .bind(task)
    .bind(report)
    .bind(attempt)
    .bind(result.spec.lifecycle.project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)
}
