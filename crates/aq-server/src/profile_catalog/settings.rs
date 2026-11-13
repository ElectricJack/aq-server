use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_profile_settings::{ModelChoice, http};
use axum::{
    Json, Router,
    extract::{OriginalUri, Path, State},
    routing::get,
};
use serde_json::Value;
pub(crate) fn error(error: aq_profile_settings::Error) -> AppError {
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
pub(crate) fn routes() -> Router<AppState> {
    http::routes(get(read).post(save), get(operation)).merge(crate::routing::settings_routes())
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
    let pool = &state.store.pool;
    let structure = uri.path().contains("/structure");
    let before = match structure {
        true => Some(
            aq_profile_settings::structure::document(pool)
                .await
                .map_err(error)?
                .0,
        ),
        false => None,
    };
    let now = (state.clock)().timestamp_millis();
    let saved = http::save(pool, actor.id, request, &choices(&state), now, uri.path()).await;
    let saved = saved.map_err(error)?;
    // A saved change to the routing Markdown is compiled (spec 0011 §6); the save stands either way.
    let revision = saved["revision"].as_str().and_then(|r| r.parse().ok());
    if let (Some(before), Some(revision)) = (before, revision)
        && saved["disposition"] == "saved"
        && saved["routing_markdown"].as_str() != Some(before.routing_markdown.as_str())
        && let Err(error) = crate::routing::compile::requested(&state, revision).await
    {
        tracing::warn!(?error, "routing policy compile not requested");
    }
    Ok(Json(saved))
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
