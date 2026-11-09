//! Authenticated execution extensions. Foundation v1 routes retain their shape.
use super::{AttemptState, Error};
use crate::{
    AppState,
    authz::{CurrentUser, RequireOwner},
    config::AuthConfig,
    error::AppError,
};
use aq_worker_protocol::*;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequest, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            Self::Refused(code, message) => (code, message),
            Self::Database(e) if aq_machine_control::is_contention(&e) => (
                ErrorCode::Overloaded,
                "row lock contended; retry the same operation",
            ),
            other => {
                tracing::error!(error=%other, "execution storage failure");
                (ErrorCode::Internal, "execution storage failure")
            }
        };
        let status = match code {
            ErrorCode::InvalidCredential | ErrorCode::CredentialRevoked => StatusCode::UNAUTHORIZED,
            ErrorCode::UnsupportedProtocol => StatusCode::UPGRADE_REQUIRED,
            ErrorCode::InvalidRequest | ErrorCode::UnsupportedCapability => StatusCode::BAD_REQUEST,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::Overloaded => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        };
        let response = (
            status,
            Json(ProtocolError {
                code,
                message: message.into(),
                retryable: matches!(code, ErrorCode::Overloaded | ErrorCode::Internal),
                operation_id: None,
            }),
        );
        let retry = (code == ErrorCode::Overloaded).then_some([(header::RETRY_AFTER, "1")]);
        (retry, response).into_response()
    }
}

struct ExecutionJson<T>(T);
impl<T: DeserializeOwned> FromRequest<AppState> for ExecutionJson<T> {
    type Rejection = Error;
    async fn from_request(req: Request, state: &AppState) -> Result<Self, Error> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| Self(v))
            .map_err(|_| {
                Error::Refused(
                    ErrorCode::InvalidRequest,
                    "invalid or oversized execution JSON",
                )
            })
    }
}
async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    identity: &NodeIdentity,
) -> Result<(), Error> {
    let node = crate::worker_api::authenticate(state, headers)
        .await
        .map_err(|e| match e {
            AppError::CredentialRevoked => {
                Error::Refused(ErrorCode::CredentialRevoked, "node credential revoked")
            }
            AppError::Internal => Error::Refused(ErrorCode::Internal, "authentication unavailable"),
            _ => Error::Refused(ErrorCode::InvalidCredential, "invalid node credential"),
        })?;
    if node.id != identity.node_id {
        return Err(Error::Refused(
            ErrorCode::InvalidCredential,
            "credential does not name this node",
        ));
    }
    Ok(())
}
pub(crate) fn now(state: &AppState) -> u64 {
    let now = (state.clock)().timestamp_millis().max(0) as u64;
    #[cfg(feature = "recovery-test-support")]
    if matches!(state.config.auth, AuthConfig::Dev { .. }) {
        return now.saturating_add(state.store.recovery_test.offset_ms());
    }
    now
}

fn admission_clock(state: &AppState) -> super::AdmissionClock {
    let state = state.clone();
    super::AdmissionClock::Live(std::sync::Arc::new(move || now(&state)))
}

pub fn worker_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v2/worker/register", post(register_dispatch))
        .route("/v2/worker/accept", post(accept))
        .route("/v2/worker/artifact-handoff", post(artifact_handoff))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/v2/worker/poll", post(poll))
                .route("/v2/worker/decline", post(decline))
                .route("/v2/worker/renew", post(renew))
                .route("/v2/worker/observe", post(observe))
                .route("/v2/worker/directive-ack", post(directive_ack))
                .route("/v2/worker/result", post(result))
                .route("/v2/worker/reconcile", post(reconcile))
                .with_state(state.control_view()),
        )
        .layer(DefaultBodyLimit::max(1024 * 1024))
}
pub fn admin_routes(state: AppState) -> Router<AppState> {
    let routes = Router::new()
        .route("/api/execution/fixtures", post(fixture))
        .route("/api/execution/attempts/{id}", get(inspect))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/api/execution/attempts/{id}/cancel", post(cancel))
                .route(
                    "/api/execution/replace-incarnation",
                    post(replace_incarnation),
                )
                .with_state(state.control_view()),
        );
    #[cfg(feature = "recovery-test-support")]
    let routes = routes.merge(super::testing::routes(state));
    routes.layer(DefaultBodyLimit::max(1024 * 1024))
}
// The published sandbox registration shares the fixture route. Probe only the
// envelope discriminator; decode the original bytes again with the exact type
// so duplicate wire fields cannot be erased by a Value round-trip.
async fn register_dispatch(State(state): State<AppState>, request: Request) -> Response {
    let headers = request.headers().clone();
    let (parts, body) = request.into_parts();
    let invalid = || {
        Error::Refused(
            ErrorCode::InvalidRequest,
            "invalid or oversized execution JSON",
        )
        .into_response()
    };
    let Ok(bytes) = axum::body::to_bytes(body, 1024 * 1024).await else {
        return invalid();
    };
    let Ok(probe) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return invalid();
    };
    let sandbox = probe.get("kind").is_some();
    if sandbox && bytes.len() > 32_768 {
        return AppError::PayloadTooLarge("sandbox registration body bound".into()).into_response();
    }
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    if sandbox {
        match Json::<aq_admission_sandbox::sandbox::SandboxDocument>::from_request(request, &state)
            .await
        {
            Ok(document) => crate::delegated_permits::incarnation::register(
                State(state.control_view()),
                headers,
                document,
            )
            .await
            .unwrap_or_else(IntoResponse::into_response),
            Err(error) => error.into_response(),
        }
    } else {
        match ExecutionJson::<RegisterRequest>::from_request(request, &state).await {
            Ok(document) => register(State(state), headers, document)
                .await
                .map(IntoResponse::into_response)
                .unwrap_or_else(IntoResponse::into_response),
            Err(error) => error.into_response(),
        }
    }
}

