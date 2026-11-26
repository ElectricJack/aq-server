//! Current-machine renewal on a bounded lane independent of bulk preparation.
use super::super::control_api::{Error, nonce};
use crate::{
    AppState,
    delegated_permits::{incarnation, runtime},
    error::AppError,
    worker_api::AuthenticatedNode,
};
use aq_admission::{control::ControlReply, transport::Renew};
use aq_admission_sandbox::sandbox::{
    GrantPhase, PermitContext, PhaseProfile, RunnerOperation, RunnerRenewalRequest,
    RunnerRenewalResponse, SandboxDocument,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::post,
};
use uuid::Uuid;
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/v1/worker/profiled/renew",
            post(renew).layer(DefaultBodyLimit::max(16_384)),
        )
        .with_state(state.renewal_view())
}
async fn renew(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let nonce = nonce(&headers)?;
    let auth = runtime::authenticate_runner(&state, &headers, RunnerOperation::Renew, None).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("renewal body bound".into()))?;
    let mut response = if auth.delegated().is_some() {
        renew_delegated(&state, &auth, &body, nonce).await?
    } else {
        let request: Renew = serde_json::from_slice(&body)
            .map_err(|_| AppError::BadRequest("invalid renewal document".into()))?;
        let reply = super::service::renew(&state, &auth, &request, nonce).await?;
        Json(ControlReply::Renewed(reply)).into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

async fn renew_delegated(
    state: &AppState,
    auth: &AuthenticatedNode,
    body: &[u8],
    nonce: Uuid,
) -> Result<Response, AppError> {
    let SandboxDocument::RunnerRenewalRequest(request) = serde_json::from_slice(body)
        .map_err(|_| AppError::BadRequest("invalid delegated renewal document".into()))?
    else {
        return Err(AppError::BadRequest(
            "expected runner renewal request".into(),
        ));
    };
    runtime::require_attempts(auth, [request.permit.binding.attempt_id])?;
    runtime::require_binding(auth, &request.authority.grant.binding)?;
    let reply = delegated_reply(state, auth, &request, nonce).await?;
    // The sibling headers carry the committed operation's pre-mutation
    // context for B6a2; the owner response body remains unchanged. On exact
    // replay this is the original request context, not a fresh lease.
    let context = PermitContext {
        binding: request.permit.binding.clone(),
        phase: GrantPhase::Granted,
        grant_expires_at_ms: request.permit.expires_at_ms,
        hard_deadline_ms: request.permit.hard_deadline_ms,
    };
    let server_now = match &reply.delivery {
        Some(delivery) => delivery.clock.server_time_ms,
        None => runtime::now(state)?,
    };
    let mut response = Json(SandboxDocument::RunnerRenewalResponse(reply)).into_response();
    let context = serde_json::to_string(&context).map_err(|_| AppError::Internal)?;
    let headers = response.headers_mut();
    headers.insert("aq-permit-context", header_value(context)?);
    headers.insert("aq-server-now-ms", header_value(server_now.to_string())?);
    Ok(response)
}
fn header_value(text: String) -> Result<HeaderValue, AppError> {
    text.parse().map_err(|_| AppError::Internal)
}
/// A rebooted runner's replay wins; otherwise the permit's phase profile renews.
async fn delegated_reply(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &RunnerRenewalRequest,
    nonce: Uuid,
) -> Result<RunnerRenewalResponse, AppError> {
    if let Some(reply) = incarnation::renew_after_reboot(state, auth, request, nonce).await? {
        return Ok(reply);
    }
    match request.authority.profile {
        PhaseProfile::PhaseV2 => {
            crate::profiled_execution::phase_execution::renew_delegated(state, auth, request, nonce)
                .await
        }
        PhaseProfile::PhaseRefreshV1 => {
            crate::profiled_execution::phase_refresh_execution::renew_delegated(
                state, auth, request, nonce,
            )
            .await
        }
    }
}
