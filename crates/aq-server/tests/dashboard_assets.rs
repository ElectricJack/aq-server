//! Real human/machine routers and PostgreSQL authorization; identity verification
//! is injected deterministically here, not a substitute for existing JWT tests.
use aq_server::{
    AppState, build_admin_router, build_admin_router_with_dashboard, build_worker_router,
    dashboard_assets::DashboardAssets,
    identity::{IdentityError, IdentityVerifier, VerifiedIdentity},
    store::Role,
    test_support::TestDb,
};
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;
const INDEX: &[u8] = b"<!doctype html><title>dashboard fixture</title>";
const JS: &[u8] = b"console.log('dashboard fixture')";
struct VerifiedFixture;
#[async_trait]
impl IdentityVerifier for VerifiedFixture {
    async fn verify(&self, headers: &HeaderMap) -> Result<VerifiedIdentity, IdentityError> {
        let email = headers
            .get("x-test-verified-email")
            .ok_or(IdentityError::Missing)?
            .to_str()
            .map_err(|_| IdentityError::Invalid("fixture header".into()))?;
        Ok(VerifiedIdentity {
            issuer: "fixture".into(),
            subject: email.into(),
            email: email.into(),
        })
    }
}
fn bundle() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("assets")).unwrap();
    std::fs::write(root.path().join("index.html"), INDEX).unwrap();
    std::fs::write(root.path().join("assets/app.js"), JS).unwrap();
    std::fs::write(
        root.path().join("assets/not-listed.js"),
        b"must never serve",
    )
    .unwrap();
    std::fs::write(root.path().join("dashboard-manifest.json"),serde_json::to_vec(&json!({"schema_version":1,"api_version":"dashboard-read-1","files":{"index.html":{"bytes":INDEX.len(),"sha256":hex::encode(Sha256::digest(INDEX))},"assets/app.js":{"bytes":JS.len(),"sha256":hex::encode(Sha256::digest(JS))}}})).unwrap()).unwrap();
    root
}
async fn state() -> (TestDb, AppState) {
    let db = TestDb::create().await;
    let store = db.store().await;
    let config = toml::from_str(
        r#"
format_version=1
environment="production"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="https://aq.example.test"
[auth]
mode="cloudflare_access"
team_domain="fixture.cloudflareaccess.com"
audience="fixture"
[bootstrap]
enabled=false
"#,
    )
    .unwrap();
    let now = chrono::Utc::now();
    store
        .bootstrap_owner("owner@example.test", "fixture", "owner@example.test", now)
        .await
        .unwrap();
    store
        .invite_user("viewer@example.test", Role::Viewer, now)
        .await
        .unwrap();
    let state = AppState {
        agent_access: None,
        source_retention: None,
        config: Arc::new(config),
        store,
        identity: Arc::new(VerifiedFixture),
        clock: Arc::new(chrono::Utc::now),
    };
    (db, state)
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    email: Option<&str>,
    csrf: bool,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(email) = email {
        builder = builder.header("x-test-verified-email", email);
    }
    if csrf {
        builder = builder
            .header("x-aq-request", "1")
            .header("origin", "https://aq.example.test");
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}
#[tokio::test]
async fn current_owner_viewer_and_removed_user_authenticate_each_get_and_head() {
    let (db, state) = state().await;
    let root = bundle();
    let assets = Arc::new(DashboardAssets::load(root.path()).unwrap());
    let app = build_admin_router_with_dashboard(state, assets);
    for path in [
        "/",
        "/assets/app.js",
        "/work/task_1",
        "/results/result-1/revisions/2",
        "/playbooks/runs/run-1",
    ] {
        let (status, headers, body) = request(&app, "GET", path, None, false).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(headers["content-type"], "application/json");
        assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
        for email in ["owner@example.test", "viewer@example.test"] {
            let (status, headers, body) = request(&app, "GET", path, Some(email), false).await;
            assert_eq!(status, StatusCode::OK, "{path} {email}");
            assert_eq!(headers["cache-control"], "no-store");
            assert_eq!(headers["x-content-type-options"], "nosniff");
            assert_eq!(
                body,
                if path.starts_with("/assets/") {
                    JS
                } else {
                    INDEX
                }
            );
        }
    }
    for path in ["/", "/assets/app.js"] {
        let (status, headers, body) =
            request(&app, "HEAD", path, Some("viewer@example.test"), false).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
        assert_eq!(
            headers["content-length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            if path == "/" { INDEX.len() } else { JS.len() }
        );
        assert_eq!(
            request(&app, "HEAD", path, None, false).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let pool = db.pool().await;
    sqlx::query("DELETE FROM users WHERE email='viewer@example.test'")
        .execute(&pool)
        .await
        .unwrap();
    for path in ["/", "/assets/app.js"] {
        assert_eq!(
            request(&app, "GET", path, Some("viewer@example.test"), false)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
}
#[tokio::test]
async fn exact_spa_paths_do_not_capture_api_machine_unknown_assets_or_traversal() {
    let (_db, state) = state().await;
    let root = bundle();
    let assets = Arc::new(DashboardAssets::load(root.path()).unwrap());
    let app = build_admin_router_with_dashboard(state.clone(), assets);
    let worker = build_worker_router(state.clone());
    let api_only = build_admin_router(state);
    for path in [
        "/overview",
        "/supervisor",
        "/work",
        "/work/a.b_-9",
        "/results",
        "/results/a/revisions/v1",
        "/decisions",
        "/decisions/a",
        "/agents",
        "/agents/a",
        "/playbooks",
        "/playbooks/runs/a",
        "/knowledge",
        "/knowledge/a",
        "/settings",
    ] {
        assert_eq!(
            request(&app, "GET", path, Some("owner@example.test"), false)
                .await
                .0,
            StatusCode::OK,
            "{path}"
        );
    }
    for path in [
        "/projects",
        "/index.html",
        "/unknown",
        "/api/missing",
        "/v1/worker/missing",
        "/assets/missing.js",
        "/assets/not-listed.js",
        "/assets/app.js.map",
        "/dashboard-manifest.json",
        "/work/..",
        "/work/%2e%2e",
        "/work/a%2fb",
        "/assets/../index.html",
        "/settings/",
        "/knowledge/a/b",
    ] {
        let (status, headers, body) =
            request(&app, "GET", path, Some("owner@example.test"), false).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(headers["content-type"], "application/json");
        assert!(
            serde_json::from_slice::<serde_json::Value>(&body).is_ok(),
            "{path}"
        );
    }
    let (status, headers, body) =
        request(&app, "GET", "/api/me", Some("owner@example.test"), false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    assert_eq!(
        request(&app, "POST", "/settings", Some("owner@example.test"), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, headers, _) =
        request(&app, "POST", "/settings", Some("owner@example.test"), true).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["content-type"], "application/json");
    for router in [&worker, &api_only] {
        for path in ["/", "/work/a", "/assets/app.js"] {
            let (status, headers, _) =
                request(router, "GET", path, Some("owner@example.test"), false).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(headers["content-type"], "application/json");
        }
    }
}
#[tokio::test]
async fn actual_local_human_http_serves_loaded_bytes_after_source_removal() {
    let (_db, state) = state().await;
    let root = bundle();
    let assets = Arc::new(DashboardAssets::load(root.path()).unwrap());
    let app = build_admin_router_with_dashboard(state, assets);
    root.close().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    for (path, expected) in [("/agents/physical-session", INDEX), ("/assets/app.js", JS)] {
        let response = client
            .get(format!("http://{address}{path}"))
            .header("x-test-verified-email", "viewer@example.test")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), expected);
    }
    assert_eq!(
        client
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    task.abort();
    let _ = task.await;
}
