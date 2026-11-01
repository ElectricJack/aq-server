//! Opt-in transport around the actual history owner; no production route mount.
use super::*;
use aq_phase_terminal_protocol as wire;
use axum::{body::to_bytes, response::IntoResponse};
use sqlx::Row;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Semaphore;
static INGRESS: Semaphore = Semaphore::const_new(2);

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
pub(super) fn routes(state: aq_server::AppState) -> axum::Router {
    axum::Router::new()
        .route("/v1/worker/profiled-v2/history", axum::routing::post(read))
        .layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response
                    .headers_mut()
                    .insert("aq-phase-terminal-api-version", "1".parse().unwrap());
                response.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    "no-store".parse().unwrap(),
                );
                response
            },
        ))
        .with_state(state)
}
async fn read(
    axum::extract::State(state): axum::extract::State<aq_server::AppState>,
    request: axum::extract::Request,
) -> Result<axum::Json<wire::HistoryReceipt>, aq_server::error::AppError> {
    use aq_server::error::AppError;
    let _permit = INGRESS
        .try_acquire()
        .map_err(|_| AppError::Maintenance("history ingress full".into()))?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut versions = request
            .headers()
            .get_all("aq-phase-terminal-api-version")
            .iter();
        if versions.next().is_none_or(|v| v != "1")
            || versions.next().is_some()
            || request
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .is_none_or(|v| v != "application/json")
        {
            return Err(AppError::BadRequest(
                "exact phase history version/type required".into(),
            ));
        }
        let auth = aq_server::worker_api::authenticate_context(&state, request.headers()).await?;
        let body = to_bytes(request.into_body(), wire::MAX_BYTES)
            .await
            .map_err(|_| AppError::PayloadTooLarge("phase history body".into()))?;
        let q = wire::decode_history_request(&body)
            .map_err(|_| AppError::BadRequest("phase history request".into()))?;
        aq_server::profiled_execution::phase_terminal::history::read(&state, &auth, &q)
            .await
            .map(axum::Json)
    })
    .await
    .map_err(|_| AppError::Maintenance("history ingress timed out".into()))?
}

pub(super) async fn verify(
    case: &Case,
    result: &Value,
    acceptance: &aq_admission::preparation_v2::AcceptRequest,
    proposal: &aq_admission::preparation_v2::Proposal,
    accepted: &aq_admission::preparation_v2::AcceptedDelivery,
    snapshot: &Value,
    hot: &Value,
) {
    let q: wire::HistoryRequest =
        serde_json::from_value(result["history_request"].clone()).unwrap();
    let r: wire::HistoryReceipt =
        serde_json::from_value(result["history_receipt"].clone()).unwrap();
    r.validate_for(&q).unwrap();
    let prepare: aq_admission::preparation_v2::PrepareRequest =
        serde_json::from_value(result["prepare_request"].clone()).unwrap();
    assert_eq!(q.binding, prepare.original.binding);
    assert_eq!(q.prepare_operation_id, prepare.original.operation_id);
    assert_eq!(
        q.accepted_receipt_digest,
        accepted.receipt.digest().unwrap()
    );
    let row = sqlx::query("SELECT receipt_bytes,receipt_digest FROM profiled_phase_execution_grants WHERE attempt_id=$1")
        .bind(q.binding.attempt_id).fetch_one(&case.env.env.pool).await.unwrap();
    let execution: aq_admission::preparation_v2::ExecutionReceipt =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("receipt_bytes")).unwrap();
    execution
        .validate_for(&prepare, &accepted.receipt, acceptance, proposal)
        .unwrap();
    let identity = r.identity.as_ref().unwrap();
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
    assert_eq!(hot["status"], "granted");
    let attempt = &snapshot["state"]["attempts"][q.binding.attempt_id.to_string()];
    assert!(attempt["phase_execution"].is_null() && attempt["native_host"].is_null());
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
    eprintln!(
        "actual withheld execution reply: server Granted, worker Prepared only, exact historical identity, no host"
    );
}
