//! Agent wait routes (per-attempt credential) and owner G10 read routes.
use super::super::{Error, authenticate, ok};
use crate::{AppState, authz::RequireOwner, error::AppError};
use aq_wait_engine::wire;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

/// Route → command capability. Published in `protocol/agent-waits/v1/routes.json`.
pub const ROUTES: [(&str, &str); 3] = [
    ("/v1/agent/waits/register", wire::WAIT_CAPABILITY),
    ("/v1/agent/waits/inspect", wire::INSPECT_CAPABILITY),
    ("/v1/agent/waits/ack", wire::ACK_CAPABILITY),
];

pub fn machine_routes() -> Router<AppState> {
    Router::new()
        .route(ROUTES[0].0, post(register))
        .route(ROUTES[1].0, post(inspect))
        .route(ROUTES[2].0, post(ack))
}
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/waits", get(owner_list))
        .route("/api/waits/{wait_id}", get(owner_show))
}

async fn call(state: AppState, headers: HeaderMap, body: Bytes, route: usize) -> Response {
    let principal = match authenticate(&state, &headers).await {
        Ok(p) => p,
        Err(error) => return error.into_response(),
    };
    let request: wire::Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return Error("invalid_request").into_response(),
    };
    if request.body.command.capability() != ROUTES[route].1 {
        return Error("unsupported_capability").into_response();
    }
    let (Ok(node), Ok(attempt)) = (
        super::super::id(&principal.binding, "node_id"),
        super::super::id(&principal.binding, "attempt_id"),
    ) else {
        return Error("unauthorized").into_response();
    };
    let result = match route {
        0 => super::register(&state, node, attempt, &request).await,
        1 => super::inspect(&state, node, attempt, &request).await,
        _ => super::ack(&state, node, attempt, &request).await,
    };
    match result.and_then(|r| Ok(serde_json::to_value(r)?)) {
        Ok(value) => ok(value),
        Err(error) => Error(error.0).into_response(),
    }
}
async fn register(State(s): State<AppState>, h: HeaderMap, b: Bytes) -> Response {
    call(s, h, b, 0).await
}
async fn inspect(State(s): State<AppState>, h: HeaderMap, b: Bytes) -> Response {
    call(s, h, b, 1).await
}
async fn ack(State(s): State<AppState>, h: HeaderMap, b: Bytes) -> Response {
    call(s, h, b, 2).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    project_id: Option<Uuid>,
}
async fn owner_list(
    State(s): State<AppState>,
    RequireOwner(_): RequireOwner,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    super::owner_list(&s, q.project_id)
        .await
        .map(Json)
        .map_err(|_| AppError::UpdatesUnavailable("Wait registry unavailable".into()))
}
async fn owner_show(
    State(s): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(wait_id): Path<Uuid>,
) -> Result<Json<Value>, AppError> {
    match super::owner_show(&s, wait_id).await {
        Ok(Some(v)) => Ok(Json(v)),
        Ok(None) => Err(AppError::NotFound("Wait not found".into())),
        Err(_) => Err(AppError::UpdatesUnavailable(
            "Wait registry unavailable".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Published fixtures decode as sealed requests whose command capability
    /// matches the route table.
    #[test]
    fn wait_route_fixtures_decode() {
        let raw = include_str!("../../../../../protocol/agent-waits/v1/fixtures.json");
        let fixtures: serde_json::Map<String, Value> = serde_json::from_str(raw).unwrap();
        let routes: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-waits/v1/routes.json"
        ))
        .unwrap();
        for (path, capability) in ROUTES {
            let entry = routes["routes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["path"] == path)
                .unwrap();
            assert_eq!(entry["capability"], capability);
            let fixture = &fixtures[entry["fixture"].as_str().unwrap()];
            let request = wire::decode_request(&serde_json::to_vec(fixture).unwrap()).unwrap();
            assert_eq!(request.body.command.capability(), capability);
        }
    }
}
