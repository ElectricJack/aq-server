//! Owner Settings for the swarm through the real owner router and PostgreSQL:
//! the provider/class limits, whose classes are the level documents of the
//! active role snapshot (spec 0014 §5.2), and the provider list.
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

/// A store with the shipped roles and profiles active, as after start, and
/// its owner router.
pub(crate) async fn setup() -> (TestDb, Router, sqlx::PgPool) {
    setup_with("").await
}
pub(crate) async fn setup_with(extra: &str) -> (TestDb, Router, sqlx::PgPool) {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", Utc::now())
        .await
        .unwrap()
        .unwrap();
    let config: Config = toml::from_str(&format!(
        r#"format_version=1
environment="development"
{extra}
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
    ))
    .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let pool = db.pool().await;
    (db, aq_server::build_admin_router(state), pool)
}
pub(crate) async fn call(
    app: &Router,
    path: &str,
    body: Option<Value>,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json");
    if csrf {
        req = req.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}

mod providers;
mod swarm_settings;
