//! Withheld execution delivery and exact history checks through the worker router.
use super::phase::wire as phase_wire;
use super::phase_terminal::wire;
use super::*;
use axum::response::IntoResponse;
use sqlx::Row;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) fn withhold_first_success(routes: axum::Router) -> axum::Router {
    let withheld = Arc::new(AtomicBool::new(false));
    routes.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let withheld = withheld.clone();
            async move {
                let response = next.run(request).await;
                if response.status().is_success() && !withheld.swap(true, Ordering::SeqCst) {
                    // The real owner has already committed. Return an ambiguous
                    // failure instead of its usable execution delivery, once only.
                    axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
                } else {
                    response
                }
            }
        },
    ))
}
pub(super) async fn verify(
    case: &Case,
    result: &Value,
    acceptance: &phase_wire::AcceptRequest,
    proposal: &phase_wire::Proposal,
    accepted: &phase_wire::AcceptedDelivery,
    snapshot: &Value,
    hot: &Value,
) {
    let history_request: wire::HistoryRequest =
        serde_json::from_value(result["history_request"].clone()).unwrap();
    let history_receipt: wire::HistoryReceipt =
        serde_json::from_value(result["history_receipt"].clone()).unwrap();
    history_receipt.validate_for(&history_request).unwrap();
    let prepare: phase_wire::PrepareRequest =
        serde_json::from_value(result["prepare_request"].clone()).unwrap();
    assert_eq!(history_request.binding, prepare.original.binding);
    assert_eq!(
        history_request.prepare_operation_id,
        prepare.original.operation_id
    );
    assert_eq!(
        history_request.accepted_receipt_digest,
        accepted.receipt.digest().unwrap()
    );
    let row = sqlx::query("SELECT receipt_bytes,receipt_digest FROM profiled_phase_execution_grants WHERE attempt_id=$1")
        .bind(history_request.binding.attempt_id).fetch_one(&case.env.env.pool).await.unwrap();
    let execution: phase_wire::ExecutionReceipt =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes")).unwrap();
    execution
        .validate_for(&prepare, &accepted.receipt, acceptance, proposal)
        .unwrap();
    let identity = history_receipt.identity.as_ref().unwrap();
    assert_eq!(
        identity.origin.execution_receipt_digest,
        execution.digest().unwrap()
    );
    assert_eq!(
        identity.origin.execution_receipt_digest.as_str(),
        row.get::<String, _>("receipt_digest")
    );
    assert_eq!(identity.grant_id, execution.grant.grant_id);
    assert_eq!(
        identity.prepared.revision,
        prepare.original.journal_revision
    );
    assert_eq!(identity.prepared.digest, prepare.original.journal_digest);
    verify_prepared_worker_state(result, snapshot, hot, history_request.binding.attempt_id);
    eprintln!(
        "actual withheld execution reply: server Granted, worker Prepared only, exact historical identity, no host"
    );
}

fn verify_prepared_worker_state(
    result: &Value,
    snapshot: &Value,
    hot: &Value,
    attempt_id: uuid::Uuid,
) {
    assert_eq!(hot["status"], "granted");
    let attempt = &snapshot["state"]["attempts"][attempt_id.to_string()];
    assert!(attempt[phase::EXECUTION].is_null() && attempt["native_host"].is_null());
    assert_eq!(result["history_revision"], result["prepared_revision"]);
    assert_eq!(snapshot["state"]["revision"], result["prepared_revision"]);
    for absent in [
        "execution_delivery",
        "execution_record_digest",
        "execution_deadline_ns",
        "execution_revision",
        "host_started",
        "host_stopped",
    ] {
        assert!(
            result.get(absent).is_none(),
            "historical lookup must not fabricate {absent}"
        );
    }
}
