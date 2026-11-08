//! Central intelligence facts and deterministic selection. Provider RPCs are
//! deliberately absent: availability is bounded authenticated observation.
pub mod config {
    pub use aq_intelligence_config::*;
}
pub(crate) mod availability_action;
mod feedback;
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
    principal::fence_subject(state, tx, decision_id, project_id, project_id).await
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
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
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
        .route(
            "/api/intelligence/explain",
            post(resolver::explain).layer(DefaultBodyLimit::max(10 * 1024 * 1024)),
        )
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
async fn observe(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Json(request): Json<ManualObservation>,
) -> Result<Json<Value>> {
    if request.operation_id.is_nil()
        || request.sequence == 0
        || request.sequence > i64::MAX as u64
        || request.expected_revision > i64::MAX as u64
        || !(1000..=3_600_000).contains(&request.ttl_ms)
        || request.cooldown_ms > request.ttl_ms
    {
        return Err(bad("invalid observation identity/sequence/timing"));
    }
    let payload = aq_selection::digest(&request).map_err(|_| bad("observation digest"))?;
    // Durable accepted receipts precede current configuration validation.
    if let Some(row) = sqlx::query("SELECT payload_digest,response FROM intelligence_operations WHERE actor_id=$1 AND operation_id=$2").bind(user.id).bind(request.operation_id).fetch_optional(&state.store.pool).await.map_err(db)? {
        if row.get::<String,_>("payload_digest") != payload.as_str() { return Err(conflict("operation ID reused with changed observation")); }
        return Ok(Json(row.get("response")));
    }
    registry::register(&state).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SELECT pg_advisory_xact_lock(712009)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if let Some(row)=sqlx::query("SELECT payload_digest,response FROM intelligence_operations WHERE actor_id=$1 AND operation_id=$2").bind(user.id).bind(request.operation_id).fetch_optional(&mut *tx).await.map_err(db)? {
        if row.get::<String,_>("payload_digest")!=payload.as_str(){return Err(conflict("operation ID reused with changed observation"));}
        return Ok(Json(row.get("response")));
    }
    let registered = registry::check_current(&state, &mut tx).await?;
    let credential = loaded(&state)?
        .credentials
        .iter()
        .find(|c| {
            c.profile == request.scope.credential_profile
                && c.revision == request.scope.credential_revision
                && c.provider == request.scope.provider
                && c.account_reference == request.scope.account_reference
                && c.endpoint_reference == request.scope.endpoint_reference
        })
        .ok_or_else(|| bad("unknown exact credential scope"))?;
    if !credential.enabled {
        return Err(conflict("credential disabled"));
    }
    if let Some(node) = request.scope.node_id {
        let entry = loaded(&state)?
            .config
            .credentials
            .iter()
            .find(|c| c.credential.profile == credential.profile)
            .ok_or(AppError::Internal)?;
        let enrolled = aq_machine_control::enrolled(&mut tx, node).await;
        if !entry.admits(node, enrolled.map_err(|_| AppError::Internal)?) {
            return Err(bad("observation node outside credential authority"));
        }
    }
    let source = ObservationSource::Operator {
        principal_id: user.id,
    };
    let key = aq_selection::digest(&(&request.scope, &source)).map_err(|_| bad("scope digest"))?;
    let previous: Option<i64> = sqlx::query_scalar(
        "SELECT sequence FROM intelligence_observations WHERE scope_source_digest=$1",
    )
    .bind(key.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?;
    if registered != request.expected_revision {
        return Err(conflict("registry revision changed"));
    }
    if previous.is_some_and(|v| request.sequence <= v as u64) {
        return Err(conflict(
            "observation sequence must increase; expired rows retain replay fence",
        ));
    }
    let received = now(&state)?;
    let revision = registered
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(AppError::Internal)?;
    let observation = AvailabilityObservation {
        observation_id: Uuid::new_v4(),
        scope: request.scope,
        source,
        sequence: request.sequence,
        revision,
        state: request.state,
        received_at_ms: received,
        expires_at_ms: received
            .checked_add(request.ttl_ms)
            .ok_or_else(|| bad("expiry overflow"))?,
        cooldown_until_ms: received
            .checked_add(request.cooldown_ms)
            .ok_or_else(|| bad("cooldown overflow"))?,
    };
    observation
        .validate()
        .map_err(|_| bad("invalid observation scope"))?;
    let response = serde_json::to_value(&observation).map_err(|_| AppError::Internal)?;
    registry::quota(&mut tx, previous.is_none(), &response).await?;
    sqlx::query("INSERT INTO intelligence_observations(scope_source_digest,sequence,observation) VALUES($1,$2,$3) ON CONFLICT(scope_source_digest) DO UPDATE SET sequence=excluded.sequence,observation=excluded.observation").bind(key.as_str()).bind(request.sequence as i64).bind(&response).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("UPDATE intelligence_registry SET revision=$1 WHERE singleton=TRUE")
        .bind(revision as i64)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO intelligence_operations(actor_id,operation_id,payload_digest,response) VALUES($1,$2,$3,$4)").bind(user.id).bind(request.operation_id).bind(payload.as_str()).bind(&response).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(Json(response))
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
