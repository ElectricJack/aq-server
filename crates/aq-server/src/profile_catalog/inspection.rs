//! Owner-only deterministic profile inspection.
use crate::{authz::RequireOwner, error::AppError};
use aq_profile_settings::inspection::{Inspection, Request};
use axum::{Json, Router, extract::DefaultBodyLimit, routing::post};
pub(super) fn routes() -> Router<crate::AppState> {
    Router::new()
        .route("/api/profiles/validate", post(validate))
        .route("/api/profiles/explain", post(explain))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
}
async fn inspect(request: Request, require: bool) -> Result<Json<Inspection>, AppError> {
    if aq_profile_settings::inspection::oversized(&request) {
        return Err(AppError::PayloadTooLarge(
            "inspection input exceeds 2 MiB".into(),
        ));
    }
    tokio::task::spawn_blocking(move || aq_profile_settings::inspection::inspect(request, require))
        .await
        .map_err(|_| AppError::Internal)?
        .map(Json)
        .map_err(super::settings::error)
}
async fn validate(
    RequireOwner(_): RequireOwner,
    Json(request): Json<Request>,
) -> Result<Json<Inspection>, AppError> {
    inspect(request, false).await
}
async fn explain(
    RequireOwner(_): RequireOwner,
    Json(request): Json<Request>,
) -> Result<Json<Inspection>, AppError> {
    inspect(request, true).await
}
