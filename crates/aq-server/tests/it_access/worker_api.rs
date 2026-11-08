use std::sync::{Arc, Mutex};

use aq_protocol::{ApiError, CompatibilityError, EnrollResponse, HeartbeatResponse, ProbeResponse};
use aq_server::config::Config;
use aq_server::identity::verifier_from_config;
use aq_server::store::Store;
use aq_server::test_support::TestDb;
use aq_server::tokens::verifier;
use aq_server::{AppState, Clock, build_admin_router, build_worker_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct Harness {
    worker: Router,
    admin: Router,
    store: Store,
    now: Arc<Mutex<DateTime<Utc>>>,
    _db: TestDb,
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn config() -> Config {
    toml::from_str(
        r#"
format_version = 1
environment = "development"
[server]
admin_bind = "127.0.0.1:0"
worker_bind = "127.0.0.1:0"
public_origin = "http://127.0.0.1:8080"
[auth]
mode = "dev"
dev_email = "owner@localhost"
[bootstrap]
owner_email = "owner@localhost"
enabled = true
"#,
    )
    .unwrap()
}

async fn harness() -> Harness {
    let db = TestDb::create().await;
    let store = db.store().await;
    let config = config();
    let now = Arc::new(Mutex::new(t0()));
    let n = now.clone();
    let clock: Clock = Arc::new(move || *n.lock().unwrap());
    let state = AppState {
        agent_access: None,
        source_retention: None,
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store: store.clone(),
        clock,
    };
    Harness {
        worker: build_worker_router(state.clone()),
        admin: build_admin_router(state),
        store,
        now,
        _db: db,
    }
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    // The CSRF header lets requests to the admin router past the guard, so
    // 404 assertions prove the route is absent.
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("x-aq-request", "1");
    if let Some(t) = bearer {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn new_code(h: &Harness) -> String {
    let owner = h
        .store
        .bootstrap_owner("owner@localhost", "iss", "sub", t0())
        .await
        .unwrap();
    let owner_id = match owner {
        Some(u) => u.id,
        None => h.store.list_users().await.unwrap()[0].id,
    };
    let code = aq_server::tokens::new_enrollment_code();
    let now = *h.now.lock().unwrap();
    h.store
        .create_enrollment(
            &verifier(&code),
            owner_id,
            now,
            now + Duration::seconds(600),
        )
        .await
        .unwrap();
    code
}

fn enroll_body(code: &str) -> Value {
    json!({"code": code, "name": "box", "worker_version": "0.1.0", "protocol_version": 1})
}

async fn enroll(h: &Harness) -> EnrollResponse {
    let code = new_code(h).await;
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

fn hb() -> Value {
    json!({"worker_version": "0.1.0", "protocol_version": 1})
}

#[tokio::test]
async fn enroll_with_valid_code_returns_credential() {
    let h = harness().await;
    let r = enroll(&h).await;
    assert!(r.credential.starts_with("aqn_"));
    assert_eq!(r.heartbeat_interval_secs, 15);
    let nodes = h.store.list_nodes().await.unwrap();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, r.node_id);
    assert_eq!(nodes[0].name, "box");
    // Only the verifier is stored: the credential authenticates.
    let found = h
        .store
        .find_node_by_credential(&verifier(&r.credential))
        .await
        .unwrap();
    assert_eq!(found.unwrap().id, r.node_id);
}

#[tokio::test]
async fn reused_code_fails() {
    let h = harness().await;
    let code = new_code(&h).await;
    let (s, _) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "enrollment_invalid");
}

#[tokio::test]
async fn expired_code_fails() {
    let h = harness().await;
    let code = new_code(&h).await;
    *h.now.lock().unwrap() = t0() + Duration::seconds(601);
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "enrollment_invalid");
}

#[tokio::test]
async fn unknown_code_gives_same_response_as_used() {
    let h = harness().await;
    let code = new_code(&h).await;
    send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    let (_, used) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    let (_, unknown) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body("aqe_nope")),
    )
    .await;
    assert_eq!(used, unknown);
}

