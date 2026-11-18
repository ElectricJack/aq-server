//! Raw versioned native poll, only on the machine listener.
use crate::{AppState, error::AppError};
use aq_admission::{control::ControlReply, transport::Poll};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::post,
};
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled/poll", post(poll))
        .layer(DefaultBodyLimit::max(16_384))
        .with_state(state.control_view())
}
async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, super::super::control_api::Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("native poll body bound".into()))?;
    let request: Poll = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("invalid native poll document".into()))?;
    let slot = super::super::api::slot_header(&headers, aq_admission::node_slots::SLOT_HEADER)?
        .unwrap_or(1);
    let reply = super::workflow::poll_slot(&state, &auth, &request, slot).await?;
    #[cfg(feature = "delivery_playbook")]
    let reply = super::super::git_grant::deliver(&state, auth.id(), reply)?;
    let value = ControlReply::Poll(reply);
    if serde_json::to_vec(&value)
        .map_err(|_| AppError::Internal)?
        .len()
        > 4_194_304
    {
        return Err(AppError::PayloadTooLarge("native poll reply bound".into()).into());
    }
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}
