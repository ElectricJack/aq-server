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
/// The configured providers; one whose saved Codex login is past the owner's age limit
/// says so, since selection then treats that credential as missing.
async fn available(
    state: &AppState,
) -> Result<Vec<aq_swarm_settings::providers::Provider>, AppError> {
    let Some(loaded) = state.config.intelligence.as_deref() else {
        return Ok(aq_swarm_settings::http::providers(&[]));
    };
    let mut available = aq_swarm_settings::http::providers(&loaded.credentials);
    let mut conn = state.store.pool.acquire().await.map_err(crate::error::db)?;
    let now = (state.clock)().timestamp_millis();
    for (index, reason) in aq_login_store::stale_logins(&mut conn, loaded, now).await? {
        let provider = &loaded.credentials[index].provider;
        if let Some(entry) = available.iter_mut().find(|p| &p.id == provider) {
            entry.attention = Some(reason.replace("codex_login_stale: ", ""));
        }
    }
    Ok(available)
}
pub(crate) fn routes() -> Router<AppState> {
    aq_swarm_settings::http::routes(get(read), post(save), get(operation))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<Value>, AppError> {
    aq_swarm_settings::http::read(&state.store.pool, &available(&state).await?, uri.path())
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
        &available(&state).await?,
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
