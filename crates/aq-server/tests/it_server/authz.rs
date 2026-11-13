use crate::common;

use std::sync::Arc;

use aq_protocol::ApiError;
use aq_server::authz::{CurrentUser, RequireOwner, csrf_guard};
use aq_server::config::Config;
use aq_server::identity::CloudflareAccessVerifier;
use aq_server::store::{Role, Store};
use aq_server::test_support::TestDb;
use aq_server::{AppState, Clock};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use chrono::{TimeZone, Utc};
use common::{TestKeys, serve_jwks};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const ISS: &str = "https://team.cloudflareaccess.com";
const AUD: &str = "aud-tag-123";
const ORIGIN: &str = "https://aq.example.com";

struct Harness {
    app: Router,
    store: Store,
    _db: TestDb,
    _jwks: common::JwksServer,
}

fn config(bootstrap_enabled: bool) -> Config {
    toml::from_str(&format!(
        r#"
format_version = 1
environment = "production"
[server]
admin_bind = "127.0.0.1:8080"
worker_bind = "127.0.0.1:8081"
public_origin = "{ORIGIN}/"
[auth]
mode = "cloudflare_access"
team_domain = "team.cloudflareaccess.com"
audience = "{AUD}"
[bootstrap]
owner_email = "Owner@Example.com"
enabled = {bootstrap_enabled}
"#
    ))
    .unwrap()
}

