//! Owner controls use the same authentication and CSRF boundary as Intelligence Settings.
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_swarm_settings::{ModeRequest, SaveRequest, Snapshot};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::get,
};
use uuid::Uuid;

fn error(error: aq_swarm_settings::Error) -> AppError {
    match error {
        aq_swarm_settings::Error::Invalid(m) => AppError::BadRequest(m),
        aq_swarm_settings::Error::Conflict(m) => AppError::Conflict(m),
        aq_swarm_settings::Error::NotFound(m) => AppError::NotFound(m),
        aq_swarm_settings::Error::Database(e) => crate::error::db(e),
        aq_swarm_settings::Error::Internal => AppError::Internal,
    }
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings/swarm", get(read).post(save))
        .route("/api/settings/swarm/mode", axum::routing::post(mode))
        .route("/api/settings/swarm/operations/{id}", get(operation))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Snapshot>, AppError> {
    aq_swarm_settings::read(&state.store.pool)
        .await
        .map(Json)
        .map_err(error)
}
async fn save(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Json(request): Json<SaveRequest>,
) -> Result<Json<Snapshot>, AppError> {
    aq_swarm_settings::save(&state.store.pool, actor.id, request)
        .await
        .map(Json)
        .map_err(error)
}
async fn mode(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Json(request): Json<ModeRequest>,
) -> Result<Json<Snapshot>, AppError> {
    aq_swarm_settings::set_mode(&state.store.pool, actor.id, request)
        .await
        .map(Json)
        .map_err(error)
}
async fn operation(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
) -> Result<Json<Snapshot>, AppError> {
    aq_swarm_settings::operation(&state.store.pool, actor.id, id)
        .await
        .map(Json)
        .map_err(error)
}
