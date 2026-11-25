//! Owner-only successor requirements. The Graph intent and offer read share
//! serializable predicates, so an old observation cannot race admission.
use super::*;
use crate::profiled_execution::{results, settlement};
use sqlx::{Postgres, Transaction};

pub(crate) const ACTION: &str = "task.needs.set";
pub(crate) const PENDING: &str = "task_needs_change_pending";

pub(super) async fn writable(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    (project, task, actor): (Uuid, Uuid, Uuid),
    (revision, direct): (u64, bool),
) -> Result<bool> {
    let latest: Option<(Uuid, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "SELECT a.attempt_id,h.report_id,hot.state#>>'{stop,kind}'
         FROM execution_attempts a LEFT JOIN profiled_result_heads h USING(attempt_id)
         LEFT JOIN profiled_authority_hot hot USING(attempt_id)
         WHERE a.task_id=$1 AND (h.report_id IS NOT NULL
           OR NOT (COALESCE(hot.state,a.state)->>'status' IN ('offered','preparing','declined','never_granted')
            AND COALESCE(jsonb_typeof(COALESCE(hot.state,a.state)->'grant'),'null')='null'))
         ORDER BY COALESCE((a.state#>>'{offer,binding,assignment_generation}')::bigint,
           (a.state#>>'{offer,attempt,generation}')::bigint,0) DESC, a.attempt_id DESC LIMIT 1",
    )
    .bind(task).fetch_optional(&mut **tx).await.map_err(crate::error::db)?;
    let Some((attempt, report, stop)) = latest else {
        return Ok(true);
    };
    let other_hold: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE task_id=$1 AND attempt_id<>$2 AND holds_task)",
    )
    .bind(task)
    .bind(attempt)
    .fetch_one(&mut **tx)
    .await
    .map_err(crate::error::db)?;
    if !direct || other_hold {
        return Ok(false);
    }
    let owner: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users u CROSS JOIN project_execution_bindings p
           WHERE u.id=$1 AND u.role='owner' AND p.project_id=$2 AND p.revision=$3
           AND p.binding->>'enabled'='true' FOR SHARE OF u,p)",
    )
    .bind(actor)
    .bind(project)
    .bind(revision as i64)
    .fetch_one(&mut **tx)
    .await
    .map_err(crate::error::db)?;
    if !owner {
        return Ok(false);
    }
    let (Some(report), Some("stopped")) = (report, stop.as_deref()) else {
        return Ok(false);
    };
    let r = results::load_locked(state, tx, report).await?;
    if !r.is_current_report_head
        || r.report.binding.attempt_id != attempt
        || r.spec.task.task_id != task
        || r.spec.lifecycle.project_id != project
        || !r.report.pending_effect_ids.is_empty()
    {
        return Ok(false);
    }
    let finishing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_settlement_evaluations
           WHERE attempt_id=$1 AND disposition IN ('accepted','needs_reconciliation'))
         OR EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=$1
           AND disposition IN ('prepared','possible','confirmed'))",
    )
    .bind(attempt)
    .fetch_one(&mut **tx)
    .await
    .map_err(crate::error::db)?;
    if finishing {
        return Ok(false);
    }
    let reworked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE report_id=$1
           AND attempt_id=$2 AND disposition='rework_required')
         AND NOT EXISTS(SELECT 1 FROM playbook_host_operations WHERE subject_id=$3::uuid::text
             AND request#>>'{result_ready,result_id}'=$1::uuid::text
             AND (status='running' OR (
               command_name='branch.publish' AND status='refused' AND output->>'reason'='unreachable'
               AND request->>'answer_note' IS DISTINCT FROM 'remote_rejected')))
         AND NOT EXISTS(SELECT 1 FROM playbook_effects WHERE receipt->>'result_id'=$1::uuid::text
             AND status='pending')",
    )
    .bind(report)
    .bind(attempt)
    .bind(task)
    .fetch_one(&mut **tx)
    .await
    .map_err(crate::error::db)?;
    #[cfg(feature = "delivery_playbook")]
    let reworked = reworked || settlement::terminal_refusal(tx, &r).await?.is_some();
    if !reworked {
        return Ok(false);
    }
    let effects = settlement::effects::inspect_aq_effects_locked(state, tx, &r).await?;
    Ok(effects.quiescent())
}
