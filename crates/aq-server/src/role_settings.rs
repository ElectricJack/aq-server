//! Roles and profiles as Settings documents (spec 0014 §7): the server side of
//! `aq-role-settings`. Owner controls share authentication and CSRF with
//! Intelligence Settings.
use crate::{AppState, authz::RequireOwner, dashboard_api::errors::ApiError, error::AppError};
use aq_profiles::roles::Snapshot;
use aq_role_settings::{Error, http, tasks};
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    response::Response,
    routing::{delete, get, put},
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub(crate) fn error(error: Error) -> AppError {
    match error {
        Error::Invalid(message) => AppError::BadRequest(message),
        Error::Problems(_) => AppError::BadRequest("The documents are not valid".into()),
        Error::Conflict(message) => AppError::Conflict(message),
        Error::NotFound(message) => AppError::NotFound(message),
        Error::Database(error) => crate::error::db(error),
        Error::Internal => AppError::Internal,
    }
}
pub(crate) fn admin_routes() -> Router<AppState> {
    http::routes(get(read), put(write), delete(remove))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Response {
    http::respond(http::read(&state.store.pool, uri.path()).await, error)
}
async fn write(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Response {
    let now_ms = (state.clock)().timestamp_millis();
    let saved = http::write(&state.store.pool, actor.id, uri.path(), body, now_ms).await;
    http::respond(saved, error)
}
async fn remove(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    OriginalUri(uri): OriginalUri,
) -> Response {
    let now_ms = (state.clock)().timestamp_millis();
    let (pool, references) = (&state.store.pool, crate::project_bindings::role_references);
    let removed = http::remove(pool, actor.id, uri.path(), uri.query(), now_ms, references).await;
    http::respond(removed, error)
}
/// Server start: shipped defaults that are missing are installed and the
/// stored set's snapshot is made active. A stored set that does not compile
/// is logged; the server starts and keeps the snapshot it had.
pub(crate) async fn install(state: &AppState) -> Result<(), AppError> {
    let now_ms = (state.clock)().timestamp_millis();
    let started = aq_role_settings::startup(&state.store.pool, now_ms)
        .await
        .map_err(error)?;
    for (kind, id) in &started.installed {
        tracing::info!(kind = kind.label(), %id, "installed the shipped default document");
    }
    for problem in &started.problems {
        tracing::error!(%problem, "the stored roles and profiles are not valid");
    }
    if !started.problems.is_empty() {
        tracing::error!(
            active_digest = ?started.active_digest,
            "roles and profiles: keeping the previous snapshot"
        );
    }
    Ok(())
}

/// The snapshot new work is checked against; `None` until one is active.
pub(crate) async fn active(pool: &PgPool) -> Result<Option<Snapshot>, AppError> {
    let active = aq_role_settings::active_snapshot(pool).await;
    Ok(active.map_err(error)?.map(|active| active.snapshot))
}
/// Filing or editing a task names `roles`: each must exist (spec 0014 §5.5).
pub(crate) async fn named(
    state: &AppState,
    roles: &[impl AsRef<str>],
) -> Result<Result<(), tasks::UnknownRole>, AppError> {
    if roles.is_empty() {
        return Ok(Ok(()));
    }
    let snapshot = active(&state.store.pool).await?;
    let check = |role: &_| tasks::named_role(snapshot.as_ref(), AsRef::as_ref(role));
    Ok(roles.iter().try_for_each(check))
}
/// [`named`] for the action surface: 422 `unknown_role`, naming the role and
/// listing the roles that exist.
pub(crate) async fn named_api(state: &AppState, roles: &[impl AsRef<str>]) -> Result<(), ApiError> {
    named(state, roles).await?.map_err(|unknown| {
        let status = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
        ApiError::new(status, "unknown_role", "Unknown role").with_message(&unknown.to_string())
    })
}
/// A task's effective role as every task read shows it: `{id, source}`.
pub(crate) fn shown(node: &Value, default_role: &str) -> Value {
    tasks::effective_role(&node["metadata"], default_role).shown()
}
/// Whether an attempt of `task` was ever granted; its role is then fixed.
pub(crate) async fn granted(pool: &PgPool, task: Uuid) -> Result<bool, AppError> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=$1 AND NOT (COALESCE(h.state,e.state)->>'status' IN ('offered','preparing','declined','never_granted') \
         AND COALESCE(jsonb_typeof(COALESCE(h.state,e.state)->'grant'),'null')='null'))",
    )
    .bind(task)
    .fetch_one(pool)
    .await
    .map_err(crate::error::db)
}
/// The ready tasks whose effective role is disabled or missing, with the
/// hold code: the producer offers none of them.
pub(crate) async fn held(
    pool: &PgPool,
    default_role: &str,
    nodes: impl Iterator<Item = (Uuid, &Value)>,
) -> Result<std::collections::BTreeMap<Uuid, &'static str>, AppError> {
    let snapshot = active(pool).await?;
    Ok(nodes
        .filter_map(|(task, node)| {
            let role = tasks::effective_role(&node["metadata"], default_role);
            tasks::role_hold(snapshot.as_ref(), &role.id).map(|hold| (task, hold.code))
        })
        .collect())
}
