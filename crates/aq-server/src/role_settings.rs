//! Roles and profiles as Settings documents (spec 0014 §7): the server side of
//! `aq-role-settings`. Owner controls share authentication and CSRF with
//! Intelligence Settings.
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_role_settings::{Error, http};
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    response::Response,
    routing::{delete, get, put},
};
use serde_json::Value;

fn error(error: Error) -> AppError {
    match error {
        Error::Invalid(message) => AppError::BadRequest(message),
        Error::Problems(_) => AppError::BadRequest("The documents are not valid".into()),
        Error::Conflict(message) => AppError::Conflict(message),
        Error::NotFound(message) => AppError::NotFound(message),
        Error::Database(error) => crate::error::db(error),
        Error::Internal => AppError::Internal,
    }
}
pub(crate) fn admin_routes() -> Router<AppState> {
    http::routes(get(read), put(write), delete(remove))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Response {
    http::respond(http::read(&state.store.pool, uri.path()).await, error)
}
async fn write(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Response {
    let now_ms = (state.clock)().timestamp_millis();
    let saved = http::write(&state.store.pool, actor.id, uri.path(), body, now_ms).await;
    http::respond(saved, error)
}
async fn remove(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let now_ms = (state.clock)().timestamp_millis();
    let removed = http::remove(&state.store.pool, actor.id, uri.path(), uri.query(), now_ms).await;
    http::respond(removed, error)
}
/// Server start: shipped defaults that are missing are installed and the
/// stored set's snapshot is made active. A stored set that does not compile
/// is logged; the server starts and keeps the snapshot it had.
pub(crate) async fn install(state: &AppState) -> Result<(), AppError> {
    let now_ms = (state.clock)().timestamp_millis();
    let started = aq_role_settings::startup(&state.store.pool, now_ms)
        .await
        .map_err(error)?;
    for (kind, id) in &started.installed {
        tracing::info!(kind = kind.label(), %id, "installed the shipped default document");
    }
    for problem in &started.problems {
        tracing::error!(%problem, "the stored roles and profiles are not valid");
    }
    if !started.problems.is_empty() {
        tracing::error!(
            active_digest = ?started.active_digest,
            "roles and profiles: keeping the previous snapshot"
        );
    }
    Ok(())
}