async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    ExecutionJson(request): ExecutionJson<RegisterRequest>,
) -> Result<Json<RegisterResponse>, Error> {
    if request.protocol_version != EXECUTION_PROTOCOL_VERSION {
        return Err(Error::Refused(
            ErrorCode::UnsupportedProtocol,
            "execution protocol 2 required",
        ));
    }
    authenticate(&state, &headers, &request.identity).await?;
    Ok(Json(state.store.execution_register(&request).await?))
}
macro_rules! timed_handler {
    ($name:ident, $request:ty, $response:ty, $method:ident) => {
        async fn $name(
            State(state): State<AppState>,
            headers: HeaderMap,
            ExecutionJson(request): ExecutionJson<$request>,
        ) -> Result<Json<$response>, Error> {
            authenticate(&state, &headers, &request.identity).await?;
            Ok(Json(
                state
                    .store
                    .$method(&request, admission_clock(&state))
                    .await?,
            ))
        }
    };
}
macro_rules! handler {
    ($name:ident, $request:ty, $response:ty, $method:ident) => {
        async fn $name(
            State(state): State<AppState>,
            headers: HeaderMap,
            ExecutionJson(request): ExecutionJson<$request>,
        ) -> Result<Json<$response>, Error> {
            authenticate(&state, &headers, &request.identity).await?;
            Ok(Json(state.store.$method(&request).await?))
        }
    };
}
timed_handler!(poll, PollRequest, PollResponse, execution_poll);
timed_handler!(accept, AcceptRequest, AcceptResponse, execution_accept);
timed_handler!(renew, RenewRequest, RenewResponse, execution_renew);
timed_handler!(result, ResultReport, ResultResponse, execution_result);
timed_handler!(
    reconcile,
    ReconcileRequest,
    ReconcileResponse,
    execution_reconcile
);
handler!(decline, DeclineRequest, Ack, execution_decline);
handler!(observe, Observation, Ack, execution_observe);
handler!(directive_ack, DirectiveAck, Ack, execution_directive_ack);

async fn artifact_handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    ExecutionJson(request): ExecutionJson<ArtifactHandoffRequest>,
) -> Result<Json<ArtifactHandoffResponse>, Error> {
    authenticate(&state, &headers, &request.identity).await?;
    Err(Error::Refused(
        ErrorCode::UnsupportedCapability,
        "artifact transport/retention adapter not yet installed",
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRequest {
    pub task_id: Uuid,
    pub offer: Offer,
}
async fn fixture(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    ExecutionJson(request): ExecutionJson<FixtureRequest>,
) -> Result<Json<Offer>, Error> {
    if !matches!(state.config.auth, AuthConfig::Dev { .. }) {
        return Err(Error::Refused(
            ErrorCode::UnsupportedCapability,
            "synthetic assignments are development-only",
        ));
    }
    if request.offer.offer_expires_at_ms <= now(&state)
        || request.offer.offer_expires_at_ms > now(&state).saturating_add(MAX_GRANT_MS)
    {
        return Err(Error::Refused(
            ErrorCode::InvalidRequest,
            "fixture offer expiry must be within 120 seconds",
        ));
    }
    Ok(Json(
        state
            .store
            .execution_offer(request.task_id, &request.offer)
            .await?,
    ))
}
async fn inspect(
    State(state): State<AppState>,
    CurrentUser(_): CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<AttemptState>, Error> {
    Ok(Json(state.store.execution_get(id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    pub operation_id: Uuid,
    pub reason: String,
}
async fn cancel(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(id): Path<Uuid>,
    ExecutionJson(request): ExecutionJson<CancelRequest>,
) -> Response {
    let kind =
        sqlx::query_scalar::<_, String>("SELECT kind FROM execution_attempts WHERE attempt_id=$1")
            .bind(id)
            .fetch_optional(&state.store.pool)
            .await;
    match kind {
        Ok(Some(kind)) if kind == "profiled_v1" => crate::profiled_execution::cancellation::cancel(
            &state,
            id,
            request.operation_id,
            &request.reason,
        )
        .await
        .map(Json)
        .into_response(),
        Ok(_) => state
            .store
            .execution_cancel(id, request.operation_id, &request.reason)
            .await
            .map(Json)
            .into_response(),
        Err(_) => AppError::Internal.into_response(),
    }
}

async fn replace_incarnation(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    ExecutionJson(request): ExecutionJson<RegisterRequest>,
) -> Result<Json<RegisterResponse>, Error> {
    Ok(Json(
        state.store.execution_replace_incarnation(&request).await?,
    ))
}
