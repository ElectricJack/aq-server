//! Spec 0016 §5.2 (feature `delivery_playbook`): the offer's git grant and the
//! worker route `/v1/git/<repository>/…` to central aq-git, authenticated by
//! the node credential and the attempt's token. See `aq_delivery::git_grant`.
use crate::{AppState, error::AppError, profiled_execution::source::Observation};
use aq_admission::{control::NativePollPage, startup::GitGrant};
use aq_delivery::git_grant::{self as git, Refusal};
use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use uuid::Uuid;

fn socket(state: &AppState) -> Option<&std::path::Path> {
    state.config.delivery.as_ref()?.git_http_socket.as_deref()
}
fn mac(state: &AppState) -> impl Fn(&[u8]) -> Option<[u8; 32]> + '_ {
    let key = crate::delegated_permits::runtime::key(state).ok();
    move |message| key?.git_token_mac(message).ok()
}
pub(super) async fn offer(
    state: &AppState,
    source: &Observation,
    task: Uuid,
    attempt: Uuid,
) -> Result<Option<GitGrant>, AppError> {
    let Some(socket) = socket(state) else {
        return Ok(None);
    };
    let grant = git::offer_for(&state.store.pool, socket, &source.admission, task, attempt);
    grant.await.map_err(|error| {
        tracing::warn!(%attempt, %error, "git grant not issued");
        AppError::Conflict("git grant unavailable".into())
    })
}
pub(crate) fn deliver(
    state: &AppState,
    node: Uuid,
    mut page: NativePollPage,
) -> Result<NativePollPage, AppError> {
    let unavailable = || AppError::Maintenance("delegated permit key is not configured".into());
    git::deliver(&mut page, node, mac(state)).ok_or_else(unavailable)?;
    Ok(page)
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    let route = axum::routing::any(proxy);
    axum::Router::new()
        .route("/v1/git/{repository}/{*path}", route)
        .with_state(state)
}
async fn proxy(
    State(state): State<AppState>,
    Path((repository, _)): Path<(Uuid, String)>,
    request: Request,
) -> Result<Response, AppError> {
    let auth = crate::worker_api::authenticate_context(&state, request.headers()).await?;
    let socket = socket(&state).ok_or_else(|| AppError::NotFound("no git endpoint".into()))?;
    let served = git::serve(
        &state.store.pool,
        socket,
        auth.id(),
        repository,
        mac(&state),
        request,
    );
    match served.await {
        Ok(response) => Ok(response.map(axum::body::Body::new).into_response()),
        Err(Refusal::Forbidden) => Err(AppError::Forbidden("no git grant for this request".into())),
        Err(Refusal::Unavailable) => Err(AppError::Maintenance("git endpoint unavailable".into())),
    }
}
