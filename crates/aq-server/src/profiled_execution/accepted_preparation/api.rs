use super::*;
use aq_admission::preparation_v2::{self as phase, Document};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, FromRequest, Request, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
pub fn routes(state: AppState) -> Router<AppState> {
    if !policy::enabled(&state) && !super::super::accepted_refresh::enabled(&state) {
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
    request: Request,
) -> Result<Response, super::super::control_api::Error> {
    // Preserve the existing Json extractor's media-type/syntax behavior while
    // retaining the exact bytes for the final closed typed profile decoder.
    // A Value used only as a header probe may not erase duplicate wire fields.
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, phase::MAX_BYTES)
        .await
        .map_err(|_| AppError::PayloadTooLarge("phase proposal body bound".into()))?;
    let probe = Request::from_parts(parts, axum::body::Body::from(bytes.clone()));
    let _ = crate::admin_api::ApiJson::<serde_json::Value>::from_request(probe, &state).await?;
    let request = super::super::phase_profile::decode_proposal(&bytes)?;
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(match request {
        super::super::phase_profile::ProposalRequest::Frozen(request) => Json(Document::Proposal(
            Box::new(propose(&state, &auth, &request).await?),
        ))
        .into_response(),
        super::super::phase_profile::ProposalRequest::Refresh(request) => {
            Json(aq_admission::preparation_refresh_v1::Document::Proposal(
                Box::new(super::super::accepted_refresh::propose(&state, &auth, &request).await?),
            ))
            .into_response()
        }
    })
}
async fn accept_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, super::super::control_api::Error> {
    let mut auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    auth.issuance = crate::delegated_permits::handoff::header(&headers, "aq-permit-issuance")?;
    let nonce = super::super::control_api::nonce(&headers)?;
    let mut response = match super::super::phase_profile::decode_accept(&body)? {
        super::super::phase_profile::AcceptRequest::Frozen(q) => Json(Document::AcceptedDelivery(
            Box::new(accept(&state, &auth, &q, nonce).await?),
        ))
        .into_response(),
        super::super::phase_profile::AcceptRequest::Refresh(q) => Json(
            aq_admission::preparation_refresh_v1::Document::AcceptedDelivery(Box::new(
                super::super::accepted_refresh::accept(&state, &auth, &q, nonce).await?,
            )),
        )
        .into_response(),
    };
    crate::delegated_permits::handoff::response_header(&state, &auth, &mut response).await?;
    Ok(response)
}
