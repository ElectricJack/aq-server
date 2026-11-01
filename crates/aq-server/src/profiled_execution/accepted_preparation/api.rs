use super::*;
use aq_admission::preparation_v2::{self as phase, Document};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::HeaderMap,
    routing::post,
};
pub fn routes(state: AppState) -> Router<AppState> {
    if !policy::enabled(&state) {
        return Router::new();
    }
    Router::new()
        .route("/v1/worker/profiled-v2/proposal", post(proposal))
        .route("/v1/worker/profiled-v2/accept", post(accept_request))
        .layer(DefaultBodyLimit::max(phase::MAX_BYTES))
        .with_state(state.control_view())
}
async fn proposal(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<ProposalRequest>,
) -> Result<Json<Document>, super::super::control_api::Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(Json(Document::Proposal(Box::new(
        propose(&state, &auth, &request).await?,
    ))))
}
async fn accept_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Document>, super::super::control_api::Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let nonce = super::super::control_api::nonce(&headers)?;
    let Document::Accept(q) = phase::decode(&body)
        .map_err(|_| AppError::BadRequest("invalid phase accept document".into()))?
    else {
        return Err(AppError::BadRequest("expected phase accept document".into()).into());
    };
    Ok(Json(Document::AcceptedDelivery(Box::new(
        accept(&state, &auth, &q, nonce).await?,
    ))))
}
