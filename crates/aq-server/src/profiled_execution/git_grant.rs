//! Spec 0016 §5.2 (feature `delivery_playbook`): the offer's git grant and the
//! worker route `/v1/git/<repository>/…` to central aq-git, behind the node
//! credential and the `AQ-Attempt` header. See `aq_delivery::git_grant`.
use crate::{AppState, error::AppError, profiled_execution::source::Observation};
use aq_delivery::{
    commands::SocketGit,
    git_grant::{GitGrant, Refusal},
};
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

fn socket(state: &AppState) -> Option<&std::path::Path> {
    state.config.delivery.as_ref()?.git_http_socket.as_deref()
}
/// The grant of a new offer; none while the server serves no git endpoint.
pub(super) async fn offer(
    state: &AppState,
    source: &Observation,
    task: Uuid,
    attempt: Uuid,
) -> Result<Option<GitGrant>, AppError> {
    let (Some(_), Some(retention)) = (socket(state), state.source_retention.as_ref()) else {
        return Ok(None);
    };
    let git = SocketGit::new(&retention.config.git_socket);
    let grant =
        aq_delivery::git_grant::offer(&state.store.pool, &git, &source.admission, task).await;
    grant.map(Some).map_err(|error| {
        tracing::warn!(%attempt, code = %error.code, "git grant not issued");
        AppError::Conflict("git grant unavailable".into())
    })
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    let route = axum::routing::any(proxy);
    let routes = axum::Router::new().route("/v1/git/{repository}/{*path}", route);
    let grant = axum::routing::get(grant);
    routes
        .route("/v1/git/grants/{attempt}", grant)
        .with_state(state)
}
/// The attempt's grant, for its node while the attempt holds its task.
async fn grant(
    State(state): State<AppState>,
    Path(attempt): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<GitGrant>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    let grant = aq_delivery::git_grant::live(&state.store.pool, attempt, node.id()).await;
    let grant = grant.map_err(|_| AppError::Maintenance("git grants unavailable".into()))?;
    grant
        .map(axum::Json)
        .ok_or_else(|| AppError::NotFound("no live git grant".into()))
}
async fn proxy(
    State(state): State<AppState>,
    Path((repository, path)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, AppError> {
    let node = crate::worker_api::authenticate_context(&state, request.headers()).await?;
    let socket = socket(&state).ok_or_else(|| AppError::NotFound("no git endpoint".into()))?;
    let (pool, route) = (&state.store.pool, (repository, path.as_str()));
    match aq_delivery::git_grant::serve(pool, socket, node.id(), route, request).await {
        Ok(response) => Ok(response.map(axum::body::Body::new).into_response()),
        Err(Refusal::NotFound) => Err(AppError::NotFound("not a git route".into())),
        Err(Refusal::Forbidden(why)) => Err(AppError::Forbidden(why.into())),
        Err(Refusal::StaleLease) => Err(AppError::Conflict(
            "the task ref is not at the lease".into(),
        )),
        Err(Refusal::TooLarge) => Err(AppError::PayloadTooLarge(
            "push exceeds max_push_bytes".into(),
        )),
        Err(Refusal::BadRequest(why)) => Err(AppError::BadRequest(why.into())),
        Err(Refusal::Unavailable) => Err(AppError::Maintenance("git endpoint unavailable".into())),
    }
}
