//! Owner-started persistent sessions. Queue tasks and their authority are separate.
mod api;
mod stream;
mod terminal;
mod worker;
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;
#[derive(Clone, Default)]
pub struct Runtime(Arc<Mutex<BTreeMap<Uuid, terminal::Screen>>>);
pub type Auth =
    Arc<dyn Fn(HeaderMap) -> Pin<Box<dyn Future<Output = Result<Uuid>> + Send>> + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub struct Error(pub StatusCode, pub &'static str);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error":"manual_session_refused","message":self.1})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "Session storage is temporarily unavailable",
        )
    }
}
fn bad(s: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, s)
}
fn conflict(s: &'static str) -> Error {
    Error(StatusCode::CONFLICT, s)
}
fn missing() -> Error {
    Error(StatusCode::NOT_FOUND, "Session or worker not found")
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
#[derive(Clone)]
pub struct Service {
    pub pool: PgPool,
    pub owner: Auth,
    pub node: Auth,
    /// Only explicitly configured project source policies may be launched.
    pub sources: Arc<BTreeMap<Uuid, Value>>,
    runtime: Runtime,
    origin: Option<(String, bool)>,
}
impl Service {
    pub fn new(
        pool: PgPool,
        owner: Auth,
        node: Auth,
        sources: BTreeMap<Uuid, Value>,
        runtime: Runtime,
    ) -> Self {
        Self {
            pool,
            owner,
            node,
            sources: Arc::new(sources),
            runtime,
            origin: None,
        }
    }
    pub fn browser_origin(mut self, origin: String, development: bool) -> Self {
        self.origin = Some((origin, development));
        self
    }
    pub fn human_routes<S: Clone + Send + Sync + 'static>(&self) -> axum::Router<S> {
        api::routes()
            .merge(stream::human_routes())
            .with_state(self.clone())
    }
    pub fn worker_routes<S: Clone + Send + Sync + 'static>(&self) -> axum::Router<S> {
        worker::routes()
            .merge(stream::worker_routes())
            .with_state(self.clone())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: Uuid,
    pub node_id: Uuid,
    pub project_id: Uuid,
    pub spec: Value,
    pub status: String,
    pub stage: String,
    pub desired_running: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub worker_seen_at_ms: Option<i64>,
}
async fn session(s: &Service, id: Uuid) -> Result<Session> {
    let v:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(m) - 'actor_id' - 'stopped_at_ms' FROM manual_agent_sessions m WHERE id=$1").bind(id).fetch_optional(&s.pool).await?;
    serde_json::from_value(v.ok_or_else(missing)?).map_err(|_| bad("Invalid retained session"))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    operation_id: Uuid,
}
fn valid_id(id: Uuid) -> Result<()> {
    if id.is_nil() {
        Err(bad("Operation identity is required"))
    } else {
        Ok(())
    }
}

/// Manual launch sources come only from enabled installed project policies.
pub fn configured_sources<T: Serialize>(config: Option<&T>) -> BTreeMap<Uuid, Value> {
    config
        .and_then(|v| serde_json::to_value(v).ok())
        .and_then(|v| v["projects"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p["enabled"] == true)
        .filter_map(|p| {
            Some((
                serde_json::from_value(p["project_id"].clone()).ok()?,
                p.get("source")?.clone(),
            ))
        })
        .collect()
}
