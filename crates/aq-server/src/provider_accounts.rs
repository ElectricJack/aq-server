//! Sanitized configured accounts; collection lives outside the server crate.
use crate::{AppState, authz::RequireOwner, error::AppError, intelligence::config::LoadedRegistry};
pub use aq_provider_quota::Account;
use axum::{Json, Router, extract::State, routing::get};
pub(crate) fn project(
    registry: Option<&LoadedRegistry>,
    now: u64,
) -> aq_provider_quota::inventory::Accounts {
    aq_provider_quota::inventory::project(
        registry.map(|r| r.config.revision),
        registry.map_or(&[], |r| &r.credentials),
        now,
    )
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let time = u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|value| *value > 0)
        .ok_or(AppError::Internal)?;
    Ok((
        [("Cache-Control", "private, no-store")],
        Json(project(state.config.intelligence.as_deref(), time)),
    ))
}
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/human/provider-accounts", get(read))
}
