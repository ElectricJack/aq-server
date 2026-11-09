use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_profile_settings::{ModelChoice, SaveRequest};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::get,
};
use serde_json::Value;
use uuid::Uuid;
pub(super) fn error(error: aq_profile_settings::Error) -> AppError {
    match error {
        aq_profile_settings::Error::Invalid(m) => AppError::BadRequest(m),
        aq_profile_settings::Error::Conflict(m) => AppError::Conflict(m),
        aq_profile_settings::Error::NotFound(m) => AppError::NotFound(m),
        aq_profile_settings::Error::Database(e) => crate::error::db(e),
        aq_profile_settings::Error::Internal => AppError::Internal,
    }
}
fn choices(state: &AppState) -> Vec<ModelChoice> {
    state
        .config
        .intelligence
        .iter()
        .flat_map(|registry| registry.config.qualified_harnesses.iter())
        .flat_map(|harness| {
            harness.models.iter().map(|m| ModelChoice {
                provider: m.provider.clone(),
                model: m.model.clone(),
                reasoning: m.native_reasoning.clone(),
                reasoning_configuration_reference: m.reasoning_configuration_reference.clone(),
                harness: harness.harness.clone(),
            })
        })
        .collect()
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings/intelligence", get(read).post(save))
        .route("/api/settings/intelligence/operations/{id}", get(operation))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Value>, AppError> {
    aq_profile_settings::read(&state.store.pool, &choices(&state))
        .await
        .map(Json)
        .map_err(error)
}
async fn save(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Json(request): Json<SaveRequest>,
) -> Result<Json<Value>, AppError> {
    aq_profile_settings::save(
        &state.store.pool,
        actor.id,
        request,
        &choices(&state),
        (state.clock)().timestamp_millis(),
    )
    .await
    .map(Json)
    .map_err(error)
}
async fn operation(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    aq_profile_settings::operation(&state.store.pool, actor.id, id)
        .await
        .map(Json)
        .map_err(error)
}
