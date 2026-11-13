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
/// readiness is `ready_state`), or none when the binding is not on an automatic router or
/// routing is off and the task has no route. A task without a route waits only while it is ready;
/// it has waited since the node's last update, the nearest time the work read holds to when
/// it became ready.
pub(crate) async fn view(
    state: &AppState,
    record: &ProjectRecord,
    task: Uuid,
    node: &Value,
    ready_state: &str,
) -> Result<Option<Value>, AppError> {
    let pool = &state.store.pool;
    let on = enabled(pool).await?;
    // Routing off and no route: placed as without routing, whatever the binding.
    if !on && matches!(CompactRoute::from_metadata(&node["metadata"]), Ok(None)) {
        return Ok(None);
    }
    let placement = Placement::of(on_router(state, record).await?, on, &node["metadata"]);
    let updated = node["updated_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |t| t.timestamp_millis());
    Ok(match placement {
        Placement::Unchanged => None,
        Placement::Routed(route) => {
            let since = (history(pool, task).await?.first()).map_or(updated, |r| r.at_ms);
            Some(json!({"state":"routed","route":compact(&route),"since_ms":since}))
        }
        Placement::Wait | Placement::Reroute => {
            (ready_state == "ready").then(|| json!({"state":"waiting","since_ms":updated}))
        }
    })
}
