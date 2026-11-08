//! Worker control actions (spec 0007 phase 1): start, stop, drain and revoke.
use crate::common::{self, profiled::Fixture};
use aq_machine_control as mc;
use aq_server::execution::{
    authority::hot::{HotState, LaunchOwner},
    profiled::{Status, Stop},
};
use aq_session_protocol::ProfiledGrant;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const FIXTURES: &str = "../../protocol/dashboard-actions/v1/fixtures";

fn fixture_body(kind: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join(format!("request-worker-{kind}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// A request body for `kind` on `machine`, with a fresh operation id.
fn body(kind: &str, machine: &str, generation: &str) -> Value {
    let mut v = fixture_body(kind);
    let id = Uuid::new_v4().to_string();
    v["operation_id"] = json!(id);
    v["payload"]["operation_id"] = json!(id);
    v["subject"]["id"] = json!(machine);
    v["payload"]["machine"] = json!(machine);
    if kind != "revoke" {
        v["payload"]["expected_generation"] = json!(generation);
    }
    v
}

async fn act(app: &Router, email: &str, body: &Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", email)
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A fixture whose node holds the key "desktop" with `slots` slot nodes.
async fn machine(slots: u32) -> (Fixture, PgPool, Router) {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let mut c = pool.acquire().await.unwrap();
    let caller = mc::claim_key(&mut c, f.node.node_id, "desktop")
        .await
        .unwrap();
    if slots > 1 {
        let hashes: Vec<String> = (0..slots).map(|_| Uuid::new_v4().to_string()).collect();
        let new = mc::NewSlot {
            base_name: "profiled-fixture",
            worker_version: "0.1.0",
            protocol_version: 1,
        };
        mc::mint_slots(&mut c, &caller, slots, &new, &hashes, chrono::Utc::now())
            .await
            .unwrap();
    }
    drop(c);
    let app = aq_server::build_admin_router(f.state.clone());
    (f, pool, app)
}

/// Makes the fixture's attempt a held, granted, running one.
async fn running_attempt(f: &Fixture, pool: &PgPool) -> Uuid {
    let now = common::profiled::NOW;
    let attempt = common::profiled::insert_offered(f, now).await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(pool).await.unwrap();
    let set = |hot: &HotState| {
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(json!(hot))
            .execute(pool)
    };
    hot.status = Status::Preparing;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    set(&hot).await.unwrap();
    hot.status = Status::Granted;
    hot.stop = Stop::Running;
    hot.execution_deadline_ms = Some(now + 60_000);
    hot.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: now + 1_000,
    });
    set(&hot).await.unwrap();
    attempt
}

async fn control(pool: &PgPool) -> (String, i64) {
    sqlx::query_as("SELECT desired, generation FROM machine_controls WHERE machine_key='desktop'")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn owner_stop_sets_stopped_and_requests_handoffs() {
    let (f, pool, app) = machine(1).await;
    let attempt = running_attempt(&f, &pool).await;
    let (s, v) = act(&app, "owner@localhost", &body("stop", "desktop", "1")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
    assert_eq!(control(&pool).await, ("stopped".into(), 2));
    let rows: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT attempt_id, reason FROM attempt_handoffs")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, [(attempt, "owner_stop".to_string())]);
}

#[tokio::test]
async fn drain_and_start_change_state_without_handoffs() {
    let (f, pool, app) = machine(1).await;
    running_attempt(&f, &pool).await;
    let (_, v) = act(&app, "owner@localhost", &body("drain", "desktop", "1")).await;
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
    assert_eq!(control(&pool).await, ("draining".into(), 2));
    let (_, v) = act(&app, "owner@localhost", &body("start", "desktop", "2")).await;
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
    assert_eq!(control(&pool).await, ("running".into(), 3));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM attempt_handoffs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn stale_generation_is_a_conflict() {
    let (_f, _pool, app) = machine(1).await;
    let (_, v) = act(&app, "owner@localhost", &body("stop", "desktop", "1")).await;
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
    let (_, v) = act(&app, "owner@localhost", &body("start", "desktop", "1")).await;
    assert_eq!(v["data"]["disposition"], "conflict", "{v}");
    assert_eq!(v["data"]["current_versions"]["machine"], "2", "{v}");
}

#[tokio::test]
async fn legacy_machine_is_not_found() {
    let (f, _pool, app) = machine(1).await;
    for machine in [
        "unknown-box".to_string(),
        format!("node:{}", f.node.node_id),
    ] {
        let (s, v) = act(&app, "owner@localhost", &body("stop", &machine, "1")).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{machine}: {v}");
    }
}

#[tokio::test]
async fn revoke_revokes_every_slot_node() {
    let (f, pool, app) = machine(3).await;
    sqlx::query("UPDATE nodes SET last_seen_at=now()")
        .execute(&pool)
        .await
        .unwrap();
    let req = body("revoke", "desktop", "");
    let (s, v) = act(&app, "owner@localhost", &req).await;
    assert_eq!(
        (s, &v["data"]["disposition"]),
        (StatusCode::OK, &json!("confirmed")),
        "{v}"
    );
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes WHERE machine_key='desktop' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
    let swarm = app
        .clone()
        .oneshot(
            Request::get("/api/v1/swarm")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = swarm.into_body().collect().await.unwrap().to_bytes();
    let swarm: Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&Value> = swarm["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["id"])
        .collect();
    assert!(!ids.contains(&&json!("desktop")), "{swarm}");
    // Revoking again, under a new operation, is confirmed again.
    let (s, v) = act(&app, "owner@localhost", &body("revoke", "desktop", "")).await;
    assert_eq!(
        (s, &v["data"]["disposition"]),
        (StatusCode::OK, &json!("confirmed")),
        "{v}"
    );
    drop(f);
}

/// Review I4: a slot of a service machine is not revoked alone, through the
/// action or the legacy route; the answer says to revoke the machine.
#[tokio::test]
async fn revoking_one_slot_of_a_machine_is_refused() {
    let (f, pool, app) = machine(2).await;
    let slot = f.node.node_id;
    let (s, v) = act(
        &app,
        "owner@localhost",
        &body("revoke", &format!("node:{slot}"), ""),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "machine_node", "{v}");
    let legacy = app
        .clone()
        .oneshot(
            Request::post(format!("/api/workers/{slot}/revoke"))
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::CONFLICT);
    let bytes = legacy.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("revoke the whole machine") && text.contains("desktop"),
        "{text}"
    );
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes WHERE machine_key='desktop' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 2);
}

#[tokio::test]
async fn non_owner_is_refused() {
    let (_f, pool, app) = machine(1).await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'worker-viewer@localhost','viewer','dev','worker-viewer@localhost',now())")
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let (s, v) = act(
        &app,
        "worker-viewer@localhost",
        &body("stop", "desktop", "1"),
    )
    .await;
    assert!(
        s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND,
        "{s} {v}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM machine_controls")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn bootstrap_lists_worker_capabilities() {
    let (_f, _pool, app) = machine(1).await;
    let response = app
        .oneshot(
            Request::get("/api/v1/bootstrap")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    for name in ["swarm.worker.control", "swarm.worker.enroll"] {
        let c = v["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing: {v}"));
        assert_eq!((&c["version"], &c["enabled"]), (&json!("1"), &json!(true)));
    }
}

/// Spec 0007 Q2: a `swarm.availability.set` body with a fresh operation id.
fn availability(available: bool, generation: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join("request-swarm-availability-set.json");
    let mut v: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let id = Uuid::new_v4().to_string();
    v["operation_id"] = json!(id);
    v["payload"]["operation_id"] = json!(id);
    v["payload"]["available"] = json!(available);
    v["payload"]["expected_generation"] = json!(generation);
    v
}

/// A fixture with an intelligence registry of two enabled credentials and one disabled.
async fn with_registry() -> (Fixture, PgPool, Router) {
    use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
    let mut f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let credential = |name: &str, enabled: bool| CredentialConfig {
        kind: Default::default(),
        allowed_backends: aq_selection::default_allowed_backends(),
        credential: aq_selection::AuthorizedCredential {
            profile: common::profiled::artifact(name, name.as_bytes()),
            provider: "synthetic-provider".into(),
            account_reference: format!("{name}-account"),
            endpoint_reference: "endpoint-a".into(),
            revision: 1,
            enabled,
            provisionable: false,
            material_owner: None,
            allowed_backends: aq_selection::default_allowed_backends(),
        },
        allowed_node_ids: vec![],
        allow_enrolled_nodes: true,
        secret_file: None,
        delivery_mode: None,
    };
    let registry = IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: vec![
            credential("account-a", true),
            credential("account-b", true),
            credential("account-off", false),
        ],
        qualified_harnesses: vec![],
    };
    let mut config = (*f.state.config).clone();
    config.intelligence = Some(std::sync::Arc::new(registry.load().unwrap()));
    f.state.config = std::sync::Arc::new(config);
    let app = aq_server::build_admin_router(f.state.clone());
    (f, pool, app)
}

async fn owner_rows(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT observation FROM intelligence_observations WHERE observation->'source'->>'kind'='operator' ORDER BY observation->'scope'->>'account_reference'")
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Spec 0007 Q2 (fix round 1, C1): the switch writes no observation row;
/// selection synthesizes the owner's attestation from it, so nothing ages.
#[tokio::test]
async fn availability_switch_writes_no_observation_rows() {
    let (_f, pool, app) = with_registry().await;
    let before = owner_rows(&pool).await;
    for (on, generation) in [(true, "1"), (false, "2"), (true, "3")] {
        let (s, v) = act(&app, "owner@localhost", &availability(on, generation)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
        assert_eq!(owner_rows(&pool).await, before);
    }
}

#[tokio::test]
async fn availability_stale_generation_is_a_conflict() {
    let (_f, _pool, app) = with_registry().await;
    let (_, v) = act(&app, "owner@localhost", &availability(true, "1")).await;
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
    let (_, v) = act(&app, "owner@localhost", &availability(false, "1")).await;
    assert_eq!(v["data"]["disposition"], "conflict", "{v}");
    assert_eq!(
        v["data"]["current_versions"]["owner_availability"], "2",
        "{v}"
    );
}

#[tokio::test]
async fn swarm_read_reports_owner_availability() {
    let (_f, _pool, app) = with_registry().await;
    let read = || async {
        let response = app
            .clone()
            .oneshot(
                Request::get("/api/v1/swarm")
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<Value>(&bytes).unwrap()["data"]["server"]["owner_availability"]
            .clone()
    };
    assert_eq!(
        read().await,
        json!({"state":"unset","generation":"1","since":null})
    );
    act(&app, "owner@localhost", &availability(true, "1")).await;
    let on = read().await;
    assert_eq!(
        (&on["state"], &on["generation"]),
        (&json!("on"), &json!("2")),
        "{on}"
    );
    assert!(on["since"].is_string(), "{on}");
    act(&app, "owner@localhost", &availability(false, "2")).await;
    let off = read().await;
    assert_eq!(
        (&off["state"], &off["generation"]),
        (&json!("off"), &json!("3")),
        "{off}"
    );
}

#[tokio::test]
async fn bootstrap_lists_availability_capability() {
    let capability = |app: Router| async move {
        let response = app
            .oneshot(
                Request::get("/api/v1/bootstrap")
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        v["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "swarm.availability.control")
            .cloned()
            .unwrap_or_else(|| panic!("missing: {v}"))
    };
    let (_f, _pool, app) = with_registry().await;
    let c = capability(app).await;
    assert_eq!((&c["version"], &c["enabled"]), (&json!("1"), &json!(true)));
    // Without an intelligence registry the switch is listed but disabled.
    let (_f, _pool, app) = machine(1).await;
    assert_eq!(capability(app).await["enabled"], json!(false));
}
