//! Same-origin human terminal operations and node-authenticated registration.
//! Payload rejection deliberately never returns serde's potentially sensitive input.
use super::{persistence::Result, service, service::MutationExt};
use crate::{AppState, authz::CurrentUser, error::AppError};
use aq_terminal_relay::{
    api::{MutationReply, TerminalInfo},
    draft::Register,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::de::DeserializeOwned;
use uuid::Uuid;
struct SafeJson<T>(T);
impl<T: DeserializeOwned> FromRequest<AppState> for SafeJson<T> {
    type Rejection = AppError;
    async fn from_request(req: Request, state: &AppState) -> Result<Self> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| Self(v))
            .map_err(|_| AppError::BadRequest("invalid or oversized terminal JSON".into()))
    }
}
pub fn human_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/v1/terminals/{terminal}/access", post(access))
        .route("/api/v1/terminals/{terminal}/mutations", post(mutate))
        .with_state(state.terminal_view())
        .merge(
            Router::new()
                .route("/api/v1/terminals/{terminal}", get(inspect))
                .route("/api/v1/terminals/{terminal}/output", get(output))
                .route("/api/v1/terminals/{terminal}/tail", get(tail))
                .route(
                    "/api/v1/terminals/{terminal}/receipts/{operation}",
                    get(receipt),
                )
                .with_state(state.terminal_read_view()),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(axum::middleware::map_response(
            move |mut response: axum::response::Response| {
                let clock = state.clock.clone();
                async move {
                    response.headers_mut().insert(
                        axum::http::header::CACHE_CONTROL,
                        "no-store".parse().unwrap(),
                    );
                    response
                        .headers_mut()
                        .entry("aq-terminal-server-time-ms")
                        .or_insert_with(|| {
                            ((clock)().timestamp_millis().max(0) as u64)
                                .to_string()
                                .parse()
                                .unwrap()
                        });
                    response
                }
            },
        ))
}
pub fn worker_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/terminal-relay/register", post(register))
        .route("/v1/terminal-relay/poll", post(poll))
        .route("/v1/terminal-relay/output-poll", post(output_poll))
        .route(
            "/v1/terminal-relay/control-upload",
            post(control_upload).layer(DefaultBodyLimit::max(262144)),
        )
        .with_state(state.terminal_view())
        .merge(
            Router::new()
                .route("/v1/terminal-relay/upload", post(upload))
                .with_state(state.terminal_read_view()),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
async fn inspect(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(terminal): Path<Uuid>,
) -> Result<Json<TerminalInfo>> {
    Ok(Json(service::inspect(&state, user.id, terminal).await?))
}
async fn mutate(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(terminal): Path<Uuid>,
    SafeJson(request): SafeJson<service::Mutation>,
) -> Result<Json<MutationReply>> {
    if request.binding().0.terminal_id != terminal {
        return Err(AppError::BadRequest(
            "terminal path binding mismatch".into(),
        ));
    }
    Ok(Json(service::mutate(&state, user.id, &request).await?))
}
async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    SafeJson(request): SafeJson<Register>,
) -> Result<Json<MutationReply>> {
    let node = super::peer::authenticate(&state, &headers).await?;
    let info = service::register(&state, &node, &request).await?;
    Ok(Json(MutationReply::Registered {
        operation_id: request.operation_id,
        info,
    }))
}

async fn poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    SafeJson(request): SafeJson<aq_terminal_relay::draft::RelayPoll>,
) -> Result<Json<aq_terminal_relay::draft::Envelope>> {
    let node = super::peer::authenticate(&state, &headers)
        .await
        .map_err(|error| relay_failure(error, "terminal_poll_failed"))?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(22),
        super::peer::poll(&state, &node, &request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("terminal poll deadline exceeded".into()))?
    .map_err(|error| relay_failure(error, "terminal_poll_failed"))?;
    Ok(Json(value))
}
async fn output_poll(
    State(state): State<AppState>,
    headers: HeaderMap,
    SafeJson(request): SafeJson<aq_terminal_relay::draft::RelayPoll>,
) -> Result<Json<aq_terminal_relay::draft::Envelope>> {
    let node = super::peer::authenticate(&state, &headers)
        .await
        .map_err(|error| relay_failure(error, "terminal_output_poll_failed"))?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        super::peer::output_poll(&state, &node, &request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("terminal output poll deadline exceeded".into()))?
    .map_err(|error| relay_failure(error, "terminal_output_poll_failed"))?;
    Ok(Json(value))
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    SafeJson(request): SafeJson<aq_terminal_relay::draft::PeerUpload>,
) -> Result<Json<aq_terminal_relay::draft::UploadAck>> {
    let node = super::peer::authenticate(&state, &headers)
        .await
        .map_err(|error| relay_failure(error, "terminal_upload_failed"))?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        super::peer::upload(&state, &node, &request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("terminal upload deadline exceeded".into()))?
    .map_err(|error| relay_failure(error, "terminal_upload_failed"))?;
    Ok(Json(value))
}

async fn output(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(terminal): Path<Uuid>,
    Query(query): Query<aq_terminal_relay::api::ViewQuery>,
) -> Result<Json<aq_terminal_relay::api::OutputReply>> {
    Ok(Json(
        service::output(&state, user.id, terminal, &query).await?,
    ))
}

async fn access(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(terminal): Path<Uuid>,
    SafeJson(request): SafeJson<aq_terminal_relay::api::AccessUpdate>,
) -> Result<Json<MutationReply>> {
    if request.terminal.terminal_id != terminal {
        return Err(AppError::BadRequest(
            "terminal path binding mismatch".into(),
        ));
    }
    Ok(Json(service::set_access(&state, user.id, &request).await?))
}

async fn receipt(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((terminal, operation)): Path<(Uuid, Uuid)>,
) -> Result<Json<aq_session_api::InputReceipt>> {
    Ok(Json(
        service::receipt(&state, user.id, terminal, operation).await?,
    ))
}

async fn control_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    SafeJson(request): SafeJson<aq_terminal_relay::draft::PeerUpload>,
) -> Result<Json<aq_terminal_relay::draft::UploadAck>> {
    let node = super::peer::authenticate(&state, &headers)
        .await
        .map_err(|error| relay_failure(error, "terminal_control_upload_failed"))?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        super::peer::control_upload(&state, &node, &request),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("terminal control upload deadline exceeded".into()))?
    .map_err(|error| relay_failure(error, "terminal_control_upload_failed"))?;
    Ok(Json(value))
}

async fn tail(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(terminal): Path<Uuid>,
    Query(query): Query<aq_terminal_relay::api::ViewQuery>,
) -> Result<(HeaderMap, Json<super::tail::TailReply>)> {
    let reply = super::tail::read(&state, user.id, terminal, &query).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "aq-terminal-server-time-ms",
        reply
            .server_time_ms
            .to_string()
            .parse()
            .map_err(|_| AppError::Internal)?,
    );
    Ok((headers, Json(reply)))
}

/// Keep authorization and domain refusals intact. Internal failures still fail
/// closed, with a fixed endpoint code that is safe to retain in worker evidence.
fn relay_failure(error: AppError, code: &'static str) -> AppError {
    match error {
        AppError::Internal => AppError::TerminalRelayFailure(code),
        other => other,
    }
}
