//! Bounded authenticated dashboard reads. No action proxy, no browser authority.
pub mod contract;
mod runtime;
mod service;
use crate::{AppState, authz::CurrentUser, error::AppError};
use axum::{
    Extension, Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use contract::*;
use runtime::Runtime;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
type Result<T> = std::result::Result<T, Error>;
/// Facts captured by the actual protected human router, never inferred from files.
#[derive(Clone, Default)]
pub(crate) struct MountedOwners {
    pub(crate) graph: bool,
    pub(crate) session_usage: bool,
    pub(crate) session_events: bool,
    pub(crate) results: bool,
    pub(crate) media_read: bool,
    pub(crate) media_upload: bool,
    pub(crate) notifications: bool,
    pub(crate) supervisor_conversation: bool,
    pub(crate) reviews: bool,
    pub(crate) playbook_owner: Option<Uuid>,
    pub(crate) playbook_loaded_definitions: bool,
}

#[derive(Debug)]
enum Error {
    Api(AppError),
    Invalid,
    Restart,
    AuthorizationChanged,
    Unavailable,
}
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
    }
}
impl From<AppError> for Error {
    fn from(e: AppError) -> Self {
        Self::Api(e)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message, retryable) = match self {
            Self::Invalid => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid dashboard request",
                false,
            ),
            Self::Restart => (
                StatusCode::CONFLICT,
                "cursor_restart_required",
                "This page expired; refresh the list",
                false,
            ),
            Self::AuthorizationChanged => (
                StatusCode::FORBIDDEN,
                "authorization_changed",
                "Your access changed; reconnect to AQ",
                false,
            ),
            Self::Api(AppError::Unauthenticated(_)) => (
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "Sign in to AQ",
                false,
            ),
            Self::Api(AppError::Forbidden(_)) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "This read is not available to your account",
                false,
            ),
            Self::Api(AppError::NotFound(_)) => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Session not found",
                false,
            ),
            Self::Api(AppError::BadRequest(_)) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid dashboard request",
                false,
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "read_unavailable",
                "This read is temporarily unavailable",
                true,
            ),
        };
        (status,Json(serde_json::json!({"request_id":Uuid::new_v4(),"code":code,"message":message,"retryable":retryable}))).into_response()
    }
}
struct DashboardUser(crate::store::User);
impl FromRequestParts<AppState> for DashboardUser {
    type Rejection = Error;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self> {
        CurrentUser::from_request_parts(parts, state)
            .await
            .map(|CurrentUser(user)| Self(user))
            .map_err(Error::from)
    }
}
struct DashboardQuery(ReadQuery);
impl FromRequestParts<AppState> for DashboardQuery {
    type Rejection = Error;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self> {
        Query::<ReadQuery>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|_| Error::Invalid)
    }
}
struct SessionPath(Uuid);
impl FromRequestParts<AppState> for SessionPath {
    type Rejection = Error;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self> {
        Path::<Uuid>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|_| Error::Invalid)
    }
}
#[derive(Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    scope: Option<String>,
    limit: Option<u16>,
    cursor: Option<String>,
}
impl ReadQuery {
    fn scope(&self) -> Result<Scope> {
        match self.scope.as_deref() {
            None | Some("all_authorized") => Ok(Scope::AllAuthorized),
            Some(value) => {
                let id = value
                    .strip_prefix("project:")
                    .and_then(|s| s.parse::<Uuid>().ok())
                    .filter(|v| !v.is_nil())
                    .ok_or(Error::Invalid)?;
                Ok(Scope::Project { project_id: id })
            }
        }
    }
    fn limit(&self) -> Result<u16> {
        let n = self.limit.unwrap_or(runtime::PAGE);
        if n == 0 || n > runtime::MAX_PAGE {
            Err(Error::Invalid)
        } else {
            Ok(n)
        }
    }
}
/// Runtime is created once per human router and shared by its clones. A
/// dedicated two-connection pool cannot exhaust execution-control connections.
pub fn routes(state: AppState) -> Router<AppState> {
    let runtime = Arc::new(Runtime::new(&state));
    Router::new()
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/sessions", get(sessions))
        .route("/api/v1/sessions/{id}", get(detail))
        .route("/api/v1/projects", get(projects))
        .layer(Extension(runtime))
}
fn bounded<T: serde::Serialize>(value: T) -> Result<Json<T>> {
    if serde_json::to_vec(&value)
        .map_err(|_| Error::Unavailable)?
        .len()
        > 1_048_576
    {
        return Err(Error::Unavailable);
    }
    Ok(Json(value))
}
async fn bootstrap(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    assets: Option<Extension<Arc<crate::dashboard_assets::DashboardAssets>>>,
    owners: Option<Extension<MountedOwners>>,
) -> Result<Json<Envelope<Bootstrap>>> {
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    let future = service::bootstrap(
        &state,
        &runtime,
        &user,
        assets.as_ref().map(|v| format!("sha256:{}", v.0.digest())),
        owners.map(|Extension(v)| v).unwrap_or_default(),
    );
    bounded(
        tokio::time::timeout(Duration::from_secs(2), future)
            .await
            .map_err(|_| Error::Unavailable)??,
    )
}
async fn overview(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Overview>>> {
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    let scope = query.scope()?;
    bounded(
        tokio::time::timeout(
            Duration::from_secs(2),
            service::overview(&state, &runtime, &user, scope),
        )
        .await
        .map_err(|_| Error::Unavailable)??,
    )
}
async fn sessions(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Page<SessionSummary>>>> {
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    bounded(
        tokio::time::timeout(
            Duration::from_secs(3),
            service::sessions(&state, &runtime, &user, query),
        )
        .await
        .map_err(|_| Error::Unavailable)??,
    )
}
async fn detail(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    SessionPath(id): SessionPath,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<SessionDetail>>> {
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    bounded(
        tokio::time::timeout(
            Duration::from_secs(2),
            service::detail(&state, &runtime, &user, id, query.scope()?),
        )
        .await
        .map_err(|_| Error::Unavailable)??,
    )
}
async fn projects(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(runtime): Extension<Arc<Runtime>>,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Page<ProjectRef>>>> {
    let _permit = runtime.gate.try_acquire().map_err(|_| Error::Unavailable)?;
    bounded(
        tokio::time::timeout(
            Duration::from_secs(2),
            service::projects(&state, &runtime, &user, query),
        )
        .await
        .map_err(|_| Error::Unavailable)??,
    )
}
