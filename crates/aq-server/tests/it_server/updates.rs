use crate::common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use aq_release::ReleaseManifest;
use aq_server::config::Config;
use aq_server::identity::CloudflareAccessVerifier;
use aq_server::test_support::TestDb;
use aq_server::{AppState, Clock, build_admin_router, build_worker_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use common::{TestKeys, serve_jwks};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const ISS: &str = "https://team.cloudflareaccess.com";
const AUD: &str = "aud-tag-123";
const DIGEST_S: &str = "ghcr.io/electricjack/agent-q-server@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_W: &str = "ghcr.io/electricjack/agent-q-worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Harness {
    admin: Router,
    worker: Router,
    spool: tempfile::TempDir,
    _db: TestDb,
    _jwks: common::JwksServer,
}

fn config(spool: Option<&Path>) -> Config {
    let updates = spool
        .map(|p| format!("[updates]\nspool_dir = {:?}\n", p.display().to_string()))
        .unwrap_or_default();
    toml::from_str(&format!(
        r#"
format_version = 1
environment = "production"
[server]
admin_bind = "127.0.0.1:8080"
worker_bind = "127.0.0.1:8081"
public_origin = "https://aq.example.com"
[auth]
mode = "cloudflare_access"
team_domain = "{ISS}"
audience = "{AUD}"
[bootstrap]
owner_email = "owner@example.com"
enabled = true
{updates}
"#
    ))
    .unwrap()
}

async fn harness(with_spool: bool) -> Harness {
    let db = TestDb::create().await;
    let store = db.store().await;
    let jwks = serve_jwks(TestKeys::get().jwk_set_json()).await;
    let verifier = CloudflareAccessVerifier::with_jwks_url(
        ISS.into(),
        AUD.into(),
        jwks.base_url.clone(),
        reqwest::Client::new(),
    );
    let spool = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(spool.path().join("requests")).unwrap();
    std::fs::create_dir_all(spool.path().join("status")).unwrap();
    let clock: Clock = Arc::new(Utc::now);
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: None,
        config: Arc::new(config(with_spool.then(|| spool.path()))),
        store,
        identity: Arc::new(verifier),
        clock,
    };
    Harness {
        admin: build_admin_router(state.clone()),
        worker: build_worker_router(state),
        spool,
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
    // Invite via the owner API so no store handle is needed.
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/users",
        &owner_token(),
        Some(json!({"email": "viewer@example.com", "role": "viewer"})),
    )
    .await;
    assert!(s.is_success(), "{s}");
    token("viewer-sub", "viewer@example.com")
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    tok: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let b = Request::builder()
        .method(method)
        .uri(path)
        .header("cf-access-jwt-assertion", tok)
        .header("x-aq-request", "1");
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

fn write_status(h: &Harness, state: Value) {
    let body = json!({
        "updater_version": "0.1.0",
        "state": state,
        "active": {"version": "0.1.0", "source_revision": "abc", "server_image": DIGEST_S, "state_format": 1},
        "previous": null,
        "available": {"version": "0.2.0", "source_revision": "def", "server_image": DIGEST_S, "state_format": 1},
        "history": [],
        "updated_at": "2026-01-01T00:00:00Z",
    });
    std::fs::write(h.spool.path().join("status/status.json"), body.to_string()).unwrap();
}

fn request_files(h: &Harness) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(h.spool.path().join("requests"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect()
}

fn manifest_cmd(version: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aq-server"))
        .args(["manifest", "--server-image", DIGEST_S])
        .args(["--worker-image", DIGEST_W])
        .args(["--version", version, "--source-revision", "deadbeef"])
        .output()
        .unwrap()
}

#[test]
fn manifest_subcommand_uses_binary_constants() {
    let out = manifest_cmd(env!("CARGO_PKG_VERSION"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_ne!(out.stdout.last(), Some(&b'\n'), "no trailing newline");
    let m = ReleaseManifest::from_json(&out.stdout).unwrap();
    assert_eq!(m.to_canonical_json(), out.stdout, "stdout is canonical");
    assert_eq!(m.version.to_string(), env!("CARGO_PKG_VERSION"));
    assert_eq!(m.source_revision, "deadbeef");
    assert_eq!(m.images.server, DIGEST_S);
    assert_eq!(m.images.worker, DIGEST_W);
    let (min, max) = aq_protocol::SUPPORTED_WORKER_PROTOCOLS;
    assert_eq!((m.protocol.min, m.protocol.max), (min, max));
    assert_eq!(m.state_format, aq_server::STATE_FORMAT_VERSION);
    assert_eq!(m.readable_state_formats, aq_server::READABLE_STATE_FORMATS);
    assert_eq!(
        m.upgrade_from.to_string(),
        aq_server::UPGRADE_FROM.replace(' ', "")
    );
    let age = Utc::now() - m.created_at;
    assert!(age.num_seconds().abs() < 60);
}

#[test]
fn manifest_rejects_version_mismatch() {
    let out = manifest_cmd("9.9.9");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("9.9.9") && err.contains(env!("CARGO_PKG_VERSION")),
        "{err}"
    );
}

#[test]
fn manifest_rejects_unpinned_image() {
    let out = Command::new(env!("CARGO_BIN_EXE_aq-server"))
        .args(["manifest", "--server-image", "ghcr.io/x/y:latest"])
        .args(["--worker-image", DIGEST_W])
        .args([
            "--version",
            env!("CARGO_PKG_VERSION"),
            "--source-revision",
            "r",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
}

#[tokio::test]
async fn maintenance_blocks_admin_mutations_and_enroll() {
    let h = harness(true).await;
    std::fs::write(h.spool.path().join("status/maintenance"), "").unwrap();
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/enrollments",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"], "maintenance");
    // Reads still work.
    let (s, _) = call(&h.admin, "GET", "/api/status", &owner_token(), None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = call(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        "",
        Some(json!({"code": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"], "maintenance");
    // Marker removed: no longer gated.
    std::fs::remove_file(h.spool.path().join("status/maintenance")).unwrap();
    let (s, _) = call(
        &h.worker,
        "POST",
        "/v1/worker/enroll",
        "",
        Some(json!({"code": "x"})),
    )
    .await;
    assert_ne!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn maintenance_allows_heartbeat() {
    let h = harness(true).await;
    std::fs::write(h.spool.path().join("status/maintenance"), "").unwrap();
    let (s, v) = call(
        &h.worker,
        "POST",
        "/v1/worker/heartbeat",
        "",
        Some(json!({})),
    )
    .await;
    // Unauthenticated, so rejected for credentials, not for maintenance.
    assert_ne!(v["error"], "maintenance");
    assert_ne!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn no_spool_config_means_nothing_gated() {
    let h = harness(false).await;
    std::fs::write(h.spool.path().join("status/maintenance"), "").unwrap();
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/enrollments",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert!(s.is_success(), "{s}");
}

#[tokio::test]
async fn updates_unavailable_without_spool_config() {
    let h = harness(false).await;
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"], "updates_unavailable");
    let (s, v) = call(&h.admin, "GET", "/api/updates", &owner_token(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "unavailable");
    let (_, v) = call(&h.admin, "GET", "/api/status", &owner_token(), None).await;
    assert_eq!(v["update"]["state"], "unavailable");
}

#[tokio::test]
async fn missing_status_file_reads_unavailable() {
    let h = harness(true).await;
    let (s, v) = call(&h.admin, "GET", "/api/updates", &owner_token(), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"], "unavailable");
    assert!(v["detail"].is_string());
}

#[tokio::test]
async fn viewer_reads_update_status() {
    let h = harness(true).await;
    write_status(&h, json!({"kind": "idle"}));
    let tok = viewer_token(&h).await;
    let (s, v) = call(&h.admin, "GET", "/api/updates", &tok, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["state"]["kind"], "idle");
    assert_eq!(v["available"]["version"], "0.2.0");
    let (s, v) = call(&h.admin, "GET", "/api/status", &tok, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["update"]["state"], "idle");
    assert_eq!(v["update"]["active_version"], "0.1.0");
    assert_eq!(v["update"]["available_version"], "0.2.0");
}

#[tokio::test]
async fn status_maps_update_states() {
    let h = harness(true).await;
    let cases = [
        (
            json!({"kind": "in_progress", "request_id": "6f1c2a52-0000-4000-8000-000000000001",
                   "target": "0.2.0", "step": "pulled", "started_at": "2026-01-01T00:00:00Z"}),
            "in_progress",
        ),
        (
            json!({"kind": "failed_needs_operator", "detail": "x"}),
            "failed_needs_operator",
        ),
        (json!({"kind": "from_the_future"}), "unknown"),
    ];
    for (state, want) in cases {
        write_status(&h, state);
        let (_, v) = call(&h.admin, "GET", "/api/status", &owner_token(), None).await;
        assert_eq!(v["update"]["state"], want);
    }
}

#[tokio::test]
async fn viewer_cannot_request_update() {
    let h = harness(true).await;
    let tok = viewer_token(&h).await;
    let (s, _) = call(&h.admin, "POST", "/api/updates", &tok, Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn owner_request_writes_spool_file() {
    let h = harness(true).await;
    write_status(&h, json!({"kind": "idle"}));
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({"version": "0.2.0"})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let id = v["request_id"].as_str().unwrap().to_string();
    let files = request_files(&h);
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].file_name().unwrap().to_str().unwrap(),
        format!("{id}.json")
    );
    let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
    let body: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["id", "requested_at", "requested_by", "version"]);
    assert_eq!(body["id"], id);
    assert_eq!(body["version"], "0.2.0");
    assert_eq!(body["requested_by"], "owner@example.com");
    // The updater's own parser accepts it.
    let _: aq_release::UpdateRequest = serde_json::from_value(body).unwrap();
}

#[tokio::test]
async fn owner_request_without_version_omits_it() {
    let h = harness(true).await;
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&std::fs::read(&request_files(&h)[0]).unwrap()).unwrap();
    assert!(body.get("version").is_none());
}

#[tokio::test]
async fn request_rejected_while_in_progress() {
    let h = harness(true).await;
    write_status(
        &h,
        json!({"kind": "in_progress", "request_id": "6f1c2a52-0000-4000-8000-000000000001",
               "target": "0.2.0", "step": "pulled", "started_at": "2026-01-01T00:00:00Z"}),
    );
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "update_in_progress");
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn second_request_rejected_while_one_is_queued() {
    let h = harness(true).await;
    write_status(&h, json!({"kind": "idle"}));
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "update_in_progress");
    assert_eq!(request_files(&h).len(), 1);
}

#[tokio::test]
async fn dotfile_in_requests_does_not_block() {
    let h = harness(true).await;
    std::fs::write(h.spool.path().join("requests/.tmp-x"), "{}").unwrap();
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn invalid_version_rejected() {
    let h = harness(true).await;
    for bad in ["latest", "1.2", "", "v1.0.0", "1.0.0; rm -rf /"] {
        let (s, v) = call(
            &h.admin,
            "POST",
            "/api/updates",
            &owner_token(),
            Some(json!({"version": bad})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(v["error"], "invalid_version", "{bad}");
    }
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn request_body_rejects_image_field() {
    let h = harness(true).await;
    let (s, _) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({"version": "1.0.0", "image": "evil"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn unknown_state_blocks_request() {
    let h = harness(true).await;
    write_status(&h, json!({"kind": "from_the_future"}));
    let (s, v) = call(
        &h.admin,
        "POST",
        "/api/updates",
        &owner_token(),
        Some(json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["error"], "update_in_progress");
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn prerelease_and_build_versions_rejected() {
    let h = harness(true).await;
    for bad in [
        "1.0.0-rc.1",
        "1.0.0+build5",
        &format!("1.0.0-{}", "a".repeat(5000)),
    ] {
        let (s, v) = call(
            &h.admin,
            "POST",
            "/api/updates",
            &owner_token(),
            Some(json!({"version": bad})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid_version");
    }
    assert!(request_files(&h).is_empty());
}

#[tokio::test]
async fn maintenance_allows_probe() {
    let h = harness(true).await;
    std::fs::write(h.spool.path().join("status/maintenance"), "").unwrap();
    let (s, v) = call(&h.worker, "POST", "/v1/worker/probe", "", Some(json!({}))).await;
    assert_ne!(v["error"], "maintenance");
    assert_ne!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn manifest_stdout_is_exact_under_trace_logging() {
    let out = Command::new(env!("CARGO_BIN_EXE_aq-server"))
        .env("RUST_LOG", "trace")
        .args(["manifest", "--server-image", DIGEST_S])
        .args(["--worker-image", DIGEST_W])
        .args([
            "--version",
            env!("CARGO_PKG_VERSION"),
            "--source-revision",
            "r",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let m = ReleaseManifest::from_json(&out.stdout).unwrap();
    assert_eq!(m.to_canonical_json(), out.stdout);
}
