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
        .route("/api/settings/model-suggestions", get(model_suggestions))
}
/// `GET /api/settings/model-suggestions`: per harness, the models a host tool
/// discovered (`intelligence_preferences_file`). Suggestions for the level
/// editor only: any model a level names is allowed.
async fn model_suggestions(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Json<Value> {
    let path = state.config.intelligence_preferences_file.clone();
    let read =
        tokio::task::spawn_blocking(move || aq_role_settings::suggestions::read(path.as_deref()));
    let harnesses = read.await.unwrap_or_default();
    Json(serde_json::json!({ "harnesses": harnesses }))
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
    if saved.is_ok() {
        // The routing document or what the levels offer may have changed.
        crate::routing::author(&state).await;
    }
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
    if removed.is_ok() {
        crate::routing::author(&state).await;
    }
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

/// False only for a historical test store older than migration 0134 (no
/// role tables); its tasks are then not checked against roles.
#[allow(clippy::needless_return)]
pub(crate) fn tables(_state: &AppState) -> bool {
    #[cfg(feature = "test-support")]
    return _state.store.fixture_profile.is_none_or(|v| v >= 134);
    #[cfg(not(feature = "test-support"))]
    true
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
    if roles.is_empty() || !tables(state) {
        return Ok(Ok(()));
    }
    let snapshot = active(&state.store.pool).await?;
    let check = |role: &_| tasks::named_role(snapshot.as_ref(), AsRef::as_ref(role));
    Ok(roles.iter().try_for_each(check))
}
/// An agent may not hand out more than it holds (spec 0014 step 3b): the
/// role an agent files a task with (`named`, else the project's default) may
/// list only operations of the role the filing attempt itself runs as, read
/// from the snapshot that attempt launched with. `Err` is the refusal, naming
/// the operations it lacks. An attempt with no summary was not launched from
/// a role and is not restricted; people and playbooks never come here.
pub(crate) async fn within_filer(
    state: &AppState,
    attempt: Uuid,
    project: Uuid,
    named: Option<&str>,
) -> Result<Result<(), String>, AppError> {
    if !tables(state) {
        return Ok(Ok(()));
    }
    let pool = &state.store.pool;
    let mut conn = pool.acquire().await.map_err(crate::error::db)?;
    let ran_with = aq_role_settings::attempts::ran_with(&mut conn, attempt).await;
    drop(conn);
    let Some(ran_with) = ran_with.map_err(error)? else {
        return Ok(Ok(()));
    };
    // An attempt recorded before roles names no snapshot: not restricted.
    let Some(digest) = &ran_with.snapshot_digest else {
        return Ok(Ok(()));
    };
    let own = aq_role_settings::snapshot(pool, digest).await;
    let (own, active) = (own.map_err(error)?, active(pool).await?);
    let filer = own
        .as_ref()
        .and_then(|snapshot| snapshot.role(&ran_with.role));
    let role = match named {
        Some(role) => role.to_owned(),
        None => {
            let record = crate::project_bindings::current(state, project).await?;
            record.binding.default_role
        }
    };
    let target = active.as_ref().and_then(|snapshot| snapshot.role(&role));
    // A role that does not exist is refused (or held) by its own rule.
    let (Some(filer), Some(target)) = (filer, target) else {
        return Ok(Ok(()));
    };
    let lacking = tasks::operations_beyond(&filer.operations, &target.operations);
    Ok(if lacking.is_empty() {
        Ok(())
    } else {
        Err(tasks::beyond_message(&filer.id, &target.id, &lacking))
    })
}
/// [`named`] for the action surface: 422 `unknown_role`, naming the role and
/// listing the roles that exist.
pub(crate) async fn named_api(state: &AppState, roles: &[impl AsRef<str>]) -> Result<(), ApiError> {
    named(state, roles).await?.map_err(|unknown| {
        let status = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
        ApiError::new(status, "unknown_role", "Unknown role").with_message(&unknown.to_string())
    })
}
/// The hold of a task whose effective role is `role`, as the work read
/// lists it, read from `pool`.
pub(crate) async fn hold(
    state: &AppState,
    pool: &PgPool,
    role: &tasks::EffectiveRole,
) -> Result<Option<Value>, AppError> {
    if !tables(state) {
        return Ok(None);
    }
    let snapshot = active(pool).await?;
    Ok(tasks::role_hold(snapshot.as_ref(), &role.id).map(|hold| hold.shown()))
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
/// The ready tasks that get no offer, with the hold code: the effective
/// role is disabled or missing, or the level the task would run on (the class
/// it was routed to, else the project's default) has no document.
pub(crate) async fn held(
    state: &AppState,
    binding: &crate::project_bindings::domain::ProjectBinding,
    nodes: impl Iterator<Item = (Uuid, &Value, Option<String>)>,
) -> Result<std::collections::BTreeMap<Uuid, &'static str>, AppError> {
    if !tables(state) {
        return Ok(Default::default());
    }
    let snapshot = active(&state.store.pool).await?;
    let snapshot = snapshot.as_ref();
    Ok(nodes
        .filter_map(|(task, node, class)| {
            let role = tasks::effective_role(&node["metadata"], &binding.default_role);
            let role = tasks::role_hold(snapshot, &role.id).map(|hold| hold.code);
            let level = || {
                let default = &binding.default_intelligence;
                let hold = tasks::level_hold(snapshot, class.as_deref(), default)?;
                tracing::info!(%task, level = %hold.level, "{}", hold.message());
                Some(tasks::LevelHold::CODE)
            };
            role.or_else(level).map(|code| (task, code))
        })
        .collect())
}
/// The level hold of a task as the work read lists it.
pub(crate) async fn level_hold(
    state: &AppState,
    pool: &PgPool,
    binding: &crate::project_bindings::domain::ProjectBinding,
    node: &Value,
) -> Result<Option<Value>, AppError> {
    if !tables(state) {
        return Ok(None);
    }
    let class = match aq_routing::CompactRoute::from_metadata(&node["metadata"]) {
        Ok(Some(route)) => Some(route.c.to_string()),
        _ => None,
    };
    let snapshot = active(pool).await?;
    let default = &binding.default_intelligence;
    let hold = tasks::level_hold(snapshot.as_ref(), class.as_deref(), default);
    Ok(hold.map(|hold| hold.shown()))
}
