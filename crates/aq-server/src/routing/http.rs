//! What the owner sees of a task's routing (spec 0011 §3, §4, §8): the `routing` object of the
//! task work read and the route history.
use super::{enabled, history};
use crate::{AppState, authz::RequireOwner, error::AppError};
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

/// A Settings route, so the supervisor admin nest reaches it as well.
pub(crate) fn settings_routes() -> Router<AppState> {
    let policy = get(policy).post(switch);
    Router::new().route("/api/settings/routing", policy)
}

/// `GET /api/settings/routing`: the switch and the routing source.
async fn policy(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Value>, AppError> {
    switch_view(&state).await
}

async fn switch_view(state: &AppState) -> Result<Json<Value>, AppError> {
    let pool = &state.store.pool;
    let enabled = enabled(pool).await?;
    let now = (state.clock)().timestamp_millis();
    let source = match aq_playbook_authoring::get(pool, super::ROUTING_SOURCE).await? {
        Some(record) => aq_playbook_authoring::view(pool, &record, now).await?,
        // Before the server has written the record: no playbook yet.
        None => json!({"id": super::ROUTING_SOURCE, "kind": "builtin", "binding": super::BINDING,
            "active": null, "latest": null}),
    };
    Ok(Json(json!({"enabled": enabled, "source": source})))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Switch {
    enabled: bool,
}

/// `POST /api/settings/routing` with `{enabled}`; on only while a routing
/// playbook is active.
async fn switch(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Json(switch): Json<Switch>,
) -> Result<Json<Value>, AppError> {
    super::set_enabled(&state.store.pool, switch.enabled).await?;
    switch_view(&state).await
}

/// `GET /api/tasks/{id}/routes`: every route the task has had, newest first, with its reasons
/// and classification; a cleared route is `route: null`. `unrouted` is present while routing
/// has left the task without a route: `{outcome, reasons, task_version}`.
async fn routes(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(task): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    let items = history(&state.store.pool, task).await?;
    let items: Vec<Value> = (items.iter())
        .map(|r| json!({"sequence":r.sequence,"at_ms":r.at_ms,"route":r.route}))
        .collect();
    let mut body = json!({"items":items});
    // Why routing left the task without a route, while that holds.
    if let Some(unrouted) = super::unrouted(&state.store.pool, task).await? {
        body["unrouted"] = unrouted;
    }
    Ok(Json(body))
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
    task: Uuid,
    node: &Value,
    ready_state: &str,
) -> Result<Option<Value>, AppError> {
    let pool = &state.store.pool;
    let on = enabled(pool).await?;
    let updated = node["updated_at"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |t| t.timestamp_millis());
    Ok(Some(match Placement::of(true, on, &node["metadata"], 0) {
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
