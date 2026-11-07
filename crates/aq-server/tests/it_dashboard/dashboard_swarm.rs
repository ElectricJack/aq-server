//! Swarm read (spec 0007 phase 2): machine grouping, load, agents, states.
use crate::common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn read(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn swarm_fixture(value: &Value) {
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocol/dashboard-read/v1/fixtures/swarm.json");
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}

async fn add_node(pool: &sqlx::PgPool, name: &str, seen_ms: Option<u64>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,$2,$3,'t',1,now(),to_timestamp($4::double precision/1000))")
        .bind(id).bind(name).bind(id.to_string()).bind(seen_ms.map(|s| s as f64)).execute(pool).await.unwrap();
    id
}

fn windows_report() -> aq_swarm::machine::MachineReport {
    serde_json::from_str(include_str!(
        "../../../../protocol/worker-machine/v1/fixtures/report-windows-host.json"
    ))
    .unwrap()
}

#[tokio::test]
async fn swarm_requires_owner() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'swarm-viewer@localhost','viewer','dev','swarm-viewer@localhost',now())")
        .bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let viewer = app
        .oneshot(
            Request::get("/api/v1/swarm")
                .header("x-aq-dev-email", "swarm-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn swarm_groups_nodes_reports_load_and_lists_running_agents() {
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let second = add_node(&pool, "local-2", Some(now)).await;
    let registration = json!({"backend":{"kind":"docker-local","capabilities":[],"max_concurrent_sandboxes":2,"spend_ceiling":null,"templates":[]}});
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
        .bind(second)
        .bind(Uuid::new_v4())
        .bind(registration)
        .execute(&pool)
        .await
        .unwrap();
    for node in [f.node.node_id, second] {
        aq_swarm::machine::record(&pool, node, &windows_report(), now)
            .await
            .unwrap();
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let machines = body["data"]["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 1, "{body}");
    let machine = &machines[0];
    assert_eq!(machine["label"], "desktop");
    assert_eq!(machine["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(machine["load"]["source"], "windows_host");
    assert_eq!(machine["load"]["fresh"], true);

    let (_, list) = read(&app, "/api/v1/sessions").await;
    let session = list["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["attempt_id"] == json!(attempt))
        .unwrap()["id"]
        .clone();
    let (_, detail) = read(
        &app,
        &format!("/api/v1/sessions/{}", session.as_str().unwrap()),
    )
    .await;
    let agent = machine["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|n| n["agents"].as_array().unwrap())
        .find(|a| a["attempt_id"] == json!(attempt))
        .unwrap_or_else(|| panic!("agent missing: {body}"));
    assert_eq!(agent["provider"], detail["data"]["profile"]["provider"]);
    assert_eq!(agent["model"], detail["data"]["profile"]["model"]);
    assert!(body["data"]["server"]["running_agents"].as_u64().unwrap() >= 1);
    swarm_fixture(&body);
}

#[tokio::test]
async fn swarm_offline_node_without_report_is_its_own_offline_machine() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let cold = add_node(&pool, "cold", None).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let machine = body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == json!(format!("node:{cold}")))
        .unwrap();
    assert_eq!(machine["state"], "offline");
    assert!(machine["load"].is_null());
}

#[tokio::test]
async fn swarm_waiting_reason_when_no_machine_online() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    add_node(&pool, "cold", None).await;
    sqlx::query("UPDATE nodes SET last_seen_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["waiting"]["reasons"][0]["code"],
        "no_machine_online"
    );
}

#[tokio::test]
async fn bootstrap_advertises_swarm_read_for_owner() {
    let f = common::profiled::fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "swarm.read" && v["version"] == "1" && v["enabled"] == true)
    );
}
