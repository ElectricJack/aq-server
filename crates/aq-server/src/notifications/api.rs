//! Human-only API. No worker route can create external recipients or select URLs.
use super::{
    Result,
    persistence::{RetryRequest, RouteRequest, SendRequest, Service},
};
use crate::{
    AppState,
    authz::{CurrentUser, RequireOwner},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
pub fn service(state: &AppState) -> Service {
    Service {
        pool: state.store.notification_pool.clone(),
        configuration: state.config.notifications.clone(),
        public_origin: state.config.server.public_origin.clone(),
        clock: state.clock.clone(),
    }
}
pub fn routes(mut state: AppState) -> Router<AppState> {
    state.store = state.store.notification_view();
    Router::new()
        .route("/api/notifications", get(inbox).post(send))
        .route("/api/notifications/routes", post(route))
        .route("/api/notifications/retry", post(retry))
        .route("/api/notifications/{message}", get(inspect))
        .layer(DefaultBodyLimit::max(16384))
        .with_state(state)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    recipient: Option<Uuid>,
    #[serde(default)]
    after: i64,
    #[serde(default = "limit")]
    limit: u32,
}
fn limit() -> u32 {
    32
}
async fn inbox(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<Page>,
) -> Result<Json<super::persistence::Inbox>> {
    Ok(Json(
        service(&state)
            .inbox(
                user.id,
                page.recipient.unwrap_or(user.id),
                (page.after, page.limit),
            )
            .await?,
    ))
}
async fn inspect(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(message): Path<Uuid>,
) -> Result<Json<Value>> {
    Ok(Json(service(&state).inspect(user.id, message).await?))
}
async fn send(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<SendRequest>,
) -> Result<Json<Value>> {
    Ok(Json(service(&state).enqueue(user.id, request).await?))
}
async fn route(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<RouteRequest>,
) -> Result<Json<Value>> {
    Ok(Json(service(&state).set_route(user.id, request).await?))
}
async fn retry(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<RetryRequest>,
) -> Result<Json<Value>> {
    Ok(Json(service(&state).retry_unknown(user.id, request).await?))
}
