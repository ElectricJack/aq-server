//! Spec 0016 §5.2: the offer's git grant and the
//! worker route `/v1/git/<repository>/…` to central aq-git, behind the node
//! credential and the `AQ-Attempt` header. See `aq_delivery::git_grant`.
use crate::{AppState, error::AppError, profiled_execution::source::Observation};
use aq_delivery::{
    commands::SocketGit,
    git_grant::{GitGrant, Lookup, Refusal},
};
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use uuid::Uuid;

pub(crate) mod workspaces;

fn socket(state: &AppState) -> Option<&std::path::Path> {
    state.config.delivery.as_ref()?.git_http_socket.as_deref()
}
/// Freeze the grant of an enrolled offer before publishing the attempt.
pub(super) async fn offer(
    state: &AppState,
    source: &Observation,
    task: Uuid,
    attempt: Uuid,
) -> Result<GitGrant, AppError> {
    let (Some(_), Some(retention)) = (socket(state), state.source_retention.as_ref()) else {
        return Err(AppError::Maintenance(
            "enrolled git endpoint unavailable".into(),
        ));
    };
    let git = SocketGit::new(&retention.config.git_socket);
    let grant =
        aq_delivery::git_grant::offer(&state.store.pool, &git, &source.admission, task).await;
    grant.map_err(|error| {
        tracing::warn!(%attempt, code = %error.code, "git grant not issued");
        AppError::Conflict("git grant unavailable".into())
    })
}
/// Spec 0016 §5.1: a new result report's pushed ref is the attempt's task ref
/// at its reported head; recorded for the report before the report itself.
pub(super) async fn confirm(
    state: &AppState,
    node: Uuid,
    report: &aq_admission::transport::ResultReport,
    headers: &axum::http::HeaderMap,
) -> Result<(), AppError> {
    use aq_delivery::git_grant::{PushedRefusal, confirm_headers};
    let retention = state.source_retention.as_ref();
    let socket = retention.map(|r| &r.config.git_socket);
    let git = SocketGit::new(socket.cloned().unwrap_or_default());
    let ids = (node, report.binding.attempt_id, report.report_id);
    match confirm_headers(&state.store.pool, &git, ids, headers).await {
        Ok(()) => Ok(()),
        Err(PushedRefusal::BadRequest(why)) => Err(AppError::BadRequest(why.into())),
        Err(PushedRefusal::Refused(why)) => Err(AppError::Conflict(why.into())),
        Err(PushedRefusal::Unavailable) => Err(AppError::Maintenance("aq-git unavailable".into())),
    }
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    let routes = aq_supervisor_control::git_routes(&state.store.pool, &state.clock, socket(&state));
    let routes = routes.route("/v1/git/{repository}/{*path}", any(proxy));
    let routes = routes.route("/v1/git/grants/{attempt}", get(grant));
    routes.merge(workspaces::routes()).with_state(state)
}
/// Discovery returns the frozen grant to its node even on replay. Smart HTTP
/// separately requires a live grant.
async fn grant(
    State(state): State<AppState>,
    Path(attempt): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> Result<axum::Json<GitGrant>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    match aq_delivery::git_grant::lookup(&state.store.pool, attempt, node.id()).await {
        Ok(Lookup::Granted(grant)) => Ok(axum::Json(grant)),
        Ok(Lookup::Absent) => Err(AppError::NotFound("no git grant".into())),
        Ok(Lookup::WrongNode) => Err(AppError::Forbidden("git grant node mismatch".into())),
        Err(_) => Err(AppError::Maintenance("git grants unavailable".into())),
    }
}
async fn proxy(
    State(state): State<AppState>,
    Path((repository, path)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, AppError> {
    let node = crate::worker_api::authenticate_context(&state, request.headers()).await?;
    let socket = socket(&state).ok_or_else(|| AppError::NotFound("no git endpoint".into()))?;
    let (pool, route) = (&state.store.pool, (repository, path.as_str()));
    #[cfg(not(feature = "project_environments"))]
    use aq_delivery::git_grant::serve;
    #[cfg(feature = "project_environments")]
    use aq_environments::git::serve;
    serve(pool, socket, node.id(), route, request)
        .await
        .map(|response| response.map(axum::body::Body::new).into_response())
        .map_err(refusal)
}

fn refusal(error: Refusal) -> AppError {
    match error {
        Refusal::NotFound => AppError::NotFound("not a git route".into()),
        Refusal::Forbidden(why) => AppError::Forbidden(why.into()),
        Refusal::StaleLease => AppError::Conflict("the task ref is not at the lease".into()),
        Refusal::TooLarge => AppError::PayloadTooLarge("push exceeds max_push_bytes".into()),
        Refusal::BadRequest(why) => AppError::BadRequest(why.into()),
        Refusal::Unavailable => AppError::Maintenance("git endpoint unavailable".into()),
    }
}
