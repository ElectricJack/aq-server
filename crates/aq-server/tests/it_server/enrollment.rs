use crate::common;

use std::sync::Arc;
use std::time::Duration;

use aq_server::admin_api::worker_status;
use aq_server::config::Config;
use aq_server::identity::{CloudflareAccessVerifier, DevIdentity};
use aq_server::store::{NewNode, Node, Role, Store};
use aq_server::test_support::TestDb;
use aq_server::tokens::verifier;
use aq_server::{AppState, Clock, build_admin_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, TimeZone, Utc};
use common::{TestKeys, serve_jwks};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const ISS: &str = "https://team.cloudflareaccess.com";
const AUD: &str = "aud-tag-123";
const ORIGIN: &str = "https://aq.example.com";

struct Harness {
    app: Router,
    store: Store,
    _db: TestDb,
    _jwks: common::JwksServer,
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn config() -> Config {
    toml::from_str(&format!(
        r#"
format_version = 1
environment = "production"
[server]
admin_bind = "127.0.0.1:8080"
worker_bind = "127.0.0.1:8081"
public_origin = "{ORIGIN}"
[auth]
mode = "cloudflare_access"
team_domain = "{ISS}"
audience = "{AUD}"
[bootstrap]
owner_email = "owner@example.com"
enabled = true
"#
    ))
    .unwrap()
}

async fn harness() -> Harness {
    let db = TestDb::create().await;
    let store = db.store().await;
    let jwks = serve_jwks(TestKeys::get().jwk_set_json()).await;
    let verifier = CloudflareAccessVerifier::with_jwks_url(
        ISS.into(),
        AUD.into(),
        jwks.base_url.clone(),
        reqwest::Client::new(),
    );
    let clock: Clock = Arc::new(t0);
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: None,
        config: Arc::new(config()),
        store: store.clone(),
        identity: Arc::new(verifier),
        clock,
    };
    let app = build_admin_router(state);
    Harness {
        app,
        store,
        _db: db,
        _jwks: jwks,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn token(sub: &str, email: &str) -> String {
    TestKeys::get().mint(json!({
        "iss": ISS, "aud": [AUD], "sub": sub, "email": email,
        "exp": unix_now() + 300, "iat": unix_now(),
    }))
}

fn owner_token() -> String {
    token("owner-sub", "owner@example.com")
}

async fn viewer_token(h: &Harness) -> String {
    h.store
        .invite_user("viewer@example.com", Role::Viewer, t0())
        .await
        .unwrap();
    token("viewer-sub", "viewer@example.com")
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    tok: &str,
    body: Option<Value>,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("cf-access-jwt-assertion", tok);
    if csrf {
        b = b.header("x-aq-request", "1");
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
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn enroll_node(h: &Harness, name: &str) -> Node {
    let code = format!("aqe_{name}");
    let owner = h
        .store
        .bootstrap_owner("owner@example.com", ISS, "owner-sub", t0())
        .await
        .unwrap();
    let owner_id = match owner {
        Some(u) => u.id,
        None => h.store.list_users().await.unwrap()[0].id,
    };
    h.store
        .create_enrollment(
            &verifier(&code),
            owner_id,
            t0(),
            t0() + chrono::Duration::seconds(600),
        )
        .await
        .unwrap();
    h.store
        .redeem_enrollment(
            &verifier(&code),
            NewNode {
                name: name.into(),
                credential_hash: verifier(&format!("cred-{name}")),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            t0(),
        )
        .await
        .unwrap()
        .unwrap()
}

fn node(last_seen: Option<DateTime<Utc>>, revoked: Option<DateTime<Utc>>) -> Node {
    Node {
        id: Uuid::new_v4(),
        name: "n".into(),
        worker_version: "0.1.0".into(),
        protocol_version: 1,
        enrolled_at: t0(),
        last_seen_at: last_seen,
        revoked_at: revoked,
    }
}

#[tokio::test]
async fn owner_creates_enrollment_code() {
    let h = harness().await;
    let (s, v) = call(
        &h.app,
        "POST",
        "/api/enrollments",
        &owner_token(),
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let code = v["code"].as_str().unwrap();
    assert!(code.starts_with("aqe_"));
    let expires: DateTime<Utc> = v["expires_at"].as_str().unwrap().parse().unwrap();
    assert_eq!(expires, t0() + chrono::Duration::seconds(600));
    // The code is redeemable by its verifier, so what we stored matches.
    let redeemed = h
        .store
        .redeem_enrollment(
            &verifier(code),
            NewNode {
                name: "w".into(),
                credential_hash: "x".into(),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            t0(),
        )
        .await
        .unwrap();
    assert!(redeemed.is_some());
}

#[tokio::test]
async fn viewer_cannot_create_enrollment_via_direct_api() {
    let h = harness().await;
    let t = viewer_token(&h).await;
    let (s, v) = call(&h.app, "POST", "/api/enrollments", &t, None, true).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "forbidden");
}

#[tokio::test]
async fn viewer_cannot_revoke() {
    let h = harness().await;
    let n = enroll_node(&h, "w1").await;
    let t = viewer_token(&h).await;
    let (s, _) = call(
        &h.app,
        "POST",
        &format!("/api/workers/{}/revoke", n.id),
        &t,
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let nodes = h.store.list_nodes().await.unwrap();
    assert!(nodes[0].revoked_at.is_none());
}

#[tokio::test]
async fn viewer_cannot_manage_users() {
    let h = harness().await;
    let t = viewer_token(&h).await;
    let (s, _) = call(&h.app, "GET", "/api/users", &t, None, false).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = call(
        &h.app,
        "POST",
        "/api/users",
        &t,
        Some(json!({"email": "x@example.com", "role": "owner"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[test]
fn worker_status_offline_after_60s() {
    let off = Duration::from_secs(60);
    let n = node(Some(t0()), None);
    assert_eq!(
        worker_status(&n, t0() + chrono::Duration::seconds(60), off),
        "online"
    );
    assert_eq!(
        worker_status(&n, t0() + chrono::Duration::seconds(61), off),
        "offline"
    );
}

#[test]
fn never_seen_worker_is_offline() {
    let n = node(None, None);
    assert_eq!(worker_status(&n, t0(), Duration::from_secs(60)), "offline");
}

#[test]
fn revoked_status_wins() {
    let n = node(Some(t0()), Some(t0()));
    assert_eq!(worker_status(&n, t0(), Duration::from_secs(60)), "revoked");
}

#[tokio::test]
async fn status_reports_update_unavailable() {
    let h = harness().await;
    let (s, v) = call(&h.app, "GET", "/api/status", &owner_token(), None, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["health"], "ok");
    assert_eq!(v["update"]["state"], "unavailable");
    assert!(v["update"]["active_version"].is_null());
    assert!(v["update"]["available_version"].is_null());
    assert_eq!(v["version"]["server_version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn me_reports_email_role_and_auth_mode() {
    let h = harness().await;
    let (s, v) = call(&h.app, "GET", "/api/me", &owner_token(), None, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        json!({"email": "owner@example.com", "role": "owner", "auth_mode": "cloudflare_access"})
    );
}

#[tokio::test]
async fn workers_list_and_revoke() {
    let h = harness().await;
    let n = enroll_node(&h, "w1").await;
    h.store
        .record_heartbeat(n.id, "0.2.0", 1, t0())
        .await
        .unwrap();
    let (s, v) = call(&h.app, "GET", "/api/workers", &owner_token(), None, false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let w = &v[0];
    assert_eq!(w["id"], n.id.to_string());
    assert_eq!(w["name"], "w1");
    assert_eq!(w["worker_version"], "0.2.0");
    assert_eq!(w["protocol_version"], 1);
    assert_eq!(w["status"], "online");
    assert!(w["enrolled_at"].is_string());
    assert!(w["last_seen_at"].is_string());
    assert!(w.get("credential_hash").is_none());

    let path = format!("/api/workers/{}/revoke", n.id);
    let (s, _) = call(&h.app, "POST", &path, &owner_token(), None, true).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = call(&h.app, "GET", "/api/workers", &owner_token(), None, false).await;
    assert_eq!(v[0]["status"], "revoked");
}

#[tokio::test]
async fn revoke_unknown_is_404_and_malformed_is_not_500() {
    let h = harness().await;
    let p = format!("/api/workers/{}/revoke", Uuid::new_v4());
    let (s, v) = call(&h.app, "POST", &p, &owner_token(), None, true).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "not_found");
    let (s, v) = call(
        &h.app,
        "POST",
        "/api/workers/not-a-uuid/revoke",
        &owner_token(),
        None,
        true,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "bad_request");
}

#[tokio::test]
async fn every_mutation_requires_csrf_header_via_admin_routes() {
    let h = harness().await;
    let uid = Uuid::new_v4();
    let revoke = format!("/api/workers/{uid}/revoke");
    for (path, body) in [
        ("/api/enrollments", None),
        (revoke.as_str(), None),
        (
            "/api/users",
            Some(json!({"email": "a@example.com", "role": "viewer"})),
        ),
    ] {
        let (s, v) = call(&h.app, "POST", path, &owner_token(), body, false).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(v["error"], "csrf_rejected", "{path}");
    }
    assert_eq!(h.store.list_users().await.unwrap().len(), 0);
}

#[tokio::test]
async fn owner_manages_users() {
    let h = harness().await;
    let t = owner_token();
    let (s, _) = call(
        &h.app,
        "POST",
        "/api/users",
        &t,
        Some(json!({"email": "new@example.com", "role": "viewer"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = call(&h.app, "GET", "/api/users", &t, None, false).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.as_array().unwrap().iter().any(|u| {
        u["email"] == "new@example.com" && u["role"] == "viewer" && u["bound"] == false
    }));
    assert!(v.as_array().unwrap().iter().any(|u| {
        u["email"] == "owner@example.com" && u["role"] == "owner" && u["bound"] == true
    }));

    // Duplicate, case-insensitively.
    let (s, v) = call(
        &h.app,
        "POST",
        "/api/users",
        &t,
        Some(json!({"email": "New@Example.com", "role": "viewer"})),
        true,
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "conflict");
}

#[tokio::test]
async fn user_creation_validates_input() {
    let h = harness().await;
    let t = owner_token();
    for body in [
        json!({"email": "a@example.com", "role": "admin"}),
        json!({"email": "a@example.com", "role": "viewer", "extra": 1}),
        json!({"email": "not-an-email", "role": "viewer"}),
        json!({"email": "", "role": "viewer"}),
    ] {
        let (s, _) = call(&h.app, "POST", "/api/users", &t, Some(body.clone()), true).await;
        assert!(
            s == StatusCode::BAD_REQUEST || s == StatusCode::UNPROCESSABLE_ENTITY,
            "{body} -> {s}"
        );
    }
}

#[tokio::test]
async fn me_reports_dev_auth_mode() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let cfg: Config = toml::from_str(&format!(
        r#"
format_version = 1
environment = "development"
[server]
admin_bind = "127.0.0.1:8080"
worker_bind = "127.0.0.1:8081"
public_origin = "{ORIGIN}"
[auth]
mode = "dev"
dev_email = "dev@example.com"
[bootstrap]
owner_email = "dev@example.com"
enabled = true
"#
    ))
    .unwrap();
    let clock: Clock = Arc::new(t0);
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: None,
        config: Arc::new(cfg),
        store,
        identity: Arc::new(DevIdentity::new("dev@example.com".into())),
        clock,
    };
    let app = build_admin_router(state);
    let req = Request::builder()
        .uri("/api/me")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        v,
        json!({"email": "dev@example.com", "role": "owner", "auth_mode": "dev"})
    );
}
