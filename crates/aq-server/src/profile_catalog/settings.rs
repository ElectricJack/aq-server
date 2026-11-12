use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_profile_settings::{ModelChoice, http};
use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    routing::get,
};
use serde_json::Value;
pub(super) fn error(error: aq_profile_settings::Error) -> AppError {
    match error {
        aq_profile_settings::Error::Invalid(m) => AppError::BadRequest(m),
        aq_profile_settings::Error::Conflict(m) => AppError::Conflict(m),
        aq_profile_settings::Error::NotFound(m) => AppError::NotFound(m),
        aq_profile_settings::Error::Database(e) => crate::error::db(e),
        aq_profile_settings::Error::Internal => AppError::Internal,
    }
}
pub(crate) fn choices(state: &AppState) -> Vec<ModelChoice> {
    let config = &state.config;
    let harnesses = config.intelligence.iter();
    aq_profile_settings::model_choices(
        harnesses.flat_map(|r| &r.config.qualified_harnesses),
        config.intelligence_preferences_file.as_deref(),
    )
}
pub(super) fn routes() -> Router<AppState> {
    http::routes(get(read).post(save), get(operation))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, AppError> {
    http::read(&state.store.pool, &choices(&state), uri.path())
        .await
        .map(Json)
        .map_err(error)
}
async fn save(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
    Json(request): Json<Value>,
) -> Result<Json<Value>, AppError> {
    http::save(
        &state.store.pool,
        actor.id,
        request,
        &choices(&state),
        (state.clock)().timestamp_millis(),
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
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<Value>, AppError> {
    http::operation(&state.store.pool, actor.id, id, uri.path())
        .await
        .map(Json)
        .map_err(error)
}