async fn harness(bootstrap_enabled: bool) -> Harness {
    let db = TestDb::create().await;
    let store = db.store().await;
    let jwks = serve_jwks(TestKeys::get().jwk_set_json()).await;
    let verifier = CloudflareAccessVerifier::with_jwks_url(
        ISS.into(),
        AUD.into(),
        jwks.base_url.clone(),
        reqwest::Client::new(),
    );
    let clock: Clock = Arc::new(|| Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: None,
        config: Arc::new(config(bootstrap_enabled)),
        store: store.clone(),
        identity: Arc::new(verifier),
        clock,
    };
    let app = Router::new()
        .route(
            "/probe-viewer",
            get(|u: CurrentUser| async move { u.0.email }),
        )
        .route(
            "/probe-owner",
            post(|u: RequireOwner| async move { u.0.email }),
        )
        .route(
            "/probe-owner-get",
            get(|u: RequireOwner| async move { u.0.email }),
        )
        .layer(from_fn_with_state(state.clone(), csrf_guard))
        .with_state(state);
    Harness {
        app,
        store,
        _db: db,
        _jwks: jwks,
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn token(sub: &str, email: &str) -> String {
    TestKeys::get().mint(json!({
        "iss": ISS, "aud": [AUD], "sub": sub, "email": email,
        "exp": now() + 300, "iat": now(),
    }))
}

fn req(method: &str, path: &str, tok: Option<&str>, extra: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(path);
    if let Some(t) = tok {
        b = b.header("cf-access-jwt-assertion", t);
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

async fn send(app: &Router, r: Request<Body>) -> (StatusCode, String) {
    let resp = app.clone().oneshot(r).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn api_error(body: &str) -> ApiError {
    serde_json::from_str(body).unwrap_or_else(|_| panic!("not an ApiError: {body}"))
}

#[tokio::test]
async fn first_visitor_is_not_owner() {
    let h = harness(true).await;
    let t = token("s1", "stranger@example.com");
    let (s, body) = send(&h.app, req("GET", "/probe-viewer", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let e = api_error(&body);
    assert_eq!(e.error, "forbidden");
    assert_eq!(
        e.message,
        "stranger@example.com is not authorized for this Agent Q installation"
    );
    assert!(h.store.list_users().await.unwrap().is_empty());
}

#[tokio::test]
async fn unauthenticated_without_token() {
    let h = harness(true).await;
    let (s, body) = send(&h.app, req("GET", "/probe-viewer", None, &[])).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(api_error(&body).error, "unauthenticated");
}

#[tokio::test]
async fn configured_email_bootstraps_owner() {
    let h = harness(true).await;
    let t = token("s1", "owner@example.com");
    let (s, body) = send(&h.app, req("GET", "/probe-viewer", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let users = h.store.list_users().await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].role, Role::Owner);
    // Second request resolves by binding.
    let (s, _) = send(&h.app, req("GET", "/probe-owner-get", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn bootstrap_disabled_blocks_owner_creation() {
    let h = harness(false).await;
    let t = token("s1", "owner@example.com");
    let (s, _) = send(&h.app, req("GET", "/probe-viewer", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(h.store.list_users().await.unwrap().is_empty());
}

#[tokio::test]
async fn bootstrap_happens_once() {
    let h = harness(true).await;
    let t1 = token("sub-1", "owner@example.com");
    let (s, _) = send(&h.app, req("GET", "/probe-viewer", Some(&t1), &[])).await;
    assert_eq!(s, StatusCode::OK);
    let t2 = token("sub-2", "owner@example.com");
    let (s, _) = send(&h.app, req("GET", "/probe-viewer", Some(&t2), &[])).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(h.store.list_users().await.unwrap().len(), 1);
}

#[tokio::test]
async fn invited_viewer_binds_on_first_login() {
    let h = harness(true).await;
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    h.store
        .invite_user("Viewer@Example.com", Role::Viewer, now)
        .await
        .unwrap();
    let t = token("v1", "viewer@example.com");
    let (s, body) = send(&h.app, req("GET", "/probe-viewer", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let u = h
        .store
        .find_user_by_binding(ISS, "v1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(u.role, Role::Viewer);
}

#[tokio::test]
async fn viewer_cannot_call_owner_route() {
    let h = harness(true).await;
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    h.store
        .invite_user("viewer@example.com", Role::Viewer, now)
        .await
        .unwrap();
    let t = token("v1", "viewer@example.com");
    let (s, body) = send(
        &h.app,
        req(
            "POST",
            "/probe-owner",
            Some(&t),
            &[("x-aq-request", "1"), ("origin", ORIGIN)],
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(api_error(&body).error, "forbidden");
}

#[tokio::test]
async fn mutation_without_csrf_header_rejected() {
    let h = harness(true).await;
    let t = token("s1", "owner@example.com");
    let (s, body) = send(&h.app, req("POST", "/probe-owner", Some(&t), &[])).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(api_error(&body).error, "csrf_rejected");
    // Rejected before any user resolution.
    assert!(h.store.list_users().await.unwrap().is_empty());
}

#[tokio::test]
async fn mutation_with_foreign_origin_rejected() {
    let h = harness(true).await;
    let t = token("s1", "owner@example.com");
    for origin in ["https://evil.example", "null"] {
        let (s, body) = send(
            &h.app,
            req(
                "POST",
                "/probe-owner",
                Some(&t),
                &[("x-aq-request", "1"), ("origin", origin)],
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(api_error(&body).error, "csrf_rejected");
    }
}

#[tokio::test]
async fn mutation_with_header_and_matching_origin_allowed() {
    let h = harness(true).await;
    let t = token("s1", "owner@example.com");
    for extra in [
        vec![("x-aq-request", "1"), ("origin", ORIGIN)],
        vec![("x-aq-request", "1")],
    ] {
        let (s, body) = send(&h.app, req("POST", "/probe-owner", Some(&t), &extra)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
    }
}

#[tokio::test]
async fn no_cors_headers_emitted() {
    let h = harness(true).await;
    let t = token("s1", "owner@example.com");
    let resp = h
        .app
        .clone()
        .oneshot(req("GET", "/probe-viewer", Some(&t), &[("origin", ORIGIN)]))
        .await
        .unwrap();
    assert!(
        resp.headers()
            .keys()
            .all(|k| !k.as_str().starts_with("access-control-"))
    );
}

async fn concurrent_first_logins(h: &Harness, tok: &str) {
    const N: usize = 8;
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let app = h.app.clone();
            let b = barrier.clone();
            let r = req("GET", "/probe-viewer", Some(tok), &[]);
            tokio::spawn(async move {
                b.wait().await;
                send(&app, r).await
            })
        })
        .collect();
    for hd in handles {
        let (s, body) = hd.await.unwrap();
        assert_eq!(s, StatusCode::OK, "{body}");
    }
    assert_eq!(h.store.list_users().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_logins_bootstrap_owner() {
    let h = harness(true).await;
    concurrent_first_logins(&h, &token("s1", "owner@example.com")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_logins_invited_viewer() {
    let h = harness(true).await;
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    h.store
        .invite_user("viewer@example.com", Role::Viewer, now)
        .await
        .unwrap();
    concurrent_first_logins(&h, &token("v1", "viewer@example.com")).await;
}

#[test]
fn development_alone_accepts_the_loopback_alias_of_the_configured_origin() {
    for (expected, alias) in [
        ("http://127.0.0.1:28080", "http://localhost:28080"),
        ("http://localhost:28080", "http://127.0.0.1:28080"),
        ("http://127.0.0.1", "http://localhost"),
    ] {
        assert!(aq_server::authz::origin_allowed(
            expected,
            expected.as_bytes(),
            false
        ));
        assert!(
            aq_server::authz::origin_allowed(expected, alias.as_bytes(), true),
            "{alias}"
        );
        assert!(
            !aq_server::authz::origin_allowed(expected, alias.as_bytes(), false),
            "{alias}"
        );
    }
    for other in [
        "http://localhost:8080",
        "https://localhost:28080",
        "http://localhost.evil:28080",
        "http://127.0.0.2:28080",
        "null",
    ] {
        assert!(
            !aq_server::authz::origin_allowed("http://127.0.0.1:28080", other.as_bytes(), true),
            "{other}"
        );
    }
    assert!(!aq_server::authz::origin_allowed(
        "https://aq.example.com",
        b"http://localhost",
        true
    ));
}
