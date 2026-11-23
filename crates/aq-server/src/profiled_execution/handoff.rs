//! Spec 0007 phase 1: the successor of a handed-off attempt (an
//! `attempt_handoffs` row) starts from the predecessor's saved work. The saved
//! checkpoint becomes an aq-git candidate on the predecessor's admitted base;
//! the successor gets a fresh export of that commit, never the predecessor's
//! export. The handoff note goes on the task thread once.
use super::{replacement::Replacement, results::RecordedResult};
use crate::{
    AppState,
    delivery::{
        CandidateRequest, CandidateState, CommitMetadata, DeliveryService, OperationContext,
    },
    error::AppError,
};
use aq_machine_control::HandoffRow;
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
    let Some(row) = predecessor_handoff(state, predecessor).await? else {
        return Ok(None);
    };
    let Some(delivery) = state.delivery() else {
        warn_missing_delivery(predecessor);
        return Ok(None);
    };
    let Some(saved) = saved_source(state, task, predecessor).await? else {
        return Ok(None);
    };
    let operation = candidate_operation(predecessor);
    let request = saved
        .candidate_request(&delivery, predecessor, operation)
        .await?;
    let Some(oid) = ready_candidate(&delivery, request, predecessor).await else {
        return Ok(None);
    };
    let note = handoff_note(state, &saved.record, predecessor, row).await;
    let mut conn = state.store.pool.acquire().await.map_err(internal)?;
    aq_machine_control::set_handoff_candidate(
        &mut conn,
        predecessor,
        aq_machine_control::HandoffCandidate {
            operation,
            oid: &oid,
            note_comment: note,
        },
    )
    .await?;
    Ok(Some(HandoffBase {
        oid,
        retention_handle: operation,
        predecessor,
    }))
}

async fn predecessor_handoff(
    state: &AppState,
    predecessor: Uuid,
) -> Result<Option<HandoffRow>, AppError> {
    let mut conn = state.store.pool.acquire().await.map_err(internal)?;
    if !aq_machine_control::has_format(&mut conn).await? {
        return Ok(None);
    }
    // A grant-backed handoff leaves its saved work on the task branch.
    if super::has_delivery_grant(&mut *conn, predecessor).await? {
        return Ok(None);
    }
    Ok(aq_machine_control::handoff_of(&mut conn, predecessor).await?)
}

fn warn_missing_delivery(predecessor: Uuid) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(%predecessor, "handoff successor needs aq-git delivery; using the original base");
    }
}

struct SavedSource {
    record: RecordedResult,
    source: Uuid,
}

async fn saved_source(
    state: &AppState,
    task: Uuid,
    predecessor: Uuid,
) -> Result<Option<SavedSource>, AppError> {
    let pool = &state.store.pool;
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
    let record = super::results::load_locked(state, &mut tx, report).await?;
    let mut source = None;
    for op in &record.report.source_receipt_ids {
        match crate::source_retention::load_confirmed_locked(&mut tx, *op, &record).await {
            Ok(_) => {
                source = Some(*op);
                break;
            }
            Err(AppError::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
    }
    tx.rollback().await.map_err(internal)?;
    let (Some(source), true) = (source, record.spec.task.task_id == task) else {
        tracing::warn!(%predecessor, "handoff has no confirmed saved source; using the original base");
        return Ok(None);
    };
    Ok(Some(SavedSource { record, source }))
}

impl SavedSource {
    async fn candidate_request(
        &self,
        delivery: &DeliveryService,
        predecessor: Uuid,
        operation: Uuid,
    ) -> Result<CandidateRequest, AppError> {
        let admitted = &self.record.spec.source;
        let task = self.record.spec.task.task_id;
        // A retried poll replays the exact inputs frozen with the first intent.
        let (base, sources, commit) = match delivery.frozen_inputs(operation).await.ok().flatten() {
            Some(inputs) => (
                serde_json::from_value(inputs["base"].clone()).map_err(internal)?,
                serde_json::from_value(inputs["source_receipts"].clone()).map_err(internal)?,
                serde_json::from_value(inputs["commit"].clone()).map_err(internal)?,
            ),
            None => (
                ObjectId {
                    algorithm: ObjectAlgorithm::Sha1,
                    hex: admitted.resolved_object.hex.clone(),
                },
                vec![self.source],
                CommitMetadata {
                    name: "AQ".into(),
                    email: "aq@aq.invalid".into(),
                    message: aq_machine_control::handoff_commit_message(
                        task,
                        predecessor,
                        self.record.report.conclusion.as_str(),
                    ),
                },
            ),
        };
        Ok(CandidateRequest {
            context: OperationContext {
                operation_id: operation,
                repository_id: admitted.repository_id,
                binding_generation: admitted.repository_binding_generation,
            },
            base,
            source_receipts: sources,
            commit,
        })
    }
}

async fn ready_candidate(
    delivery: &DeliveryService,
    request: CandidateRequest,
    predecessor: Uuid,
) -> Option<String> {
    let operation = request.context.operation_id;
    let outcome = delivery.create_candidate(request).await.map(|r| r.outcome);
    let ready = match &outcome {
        Ok(Ok(c)) if c.state == CandidateState::Ready && c.retention_handle == operation => {
            c.candidate.as_ref().map(|o| o.hex.clone())
        }
        _ => None,
    };
    if ready.is_none() {
        let outcome = outcome.map(|o| o.map(|c| (c.state, c.conflict_step)));
        tracing::warn!(%predecessor, ?outcome, "handoff candidate not ready; using the original base");
    }
    ready
}

async fn handoff_note(
    state: &AppState,
    record: &RecordedResult,
    predecessor: Uuid,
    row: HandoffRow,
) -> Option<Uuid> {
    // The conclusion is the worker's text, so the predecessor agent authors it.
    use aq_session_protocol::SessionIdentity::Remote;
    let (None, Remote { session_id, .. }) = (row.note_comment_id, &record.report.binding.session)
    else {
        return row.note_comment_id;
    };
    let id = aq_machine_control::handoff_note_operation(predecessor);
    let body = aq_machine_control::handoff_note(
        predecessor,
        row.reason,
        record.report.conclusion.as_str(),
    );
    let thread = Thread {
        project_id: record.spec.source.project_id,
        task_id: record.spec.task.task_id,
    };
    let author = Author::Agent {
        session_id: *session_id,
        attempt_id: predecessor,
    };
    let pool = &state.store.pool;
    match aq_task_comments::add(pool, id, thread, &author, &body, (state.clock)()).await {
        Ok(_) => Some(id),
        Err(e) => {
            tracing::warn!(%predecessor, error = ?e, "handoff note not posted");
            None
        }
    }
}

/// Publication fence: the export is the fresh export of this predecessor's
/// recorded handoff candidate (not the predecessor's shared export).
pub async fn export_is_handoff(
    conn: &mut sqlx::PgConnection,
    predecessor: Uuid,
    export: &super::source_exports::Request,
) -> Result<bool, AppError> {
    if !aq_machine_control::has_format(conn).await? {
        return Ok(false);
    }
    let row = aq_machine_control::handoff_of(conn, predecessor).await?;
    Ok(row.is_some_and(|row| {
        aq_machine_control::is_handoff_export(
            &row,
            &export.base.hex,
            export.source_retention_handle,
        )
    }))
}