#[tokio::test]
async fn enroll_validates_input() {
    let h = harness().await;
    let code = new_code(&h).await;
    for body in [
        json!({"code": code, "name": "   ", "worker_version": "1", "protocol_version": 1}),
        json!({"code": code, "name": "x".repeat(65), "worker_version": "1", "protocol_version": 1}),
        json!({"code": code, "name": "ok", "worker_version": "", "protocol_version": 1}),
        json!({"code": code, "name": "ok", "worker_version": "v".repeat(65), "protocol_version": 1}),
        json!({"nope": 1}),
    ] {
        let (s, v) = send(&h.worker, "POST", "/v1/worker/enroll", None, Some(body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"], "bad_request");
    }
    // The code was never consumed by the rejected requests.
    let (s, _) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn heartbeat_updates_last_seen() {
    let h = harness().await;
    let r = enroll(&h).await;
    *h.now.lock().unwrap() = t0() + Duration::seconds(30);
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/heartbeat",
        Some(&r.credential),
        Some(hb()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let resp: HeartbeatResponse = serde_json::from_value(v).unwrap();
    assert_eq!(resp.heartbeat_interval_secs, 15);
    assert_eq!(resp.server_version, env!("CARGO_PKG_VERSION"));
    let n = &h.store.list_nodes().await.unwrap()[0];
    assert_eq!(n.last_seen_at, Some(t0() + Duration::seconds(30)));
}

#[tokio::test]
async fn revoked_credential_rejected_with_reason() {
    let h = harness().await;
    let r = enroll(&h).await;
    h.store.revoke_node(r.node_id, t0()).await.unwrap();
    *h.now.lock().unwrap() = t0() + Duration::seconds(30);
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/heartbeat",
        Some(&r.credential),
        Some(hb()),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "credential_revoked");
    // A revoked node must not update last_seen_at.
    assert_eq!(h.store.list_nodes().await.unwrap()[0].last_seen_at, None);
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/probe",
        Some(&r.credential),
        Some(json!({"nonce": "a"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "credential_revoked");
}

#[tokio::test]
async fn unknown_credential_rejected() {
    let h = harness().await;
    for auth in [Some("aqn_unknown"), Some("nope"), None] {
        let (s, v) = send(&h.worker, "POST", "/v1/worker/heartbeat", auth, Some(hb())).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "invalid_credential");
    }
    // Wrong scheme.
    let req = Request::post("/v1/worker/heartbeat")
        .header("authorization", "Basic aqn_x")
        .header("content-type", "application/json")
        .body(Body::from(hb().to_string()))
        .unwrap();
    let resp = h.worker.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unsupported_protocol_gets_426_diagnostic() {
    let h = harness().await;
    let code = new_code(&h).await;
    let body =
        json!({"code": code, "name": "box", "worker_version": "0.1.0", "protocol_version": 99});
    let (s, v) = send(&h.worker, "POST", "/v1/worker/enroll", None, Some(body)).await;
    assert_eq!(s, StatusCode::UPGRADE_REQUIRED);
    let e: CompatibilityError = serde_json::from_value(v).unwrap();
    assert_eq!(e.error, "unsupported_protocol");
    assert_eq!(e.supported_min, 1);
    assert_eq!(e.supported_max, 1);
    assert_eq!(
        e.message,
        "worker protocol 99 is not supported; this server accepts 1\u{2013}1. Install a compatible aq-worker release."
    );
    // The incompatible worker did not consume the code.
    let (s, _) = send(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body(&code)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn heartbeat_checks_protocol_before_credential() {
    let h = harness().await;
    let body = json!({"worker_version": "0.1.0", "protocol_version": 99});
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/heartbeat",
        Some("aqn_unknown"),
        Some(body),
    )
    .await;
    assert_eq!(s, StatusCode::UPGRADE_REQUIRED, "{v}");
    assert_eq!(v["supported_min"], 1);
}

#[tokio::test]
async fn worker_listener_has_no_admin_routes() {
    let h = harness().await;
    let (s, _) = send(&h.worker, "GET", "/api/status", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&h.worker, "POST", "/api/enrollments", None, None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_listener_has_no_worker_routes() {
    let h = harness().await;
    let (s, _) = send(
        &h.admin,
        "POST",
        "/v1/worker/enroll",
        None,
        Some(enroll_body("x")),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_on_both_and_hardening_headers() {
    let h = harness().await;
    for app in [&h.worker, &h.admin] {
        let resp = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
        assert_eq!(resp.headers()["cache-control"], "no-store");
    }
}

#[tokio::test]
async fn probe_echoes_nonce() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/probe",
        Some(&r.credential),
        Some(json!({"nonce": "abc123"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p: ProbeResponse = serde_json::from_value(v).unwrap();
    assert_eq!(p.nonce, "abc123");
    assert_eq!(p.server_time, t0());
    let (s, v) = send(
        &h.worker,
        "POST",
        "/v1/worker/probe",
        Some(&r.credential),
        Some(json!({"nonce": "x".repeat(65)})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let e: ApiError = serde_json::from_value(v).unwrap();
    assert_eq!(e.error, "bad_request");
}

#[tokio::test]
async fn oversized_body_rejected() {
    let h = harness().await;
    let big = json!({"nonce": "x".repeat(32 * 1024)});
    let r = enroll(&h).await;
    let (s, _) = send(
        &h.worker,
        "POST",
        "/v1/worker/probe",
        Some(&r.credential),
        Some(big),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

fn machine_report(cpu: u16) -> Value {
    let mut v: Value = serde_json::from_str(include_str!(
        "../../../../protocol/worker-machine/v1/fixtures/report-windows-host.json"
    ))
    .unwrap();
    v["cpu_busy_bps"] = json!(cpu);
    v
}

async fn post_machine(h: &Harness, credential: &str, body: Value) -> (StatusCode, Value) {
    send(
        &h.worker,
        "POST",
        "/v1/worker/machine",
        Some(credential),
        Some(body),
    )
    .await
}

fn now_ms(h: &Harness) -> u64 {
    h.now.lock().unwrap().timestamp_millis() as u64
}

#[tokio::test]
async fn machine_report_records_latest_and_window() {
    let h = harness().await;
    let r = enroll(&h).await;
    for (secs, cpu) in [(15, 6100), (30, 6300)] {
        *h.now.lock().unwrap() = t0() + Duration::seconds(secs);
        let (s, v) = post_machine(&h, &r.credential, machine_report(cpu)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v, json!({"recorded": true}));
    }
    let pool = h._db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let facts = aq_swarm::machine::latest(&mut conn, &[r.node_id], now_ms(&h))
        .await
        .unwrap();
    let f = &facts[&r.node_id];
    assert_eq!(f.cpu_avg_15m_bps, Some(6200));
    assert_eq!(f.samples_15m, 2);
    assert_eq!(f.report.cpu_busy_bps, 6300);
}

#[tokio::test]
async fn machine_samples_older_than_window_are_pruned() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (s, _) = post_machine(&h, &r.credential, machine_report(6100)).await;
    assert_eq!(s, StatusCode::OK);
    *h.now.lock().unwrap() = t0() + Duration::minutes(16);
    let (s, _) = post_machine(&h, &r.credential, machine_report(6300)).await;
    assert_eq!(s, StatusCode::OK);
    let pool = h._db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let facts = aq_swarm::machine::latest(&mut conn, &[r.node_id], now_ms(&h))
        .await
        .unwrap();
    assert_eq!(facts[&r.node_id].samples_15m, 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM node_machine_samples")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn machine_report_rejects_invalid_and_unknown_fields() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (s, _) = post_machine(&h, &r.credential, machine_report(10_001)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let mut extra = machine_report(100);
    extra["extra"] = json!(1);
    let (s, _) = post_machine(&h, &r.credential, extra).await;
    assert!(s.is_client_error(), "{s}");
}

#[tokio::test]
async fn machine_report_requires_credential() {
    let h = harness().await;
    let (s, _) = send(
        &h.worker,
        "POST",
        "/v1/worker/machine",
        None,
        Some(machine_report(100)),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

async fn samples_after_gaps(gaps_ms: [i64; 2]) -> (i64, i64) {
    let h = harness().await;
    let r = enroll(&h).await;
    let (mut at, mut last) = (0, 0);
    for (gap, cpu) in gaps_ms.into_iter().zip([6100, 6300]) {
        at += gap;
        *h.now.lock().unwrap() = t0() + Duration::milliseconds(at);
        let (s, v) = post_machine(&h, &r.credential, machine_report(cpu)).await;
        assert_eq!((s, v), (StatusCode::OK, json!({"recorded": true})));
        last = cpu as i64;
    }
    let pool = h._db.pool().await;
    let rows = sqlx::query_scalar("SELECT count(*) FROM node_machine_samples")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cpu =
        sqlx::query_scalar("SELECT (report->>'cpu_busy_bps')::bigint FROM node_machine_reports")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cpu, last, "the latest report is always updated");
    (rows, cpu)
}

#[tokio::test]
async fn machine_reports_one_second_apart_store_one_sample() {
    assert_eq!(samples_after_gaps([1_000, 1_000]).await.0, 1);
}

#[tokio::test]
async fn machine_reports_six_seconds_apart_store_two_samples() {
    assert_eq!(samples_after_gaps([1_000, 6_000]).await.0, 2);
}

fn slots_body(key: &str, slots: u32) -> Value {
    json!({"schema_version": 1, "machine_key": key, "slots": slots})
}

async fn post_slots(h: &Harness, bearer: Option<&str>, body: Value) -> (StatusCode, Value) {
    send(&h.worker, "POST", "/v1/worker/slots", bearer, Some(body)).await
}

#[tokio::test]
async fn slots_claim_key_and_mint_siblings_once_with_credentials() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (s, v) = post_slots(&h, Some(&r.credential), slots_body("desk", 3)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["machine_key"], "desk");
    let slots = v["slots"].as_array().unwrap().clone();
    assert_eq!(slots.len(), 2);
    for slot in &slots {
        let cred = slot["credential"].as_str().unwrap();
        assert!(cred.starts_with("aqn_"));
        let (s, _) = send(
            &h.worker,
            "POST",
            "/v1/worker/heartbeat",
            Some(cred),
            Some(hb()),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
    let pool = h._db.pool().await;
    let rows: Vec<(String, i32)> =
        sqlx::query_as("SELECT machine_key, slot FROM nodes ORDER BY slot")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [
            ("desk".to_string(), 1),
            ("desk".to_string(), 2),
            ("desk".to_string(), 3)
        ]
    );
    let (s, again) = post_slots(&h, Some(&r.credential), slots_body("desk", 3)).await;
    assert_eq!(s, StatusCode::OK);
    let again = again["slots"].as_array().unwrap().clone();
    assert_eq!(
        again.iter().map(|x| &x["node_id"]).collect::<Vec<_>>(),
        slots.iter().map(|x| &x["node_id"]).collect::<Vec<_>>()
    );
    assert!(again.iter().all(|x| x.get("credential").is_none()));
}

#[tokio::test]
async fn slots_refuses_sibling_caller_bounds_and_bad_key() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (_, v) = post_slots(&h, Some(&r.credential), slots_body("desk", 2)).await;
    let sibling = v["slots"][0]["credential"].as_str().unwrap().to_string();
    let (s, _) = post_slots(&h, Some(&sibling), slots_body("desk", 2)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    for body in [
        slots_body("desk", 0),
        slots_body("desk", 9),
        slots_body("Desk", 2),
    ] {
        let (s, _) = post_slots(&h, Some(&r.credential), body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
    let (s, _) = post_slots(&h, None, slots_body("desk", 2)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn slots_refuses_a_key_held_by_another_machine() {
    let h = harness().await;
    let a = enroll(&h).await;
    let b = enroll(&h).await;
    let (s, _) = post_slots(&h, Some(&a.credential), slots_body("desk", 1)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post_slots(&h, Some(&b.credential), slots_body("desk", 1)).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn slots_refused_after_machine_revoked() {
    let h = harness().await;
    let r = enroll(&h).await;
    let (s, _) = post_slots(&h, Some(&r.credential), slots_body("desk", 2)).await;
    assert_eq!(s, StatusCode::OK);
    let pool = h._db.pool().await;
    let mut c = pool.acquire().await.unwrap();
    aq_machine_control::revoke_machine(&mut c, "desk", t0())
        .await
        .unwrap();
    let (s, _) = post_slots(&h, Some(&r.credential), slots_body("desk", 2)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}
