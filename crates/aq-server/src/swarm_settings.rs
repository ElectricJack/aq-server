//! Owner controls share authentication and CSRF with Intelligence Settings.
use crate::{AppState, authz::RequireOwner, error::AppError};
use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    routing::{get, post},
};
use serde_json::Value;
use uuid::Uuid;
fn error(error: aq_swarm_settings::Error) -> AppError {
    aq_swarm_settings::map_error(
        error,
        AppError::BadRequest,
        AppError::Conflict,
        AppError::NotFound,
        crate::error::db,
        AppError::Internal,
    )
}
fn available(state: &AppState) -> Vec<aq_swarm_settings::providers::Provider> {
    aq_swarm_settings::http::catalog(
        state
            .config
            .intelligence
            .as_deref()
            .map_or(&[], |r| r.credentials.as_slice()),
    )
}
pub(crate) fn routes() -> Router<AppState> {
    aq_swarm_settings::http::routes(get(read), post(save), get(operation))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, AppError> {
    aq_swarm_settings::http::read(&state.store.pool, &available(&state), uri.path())
        .await
        .map(Json)
        .map_err(error)
}
async fn save(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    aq_swarm_settings::http::save(
        &state.store.pool,
        actor.id,
        body,
        &available(&state),
        uri.path(),
    )
    .await
    .map(Json)
    .map_err(error)
}
async fn operation(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    aq_swarm_settings::http::operation(&state.store.pool, actor.id, id, uri.path())
        .await
        .map(Json)
        .map_err(error)
}
