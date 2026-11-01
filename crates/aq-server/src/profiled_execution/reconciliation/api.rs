use super::super::control_api::Error;
use super::*;
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
        .route(
            "/v1/worker/profiled/reconcile",
            post(report).layer(DefaultBodyLimit::max(
                aq_admission::transport::MAX_FRAME_BYTES,
            )),
        )
        .with_state(state.control_view())
}
async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let nonce = super::super::control_api::nonce(&headers)?;
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let bytes =
        body.map_err(|_| AppError::PayloadTooLarge("native reconciliation body bound".into()))?;
    let request: Reconcile = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::BadRequest("invalid native reconciliation document".into()))?;
    let page = reconcile(&state, &auth, &request, nonce).await?;
    let mut response = Json(aq_admission::control::ControlReply::Reconciled(page)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}
