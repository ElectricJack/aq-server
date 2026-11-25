//! Real Graph + SQL operation owner. The proxy loses responses after real commits.
use crate::common;
use aq_server::{
    agent_access::{AgentAccessConfig, Runtime, human},
    test_support::TestDb,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Digest as _;
use sqlx::ConnectOptions;
use std::{
    os::unix::fs::PermissionsExt,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;
async fn send(app: &Router, path: &str, body: Option<&str>, email: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("x-aq-dev-email", email)
        .header("content-type", "application/json")
        .header("x-aq-request", "1")
        .body(Body::from(body.unwrap_or("").to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let code = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        code,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
fn request(id: Uuid, revision: &str) -> String {
    json!({"operation_id":id,"expected_project_revision":revision,"title":"Human authored task","body":"Durable source body","acceptance_criteria":"Exactly one actual Graph task","labels":["human"],"task_state":"draft"}).to_string()
}
fn status_schema(body: &Value) {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-task-create/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, body),
        "invalid public status: {body}"
    );
}
#[tokio::test]
async fn owner_command_requires_configured_mutation_service_and_bounded_closed_input() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let app = human::routes(&f.state).with_state(f.state.clone());
    let path = format!(
        "/api/human/projects/{}/tasks",
        f.selection.lineage.project_id
    );
    assert!(!human::task_create_available(&f.state));
    assert_eq!(
        send(
            &app,
            &path,
            Some(&request(Uuid::new_v4(), "1")),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for body in [
        "{}".to_owned(),
        request(Uuid::nil(), "1"),
        request(Uuid::new_v4(), "01"),
        request(Uuid::new_v4(), "0"),
        "{\"operation_id\":1,\"operation_id\":2}".into(),
        // 300 characters but 600 UTF-8 bytes: titles are bounded in bytes,
        // like task.edit and Graph's own 512-byte cap.
        request(Uuid::new_v4(), "1").replace("Human authored task", &"\u{e9}".repeat(300)),
    ] {
        assert_eq!(
            send(&app, &path, Some(&body), "owner@localhost").await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(&app, &path, Some(&"x".repeat(65537)), "owner@localhost")
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    // Spec 0014 §5.5: an unknown role is refused by name with the roles that
    // exist, and `profile_hint` is an unknown field.
    let with = |field: &str| {
        let extra = format!("{field},\"task_state\"");
        request(Uuid::new_v4(), "1").replace("\"task_state\"", &extra)
    };
    let unknown = with("\"role\":\"tester\"");
    let (status, body) = send(&app, &path, Some(&unknown), "owner@localhost").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["message"],
        "Role `tester` does not exist. The roles are: developer, reviewer."
    );
    let old = with("\"profile_hint\":\"coding\"");
    let (status, _) = send(&app, &path, Some(&old), "owner@localhost").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM human_graph_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn mounted_task_capability_requires_current_owner_and_mutation_identity() {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())")
        .bind(viewer).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (code, absent) = send(&app, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{absent}");
    assert!(!enabled(&absent));
    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, "mounted-capability-fixture-token").unwrap();
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut cfg = AgentAccessConfig {
        graph_url: "http://127.0.0.1:9".into(),
        graph_token_file: token_path,
        mutation_actor_id: None,
        credential_ttl_ms: 120000,
    };
    f.state.agent_access = Some(Arc::new(Runtime::new(&cfg).await.unwrap()));
    let read_only = aq_server::build_admin_router(f.state.clone());
    let (_, body) = send(&read_only, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert!(!enabled(&body));
    cfg.mutation_actor_id = Some("mounted-fixture-service".into());
    f.state.agent_access = Some(Arc::new(Runtime::new(&cfg).await.unwrap()));
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    let (_, body) = send(&standalone, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert!(
        !enabled(&body),
        "configuration cannot infer mounted command authority"
    );
    let mounted = aq_server::build_admin_router(f.state.clone());
    let (code, owner) = send(&mounted, "/api/v1/bootstrap", None, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{owner}");
    assert!(enabled(&owner));
    let (code, viewer) = send(&mounted, "/api/v1/bootstrap", None, "viewer@localhost").await;
    assert_eq!(code, StatusCode::OK, "{viewer}");
    assert!(!enabled(&viewer));
    assert_mounted_routes(&mounted, f.selection.lineage.project_id).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM human_graph_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "capability inspection and refused requests have no Graph intent"
    );
}
fn enabled(body: &Value) -> bool {
    let cap = body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "tasks.create")
        .expect("explicit task capability");
    assert_eq!(cap["version"], "1");
    cap["enabled"].as_bool().unwrap()
}
async fn assert_mounted_routes(mounted: &Router, project: Uuid) {
    let path = format!("/api/human/projects/{}/tasks", project);
    assert_eq!(
        send(mounted, &path, Some("{}"), "owner@localhost").await.0,
        StatusCode::BAD_REQUEST,
        "advertised command route must actually exist"
    );
    assert_eq!(
        send(mounted, &path, Some("{}"), "viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    let missing_header = mounted
        .clone()
        .oneshot(
            Request::post(&path)
                .header("x-aq-dev-email", "owner@localhost")
                .header("content-type", "application/json")
                .body(Body::from(request(Uuid::new_v4(), "1")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing_header.status(),
        StatusCode::FORBIDDEN,
        "mounted mutation remains behind CSRF guard"
    );
    let op = Uuid::new_v4();
    assert_eq!(
        send(
            mounted,
            &format!("/api/human/operations/{op}"),
            None,
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn actual_graph_creation_response_loss_recovery_and_authority_fences() {
    let mut scenario = Scenario::new().await;
    let original = scenario.confirm_creation().await;
    scenario.assert_stale_revision().await;
    let lost = scenario.confirm_lost_response().await;
    scenario.check_native_lease().await;
    scenario.check_union_revocation().await;
    let (blocked, count) = scenario.check_recovery_fences().await;
    scenario.check_foreign_owner(original, lost, count).await;
    scenario.check_union_replay(count).await;
    scenario.check_legacy_discovery(blocked).await;
    scenario.check_worker_launch().await;
    scenario.shutdown().await;
}
mod authority;
mod discovery;
mod fixture;
mod launch;
mod retirement;
use fixture::Scenario;
