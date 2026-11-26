//! Bounded authenticated dashboard reads. No action proxy, no browser authority.
pub mod actions;
mod aggregation;
mod attention;
mod capacity;
pub(crate) mod performance;
pub mod contract {
    pub use aq_dashboard_contract::*;
}
pub use aq_human_cursor as cursor;
pub mod errors;
pub mod quota;
mod runtime;
mod search;
mod service;
mod swarm;
use crate::{AppState, authz::CurrentUser, error::AppError};
use axum::{
    Extension, Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
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
    /// H4 owner waits inspection (`/api/waits`).
    pub(crate) waits: bool,
    /// G10 owner operations read (`/api/v1/sessions/{id}/operations`).
    pub(crate) session_operations: bool,
    /// F5 owner question inbox (`/api/questions/decisions`).
    pub(crate) questions: bool,
    pub(crate) playbook_owner: Option<Uuid>,
    pub(crate) playbook_decisions: bool,
    pub(crate) playbook_loaded_definitions: bool,
    pub(crate) playbook_catalog: Option<aq_playbook_host::CatalogSupport>,
}

#[derive(Debug)]
enum Error {
    Api(AppError),
    Invalid,
    Restart,
    AuthorizationChanged,
    Unavailable,
    /// The entry needs its own controls; it cannot be cleared.
    NotDismissible,
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
        use errors::ApiError;
        match self {
            Self::Invalid => ApiError::invalid(),
            Self::Restart => ApiError::new(
                StatusCode::GONE,
                "cursor_restart_required",
                "Restart this list; preserve your reading anchor",
            ),
            Self::AuthorizationChanged => ApiError::new(
                StatusCode::FORBIDDEN,
                "authorization_changed",
                "Your access changed; reconnect to AQ",
            ),
            Self::Api(error) => ApiError::from(error),
            Self::Unavailable => ApiError::unavailable(),
            Self::NotDismissible => ApiError::new(
                StatusCode::CONFLICT,
                "attention_not_dismissible",
                "This entry must be resolved through its own controls; it cannot be cleared",
            ),
        }
        .into_response()
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
struct DashboardRequest {
    state: AppState,
    user: crate::store::User,
    runtime: Arc<Runtime>,
}

impl FromRequestParts<AppState> for DashboardRequest {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> std::result::Result<Self, Response> {
        let (State(state), DashboardUser(user), Extension(runtime)) =
            <(State<AppState>, DashboardUser, Extension<Arc<Runtime>>)>::from_request_parts(
                parts, state,
            )
            .await?;
        Ok(Self {
            state,
            user,
            runtime,
        })
    }
}

impl DashboardRequest {
    fn parts(&self) -> (&AppState, &Runtime, &crate::store::User) {
        (&self.state, &self.runtime, &self.user)
    }

    async fn unpaged<'a, T, F>(
        &'a self,
        query: ReadQuery,
        read: impl FnOnce(&'a Self, Scope) -> F,
    ) -> Result<Json<T>>
    where
        T: serde::Serialize,
        F: std::future::Future<Output = Result<T>>,
    {
        self.reply(4, || Ok(read(self, query.unpaged_scope()?)))
            .await
    }

    async fn reply<T, F>(&self, seconds: u64, read: impl FnOnce() -> Result<F>) -> Result<Json<T>>
    where
        T: serde::Serialize,
        F: std::future::Future<Output = Result<T>>,
    {
        let _permit = self
            .runtime
            .gate
            .try_acquire()
            .map_err(|_| Error::Unavailable)?;
        let future = read()?;
        let result = tokio::time::timeout(Duration::from_secs(seconds), future)
            .await
            .map_err(|_| Error::Unavailable)??;
        bounded(result)
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
    fn unpaged_scope(&self) -> Result<Scope> {
        let scope = self.scope()?;
        if self.limit.is_some() || self.cursor.is_some() {
            return Err(Error::Invalid);
        }
        Ok(scope)
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
    routes_with_reviews(state, None, None)
}
pub(crate) fn routes_with_reviews(
    state: AppState,
    reviews: Option<Arc<crate::reviews::Service>>,
    playbooks: Option<Arc<crate::playbooks::driver::Driver>>,
) -> Router<AppState> {
    let mut runtime = Runtime::new(&state);
    runtime.reviews = reviews.clone();
    let runtime = Arc::new(runtime);
    Router::new()
        .merge(actions::routes(Arc::new(actions::existing::registry(
            &state, reviews, playbooks,
        ))))
        .merge(performance::routes())
        .merge(attention::routes())
        .route("/api/v1/bootstrap", get(bootstrap))
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/search", get(search::read))
        .route("/api/v1/provider-usage", get(provider_usage))
        .route("/api/v1/capacity", get(capacity))
        .route("/api/v1/swarm", get(swarm))
        .route("/api/v1/sessions", get(sessions))
        .route("/api/v1/sessions/{id}", get(detail))
        .route("/api/v1/sessions/{id}/operations", get(operations))
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
    request: DashboardRequest,
    assets: Option<Extension<Arc<crate::dashboard_assets::DashboardAssets>>>,
    owners: Option<Extension<MountedOwners>>,
) -> Result<Json<Envelope<Bootstrap>>> {
    request
        .reply(2, || {
            Ok(service::bootstrap(
                &request,
                assets.as_ref().map(|v| format!("sha256:{}", v.0.digest())),
                owners.map(|Extension(v)| v).unwrap_or_default(),
            ))
        })
        .await
}

async fn overview(
    owners: Option<Extension<MountedOwners>>,
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Overview>>> {
    request
        .reply(2, || {
            Ok(aggregation::overview(
                &request,
                query.scope()?,
                owners.map(|Extension(v)| v).unwrap_or_default(),
            ))
        })
        .await
}

async fn provider_usage(
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Vec<ProviderUsage>>>> {
    request.unpaged(query, service::provider_usage).await
}

async fn capacity(
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Capacity>>> {
    request.unpaged(query, capacity::capacity).await
}

async fn swarm(
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Swarm>>> {
    request.unpaged(query, swarm::swarm).await
}

async fn sessions(
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Page<SessionSummary>>>> {
    request
        .reply(3, || Ok(service::sessions(&request, query)))
        .await
}

async fn operations(
    request: DashboardRequest,
    SessionPath(id): SessionPath,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<SessionOperations>>> {
    request
        .reply(2, || Ok(service::operations(&request, id, query.scope()?)))
        .await
}

async fn detail(
    request: DashboardRequest,
    SessionPath(id): SessionPath,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<SessionDetail>>> {
    request
        .reply(2, || Ok(service::detail(&request, id, query.scope()?)))
        .await
}

async fn projects(
    request: DashboardRequest,
    DashboardQuery(query): DashboardQuery,
) -> Result<Json<Envelope<Page<ProjectRef>>>> {
    request
        .reply(2, || Ok(service::projects(&request, query)))
        .await
}
