//! Central intelligence facts and deterministic selection. Provider RPCs are
//! deliberately absent: availability is bounded authenticated observation.
pub mod config {
    pub use aq_intelligence_config::*;
}
pub(crate) mod availability_action;
mod feedback;
mod observations;
use observations::observe;
mod inventory;
pub mod login_import;
mod principal;
mod registry;
pub mod resolver;
/// Recheck the installed registry while retaining its lock through admission.
/// This never installs configuration or reads private files.
pub(crate) async fn fence_current_registry(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<u64> {
    registry::check_current(state, tx).await
}
/// Current original principal for one sealed attempt; never a fresh selection.
pub(crate) async fn fence_continuing_principal(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision_id: uuid::Uuid,
    project_id: uuid::Uuid,
) -> Result<()> {
    principal::fence_subject(state, tx, decision_id, (project_id, project_id)).await
}
pub fn worker_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/worker/intelligence/inventory", post(inventory::report))
        .route("/v1/worker/intelligence/failure", post(feedback::report))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_selection::{Availability, AvailabilityObservation, ObservationScope, ObservationSource};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
use crate::error::db;
fn bad(s: &str) -> AppError {
    AppError::BadRequest(s.into())
}
fn conflict(s: &str) -> AppError {
    AppError::Conflict(s.into())
}
fn now(state: &AppState) -> Result<u64> {
    (state.clock)()
        .timestamp_millis()
        .try_into()
        .map_err(|_| AppError::Internal)
}
fn loaded(state: &AppState) -> Result<&config::LoadedRegistry> {
    state
        .config
        .intelligence
        .as_deref()
        .ok_or_else(|| AppError::Conflict("intelligence registry not configured".into()))
}
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/intelligence/registry", get(inspect))
        .route("/api/intelligence/observations", post(observe))
        .route("/api/intelligence/register", post(install))
        .layer(DefaultBodyLimit::max(128 * 1024))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualObservation {
    pub operation_id: Uuid,
    pub expected_revision: u64,
    pub sequence: u64,
    pub scope: ObservationScope,
    pub state: Availability,
    pub ttl_ms: u64,
    pub cooldown_ms: u64,
}
async fn inspect(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Value>> {
    loaded(&state)?;
    let row = sqlx::query(
        "SELECT revision,credentials FROM intelligence_registry WHERE singleton=TRUE",
    )
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| {
        conflict("registry has not been installed; submit an owner observation or install request")
    })?;
    let observations: Vec<Value> = sqlx::query_scalar(
        "SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1024",
    )
    .fetch_all(&state.store.pool)
    .await
    .map_err(db)?;
    Ok(Json(
        json!({"revision":row.get::<i64,_>("revision"),"credentials":row.get::<Value,_>("credentials"),"observations":observations,"accounting":"unavailable","hard_cost_enforcement":"unavailable"}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {}
async fn install(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Json(_request): Json<InstallRequest>,
) -> Result<Json<Value>> {
    registry::register(&state).await?;
    Ok(Json(json!({"installed":true})))
}
