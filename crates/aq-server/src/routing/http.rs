//! What the owner sees of a task's routing (spec 0011 §3, §4, §8): the `routing` object of the
//! task work read and the route history.
use super::{enabled, history, on_router};
use crate::{
    AppState, authz::RequireOwner, error::AppError, project_bindings::domain::ProjectRecord,
};
use aq_routing::{CompactRoute, Placement};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn admin_routes() -> Router<AppState> {
    Router::new().route("/api/tasks/{id}/routes", get(routes))
}

/// `GET /api/tasks/{id}/routes`: every route the task has had, newest first, with its reasons
/// and classification; a cleared route is `route: null`.
async fn routes(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(task): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let items = history(&state.store.pool, task).await?;
    let items: Vec<Value> = (items.iter())
        .map(|r| json!({"sequence":r.sequence,"at_ms":r.at_ms,"route":r.route}))
        .collect();
    Ok(Json(json!({"items":items})))
}

fn compact(route: &CompactRoute) -> Value {
    json!({"class":route.c,"harnesses":route.h,"providers":route.p,"prefer":route.pf,
        "lane":route.l,"provider_intent":route.i,"source":route.s,"policy_digest":route.d})
}

/// The `routing` object of the work read for `node` (a task of `record`'s project whose
/// readiness is `ready_state`); none when the binding is not on an automatic router. On one:
/// `routed` with the route; `waiting` when routing is on and a ready task has no route (held
/// back, since the node's last update, the nearest time the read holds to when it became
/// ready); otherwise `unrouted` (no route, nothing held back).
pub(crate) async fn view(
    state: &AppState,
    record: &ProjectRecord,
    task: Uuid,
    node: &Value,
    ready_state: &str,
) -> Result<Option<Value>, AppError> {
    if !on_router(state, record).await? {
        return Ok(None);
    }
    let pool = &state.store.pool;
    let on = enabled(pool).await?;
    let updated = node["updated_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |t| t.timestamp_millis());
    Ok(Some(match Placement::of(true, on, &node["metadata"]) {
        Placement::Routed(route) => {
            let since = (history(pool, task).await?.first()).map_or(updated, |r| r.at_ms);
            json!({"state":"routed","route":compact(&route),"since_ms":since})
        }
        Placement::Wait | Placement::Reroute if on && ready_state == "ready" => {
            json!({"state":"waiting","since_ms":updated})
        }
        _ => json!({"state":"unrouted"}),
    }))
}
