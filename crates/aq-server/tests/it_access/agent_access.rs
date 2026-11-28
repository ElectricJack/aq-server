//! Real PostgreSQL authority tests; no graph or credential authority is mocked.
use crate::agent_launch::{Launch, enroll, send, state};
use aq_server::{
    AppState,
    agent_access::{AgentAccessConfig, Runtime},
    test_support::TestDb,
    tokens,
};
use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;
#[cfg(feature = "project_environments")]
mod environment_try;
mod fixture;
mod locks;
mod maintenance;
mod pages;
mod provision;
mod reads;
mod rotation;
/// Spec 0017 §6.2: with no git route for try pushes (no aq-git
/// configured, as here), a try can never be verified, so it is
/// refused as unavailable rather than "run it again".
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn environment_try_without_a_git_route_is_unavailable() {
    let grant = json!([{"operation":"environment.try","scope":"project:self"}]);
    let e = crate::messaging::setup_with(Some(grant)).await;
    let path = "/v1/agent/environments/try";
    for body in [
        json!({"api_version":1,"name":"web","commit":"c".repeat(40)}),
        json!({"api_version":1,"name":"web"}),
    ] {
        let (status, body) = crate::messaging::send(&e.app, path, Some(&e.token), Some(body)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "try_unavailable", "{body}");
        assert_eq!(body["retryable"], false, "{body}");
    }
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_jobs")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

struct Case {
    _db: TestDb,
    _directory: tempfile::TempDir,
    node: Uuid,
    node_token: String,
    launch: Launch,
    attempt_id: Uuid,
    app: Router,
    state: AppState,
    time: Arc<std::sync::atomic::AtomicI64>,
    pool: sqlx::PgPool,
    fake: Arc<FakeGraph>,
    graph: tokio::task::JoinHandle<()>,
}
#[tokio::test]
async fn issuance_rotation_revocation_and_locked_authority_are_online() {
    let mut case = Case::new().await;
    let small_profile = case.provision().await;
    let issue = case.issue(0);
    case.reject_identity(&issue).await;
    case.reject_small_response(&issue, small_profile).await;
    case.update_node_token(&issue).await;
    let agent = case.issue_and_rotate(issue).await;
    case.visible_graph(&agent).await;
    case.scope_vectors(&agent).await;
    if std::env::var_os("AQ_AGENT_GRAPH_TEST_URL").is_none() {
        let paged =
            json!({"kind":"search","text":"paged","kind_filter":null,"limit":1,"cursor":null});
        let first = case.first_page(&agent, &paged).await;
        let next = case.cursor_replay(&agent, paged, first).await;
        for expire in [true, false] {
            case.read_during_graph_barrier(&agent, expire).await;
        }
        case.rotation_expires_cursor(&agent, next).await;
    }
    case.expire_queued_issuance().await;
    case.expire_queued_read(&agent).await;
    case.maintenance(&agent).await;
    case.cancellation(&agent).await;
    case.finish();
}

struct FakeGraph {
    data: Value,
    hold: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
async fn fake_graph(
    axum::extract::State(f): axum::extract::State<Arc<FakeGraph>>,
    uri: axum::http::Uri,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ([(&'static str, &'static str); 1], axum::Json<Value>) {
    if f.hold.swap(false, std::sync::atomic::Ordering::SeqCst) {
        f.arrived.notify_one();
        f.release.notified().await;
    }
    let path = uri.path();
    let data = if path.ends_with("/search") {
        search(&f, &query)
    } else if path.ends_with("/context") {
        f.data["context"].clone()
    } else if path.ends_with("/history") {
        f.data["history"].clone()
    } else if path.contains("/revisions/") {
        f.data["revision"].clone()
    } else {
        f.data["nodes"][path.rsplit('/').next().unwrap()].clone()
    };
    ([("X-AQ-Graph-API-Version", "1")], axum::Json(data))
}

fn search(f: &FakeGraph, query: &std::collections::HashMap<String, String>) -> Value {
    if query.get("q").is_some_and(|q| q == "hidden") {
        json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000004"]],"next_cursor":"hidden-next"})
    } else if query.get("q").is_some_and(|q| q == "paged") {
        match query.get("cursor").map(String::as_str) {
            None => {
                json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000004"]],"next_cursor":"visible-one"})
            }
            Some("visible-one") => {
                json!({"items":[f.data["context"]["node"]],"next_cursor":"visible-two"})
            }
            Some("visible-two") => {
                json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000002"]],"next_cursor":null})
            }
            _ => panic!("unexpected cursor"),
        }
    } else {
        json!({"items":f.data["nodes"].as_object().unwrap().values().filter(|n|query.get("kind").is_none_or(|kind|n["kind"].as_str()==Some(kind.as_str()))).cloned().collect::<Vec<_>>(),"next_cursor":null})
    }
}
