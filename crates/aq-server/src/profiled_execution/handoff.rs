//! Spec 0007 phase 1: the successor of a handed-off attempt (an
//! `attempt_handoffs` row) starts from the predecessor's saved work. The saved
//! checkpoint becomes an aq-git candidate on the predecessor's admitted base;
//! the successor gets a fresh export of that commit, never the predecessor's
//! export. The handoff note goes on the task thread once.
use super::replacement::Replacement;
use crate::{
    AppState,
    delivery::{CandidateRequest, CandidateState, CommitMetadata, OperationContext},
    error::AppError,
};
use aq_task_comments::{Author, Thread};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

pub struct HandoffBase {
    pub oid: String,
    /// The candidate's central retention handle (its operation id).
    pub retention_handle: Uuid,
    pub predecessor: Uuid,
}
fn internal<E>(_: E) -> AppError {
    AppError::Internal
}
pub use aq_machine_control::handoff_candidate_operation as candidate_operation;

/// `None` keeps the replacement's old behaviour (the predecessor's export).
pub async fn successor_base(
    state: &AppState,
    task: Uuid,
    replacement: &Replacement,
) -> Result<Option<HandoffBase>, AppError> {
    let predecessor = replacement.predecessor;
    let pool = &state.store.pool;
    let mut conn = pool.acquire().await.map_err(internal)?;
    if !aq_machine_control::has_format(&mut conn)
        .await
        .map_err(internal)?
    {
        return Ok(None);
    }
    let Some(row) = aq_machine_control::handoff_of(&mut conn, predecessor)
        .await
        .map_err(internal)?
    else {
        return Ok(None);
    };
    drop(conn);
    let Some(delivery) = state.delivery() else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(%predecessor, "handoff successor needs aq-git delivery; using the original base");
        }
        return Ok(None);
    };
    let report: Option<Uuid> =
        sqlx::query_scalar("SELECT report_id FROM profiled_result_heads WHERE attempt_id=$1")
            .bind(predecessor)
            .fetch_optional(pool)
            .await
            .map_err(internal)?;
    let Some(report) = report else {
        return Ok(None);
    };
    let mut tx = pool.begin().await.map_err(internal)?;
    let r = super::results::load_locked(state, &mut tx, report).await?;
    let mut source = None;
    for op in &r.report.source_receipt_ids {
        match crate::source_retention::load_confirmed_locked(&mut tx, *op, &r).await {
            Ok(_) => {
                source = Some(*op);
                break;
            }
            Err(AppError::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
    }
    tx.rollback().await.map_err(internal)?;
    let (Some(source), true) = (source, r.spec.task.task_id == task) else {
        tracing::warn!(%predecessor, "handoff has no confirmed saved source; using the original base");
        return Ok(None);
    };
    let admitted = &r.spec.source;
    let operation = candidate_operation(predecessor);
    let sha1 = |hex: &str| ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.into(),
    };
    // A retried poll replays the exact inputs frozen with the first intent.
    let (base, sources, commit) = match delivery.frozen_inputs(operation).await.ok().flatten() {
        Some(inputs) => (
            serde_json::from_value(inputs["base"].clone()).map_err(internal)?,
            serde_json::from_value(inputs["source_receipts"].clone()).map_err(internal)?,
            serde_json::from_value(inputs["commit"].clone()).map_err(internal)?,
        ),
        None => (
            sha1(&admitted.resolved_object.hex),
            vec![source],
            CommitMetadata {
                name: "AQ".into(),
                email: "aq@aq.invalid".into(),
                message: aq_machine_control::handoff_commit_message(
                    task,
                    predecessor,
                    r.report.conclusion.as_str(),
                ),
            },
        ),
    };
    let created = delivery
        .create_candidate(CandidateRequest {
            context: OperationContext {
                operation_id: operation,
                repository_id: admitted.repository_id,
                binding_generation: admitted.repository_binding_generation,
            },
            base,
            source_receipts: sources,
            commit,
        })
        .await;
    let outcome = created.map(|r| r.outcome);
    let ready = match &outcome {
        Ok(Ok(c)) if c.state == CandidateState::Ready && c.retention_handle == operation => {
            c.candidate.as_ref().map(|o| o.hex.clone())
        }
        _ => None,
    };
    let Some(oid) = ready else {
        let outcome = outcome.map(|o| o.map(|c| (c.state, c.conflict_step)));
        tracing::warn!(%predecessor, ?outcome, "handoff candidate not ready; using the original base");
        return Ok(None);
    };
    let mut note = row.note_comment_id;
    // The conclusion is the worker's text, so the predecessor agent authors it.
    use aq_session_protocol::SessionIdentity::Remote;
    if let (None, Remote { session_id, .. }) = (note, &r.report.binding.session) {
        let id = aq_machine_control::handoff_note_operation(predecessor);
        let body =
            aq_machine_control::handoff_note(predecessor, row.reason, r.report.conclusion.as_str());
        let thread = Thread {
            project_id: admitted.project_id,
            task_id: task,
        };
        let (session_id, attempt_id) = (*session_id, predecessor);
        let author = Author::Agent {
            session_id,
            attempt_id,
        };
        match aq_task_comments::add(pool, id, thread, &author, &body, (state.clock)()).await {
            Ok(_) => note = Some(id),
            Err(e) => tracing::warn!(%predecessor, error = ?e, "handoff note not posted"),
        }
    }
    let mut conn = pool.acquire().await.map_err(internal)?;
    aq_machine_control::set_handoff_candidate(&mut conn, predecessor, operation, &oid, note)
        .await
        .map_err(internal)?;
    Ok(Some(HandoffBase {
        oid,
        retention_handle: operation,
        predecessor,
    }))
}
/// Publication fence: the export is the fresh export of this predecessor's
/// recorded handoff candidate (not the predecessor's shared export).
pub async fn export_is_handoff(
    conn: &mut sqlx::PgConnection,
    predecessor: Uuid,
    export: &super::source_exports::Request,
) -> Result<bool, AppError> {
    if !aq_machine_control::has_format(conn)
        .await
        .map_err(internal)?
    {
        return Ok(false);
    }
    let row = aq_machine_control::handoff_of(conn, predecessor)
        .await
        .map_err(internal)?;
    Ok(row.is_some_and(|row| {
        aq_machine_control::is_handoff_export(
            &row,
            &export.base.hex,
            export.source_retention_handle,
        )
    }))
}
