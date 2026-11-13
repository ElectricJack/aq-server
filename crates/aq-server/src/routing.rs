//! The route on a task (spec 0011 §3) and the routing switch (§4). The current
//! route is `aq:route` in the task's graph metadata, written with the task's
//! expected version; `task_routes` keeps every route the task has had.
use crate::{
    AppState,
    agent_access::{self, human::graph_actions::current},
    error::{AppError, db},
};
use aq_routing::{Class, CompactRoute, Route, Source};
pub use aq_routing::{StoredRoute, Written};
use serde_json::json;
use sqlx::{PgPool, types::Json};
use std::time::Duration;
use uuid::Uuid;

pub async fn write(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    route: &Route,
) -> Result<Written, AppError> {
    let by = route.source;
    store(state, workspace, task, expected_version, by, Some(route)).await
}

pub async fn clear(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    by: Source,
) -> Result<Written, AppError> {
    store(state, workspace, task, expected_version, by, None).await
}

fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("Graph route write unavailable".into())
}

async fn store(
    state: &AppState,
    workspace: Uuid,
    task: Uuid,
    expected_version: u64,
    by: Source,
    route: Option<&Route>,
) -> Result<Written, AppError> {
    let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
    let node = current(rt, workspace, task, "task").await?;
    let stored = CompactRoute::from_metadata(&node["metadata"]).map_err(|_| unavailable())?;
    if !aq_routing::may_replace(stored.as_ref(), by) {
        return Ok(Written::Stale);
    }
    let pool = &state.store.pool;
    let mut tx = pool.begin().await.map_err(db)?;
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO task_routes(task_id,sequence,route,at_ms)
         SELECT $1, COALESCE(MAX(sequence),0)+1, $2, $3 FROM task_routes WHERE task_id=$1
         RETURNING sequence",
    )
    .bind(task)
    .bind(route.map(Json))
    .bind((state.clock)().timestamp_millis())
    .fetch_one(&mut *tx)
    .await;
    let sequence = match inserted {
        // A concurrent writer took this sequence; at most one of us can win the version.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Ok(Written::Stale),
        r => r.map_err(db)? as u64,
    };
    // Until the commit below, any error rolls the history row back.
    let compact = route
        .map(|r| r.compact(sequence))
        .transpose()
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let metadata =
        aq_routing::with_route(&node["metadata"], compact.as_ref()).map_err(|_| unavailable())?;
    let body = json!({
        "idempotency_key": format!("aq-route:{task}:{sequence}:{}", Uuid::new_v4().simple()),
        "reason": "AQ routing",
        "operations": [{"op":"update_node","id":task,"expected_version":expected_version,
            "patch":{"metadata":metadata}}],
    });
    if !agent_access::settlement_request_valid(&body) {
        return Err(unavailable());
    }
    let bytes = serde_json::to_vec(&body).map_err(|_| unavailable())?;
    tx.commit().await.map_err(db)?;
    let posted = tokio::time::timeout(
        Duration::from_secs(3),
        agent_access::settlement_post(rt, workspace, bytes),
    )
    .await;
    // A lost or unreadable response keeps the history row: the update may have applied.
    let Ok(Ok((status, raw))) = posted else {
        return Err(unavailable());
    };
    if status.is_success() && agent_access::settlement_response_valid(&raw) {
        return Ok(Written::Routed { sequence });
    }
    let code = raw["error"]["code"].as_str();
    let stale = status.as_u16() == 409 && code == Some("conflict");
    // Graph refused the update outright, so this route never existed.
    if stale || (matches!(status.as_u16(), 400 | 401 | 403 | 404 | 422) && code.is_some()) {
        sqlx::query("DELETE FROM task_routes WHERE task_id=$1 AND sequence=$2")
            .bind(task)
            .bind(sequence as i64)
            .execute(pool)
            .await
            .map_err(db)?;
    }
    if stale {
        return Ok(Written::Stale);
    }
    Err(unavailable())
}

/// Every route the task has had, newest first.
pub async fn history(pool: &PgPool, task: Uuid) -> Result<Vec<StoredRoute>, AppError> {
    let rows = sqlx::query_as::<_, (i64, Option<Json<Route>>, i64)>(
        "SELECT sequence,route,at_ms FROM task_routes WHERE task_id=$1 ORDER BY sequence DESC",
    )
    .bind(task)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|(sequence, route, at_ms)| StoredRoute {
            sequence: sequence as u64,
            route: route.map(|r| r.0),
            at_ms,
        })
        .collect())
}

pub async fn enabled(pool: &PgPool) -> Result<bool, AppError> {
    sqlx::query_scalar("SELECT enabled FROM routing_state WHERE singleton")
        .fetch_one(pool)
        .await
        .map_err(db)
}

/// Routing turns on only while a compiled policy is active.
pub async fn set_enabled(pool: &PgPool, on: bool) -> Result<(), AppError> {
    let changed = sqlx::query(
        "UPDATE routing_state s SET enabled=$1 WHERE singleton AND (NOT $1 OR EXISTS(
           SELECT 1 FROM routing_policies p WHERE p.digest=s.active_policy_digest AND p.status='active'))",
    )
    .bind(on)
    .execute(pool)
    .await
    .map_err(db)?
    .rows_affected();
    if changed == 0 {
        return Err(AppError::Conflict(
            "Routing can be turned on once a routing policy has compiled".into(),
        ));
    }
    Ok(())
}

/// The profile Settings assigns to the class's parent and effort, as `id@version`.
/// The automatic router is never a class's profile.
pub async fn class_profile(
    pool: &PgPool,
    catalog: &aq_profiles::ValidatedCatalog,
    class: Class,
) -> Result<Option<String>, AppError> {
    let (document, _, _) = aq_profile_settings::structure::document(pool)
        .await
        .map_err(crate::profile_catalog::settings::error)?;
    let Some(assignment) = document
        .assignments
        .iter()
        .find(|a| a.parent_id == class.parent.as_str() && a.effort == class.effort.as_str())
    else {
        return Ok(None);
    };
    Ok(catalog
        .artifact()
        .catalog
        .intelligence
        .iter()
        .find(|p| p.id == assignment.profile_id && p.automatic_routing != Some(true))
        .map(|p| aq_profiles::key(&p.id, p.version)))
}
