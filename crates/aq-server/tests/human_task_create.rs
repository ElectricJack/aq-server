//! Real Graph + SQL operation owner. The proxy loses responses after real commits.
mod common;
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
        "../../../protocol/human-task-create/v1/schema.json"
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
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM human_graph_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn actual_graph_creation_response_loss_recovery_and_authority_fences() {
    let binary = std::env::var("AQ_TEST_GRAPH_BINARY").unwrap();
    assert_eq!(
        hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
        std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
    );
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let graph_db = TestDb::create().await;
    let dir = tempfile::tempdir().unwrap();
    let token = "human-create-isolated-fixture-token";
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, token).unwrap();
    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let workspace = f.selection.lineage.workspace_id;
    let mut child = tokio::process::Command::new(binary)
        .args([
            "serve",
            "--bind",
            &address.to_string(),
            "--bootstrap-workspace",
            &workspace.to_string(),
            "--bootstrap-token-file",
            token_path.to_str().unwrap(),
            "--pool-size",
            "2",
        ])
        .env("DATABASE_URL", graph_db.options().to_url_lossy().as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let origin = format!("http://{address}");
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..200 {
        if client
            .get(format!("{origin}/health/ready"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ready);
    // A real forwarding boundary, never a fabricated Graph success or receipt.
    let calls = Arc::new(AtomicUsize::new(0));
    let lose = Arc::new(AtomicBool::new(false));
    let observed = Arc::new(tokio::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let hold = Arc::new(AtomicBool::new(false));
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let wait = hold.clone();
    let arrived_proxy = arrived.clone();
    let release_proxy = release.clone();
    let journal_pool = pool.clone();
    let upstream = origin.clone();
    let n = calls.clone();
    let drop_response = lose.clone();
    let seen = observed.clone();
    let proxy = Router::new().route(
        "/{*path}",
        axum::routing::post(move |request: axum::extract::Request| {
            let upstream = upstream.clone();
            let n = n.clone();
            let drop_response = drop_response.clone();
            let seen = seen.clone();
            let journal_pool = journal_pool.clone();
            let wait = wait.clone(); let arrived = arrived_proxy.clone(); let release = release_proxy.clone();
            async move {
                let uri = request.uri().to_string();
                let body = axum::body::to_bytes(request.into_body(), 131072)
                    .await
                    .unwrap();
                let mapped: Value = serde_json::from_slice(&body).unwrap();
                let id: Uuid = mapped["operations"][0]["id"].as_str().unwrap().parse().unwrap();
                let retained: Vec<u8> = sqlx::query_scalar("SELECT payload_bytes FROM human_graph_operations WHERE operation_id=$1 AND dispatch_count>0 AND status->>'state'='unknown'").bind(id).fetch_one(&journal_pool).await.unwrap();
                assert_eq!(retained, body.as_ref(), "exact durable intent must precede Graph IO");
                seen.lock().await.push(body.to_vec());
                n.fetch_add(1, Ordering::SeqCst);
                let res = reqwest::Client::new()
                    .post(format!("{upstream}{uri}"))
                    .bearer_auth(token)
                    .header("X-AQ-Graph-API-Version", "1")
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                let code = res.status();
                let bytes = res.bytes().await.unwrap();
                if wait.swap(false, Ordering::SeqCst) { arrived.notify_one(); release.notified().await; }
                if drop_response.swap(false, Ordering::SeqCst) {
                    assert!(
                        code.is_success(),
                        "{code} {}",
                        String::from_utf8_lossy(&bytes)
                    );
                    return axum::response::Response::builder()
                        .status(503)
                        .body(Body::empty())
                        .unwrap();
                }
                axum::response::Response::builder()
                    .status(code)
                    .header("X-AQ-Graph-API-Version", "1")
                    .body(Body::from(bytes))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
    let config = AgentAccessConfig {
        graph_url: proxy_origin,
        graph_token_file: token_path,
        mutation_actor_id: Some("local-admin".into()),
        credential_ttl_ms: 120000,
    };
    f.state.agent_access = Some(Arc::new(Runtime::new(&config).await.unwrap()));
    assert!(human::task_create_available(&f.state));
    let app = human::routes(&f.state).with_state(f.state.clone());
    let project = f.selection.lineage.project_id;
    let path = format!("/api/human/projects/{project}/tasks");
    let op = Uuid::new_v4();
    let raw = request(op, "1");
    let (code, created) = send(&app, &path, Some(&raw), "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{created}");
    status_schema(&created);
    assert_eq!(created["state"], "confirmed");
    assert_eq!(created["actor_id"], json!(f.actor));
    assert_eq!(created["task_id"], json!(op));
    assert_eq!(created["receipt"]["workspace_id"], json!(workspace));
    assert_eq!(
        created["request_digest"],
        format!(
            "sha256:{}",
            hex::encode(sha2::Sha256::digest(raw.as_bytes()))
        )
    );
    assert!(!created.to_string().contains(token));
    let (_, again) = send(&app, &path, Some(&raw), "owner@localhost").await;
    assert_eq!(again, created);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let lookup = format!("/api/human/operations/{op}");
    assert_eq!(
        send(&app, &lookup, None, "owner@localhost").await.1,
        created
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        send(&app, &path, Some(&format!("{raw} ")), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let saved: Vec<u8> = sqlx::query_scalar(
        "SELECT request_bytes FROM human_graph_operations WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(saved, raw.as_bytes());
    for sql in [
        "UPDATE human_graph_operations SET request_bytes='changed' WHERE operation_id=$1",
        "UPDATE human_graph_operations SET dispatch_count=0 WHERE operation_id=$1",
        "DELETE FROM human_graph_operations WHERE operation_id=$1",
    ] {
        assert!(sqlx::query(sql).bind(op).execute(&pool).await.is_err());
    }
    assert!(
        sqlx::query("TRUNCATE human_graph_operations")
            .execute(&pool)
            .await
            .is_err()
    );
    // Stale revision has durable proof of no dispatch, not a generic ambiguous409.
    let stale = Uuid::new_v4();
    let (code, refused) = send(&app, &path, Some(&request(stale, "2")), "owner@localhost").await;
    assert_eq!(code, StatusCode::CONFLICT, "{refused}");
    status_schema(&refused);
    assert_eq!(refused["effect"], "not_dispatched");
    assert_eq!(
        refused["recovery"]["reason"],
        "project_binding_changed_before_dispatch"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        send(
            &app,
            &format!("/api/human/operations/{stale}"),
            None,
            "owner@localhost"
        )
        .await
        .1,
        refused
    );
    // Real Graph committed, but AQ lost the response. Lookup does not retry.
    let lost = Uuid::new_v4();
    lose.store(true, Ordering::SeqCst);
    let (code, unknown) = send(&app, &path, Some(&request(lost, "1")), "owner@localhost").await;
    assert_eq!(code, StatusCode::ACCEPTED, "{unknown}");
    status_schema(&unknown);
    assert_eq!(unknown["state"], "unknown");
    assert_eq!(unknown["effect"], "possible");
    let count = calls.load(Ordering::SeqCst);
    let lost_path = format!("/api/human/operations/{lost}");
    assert_eq!(
        send(&app, &lost_path, None, "owner@localhost").await.1,
        unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), count);
    let actual: Value = client
        .get(format!("{origin}/v1/workspaces/{workspace}/nodes/{lost}"))
        .bearer_auth(token)
        .header("X-AQ-Graph-API-Version", "1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(actual["id"], json!(lost));
    assert_eq!(actual["version"], 1);
    assert_eq!(actual["title"], "Human authored task");
    assert_eq!(actual["body"], "Durable source body");
    assert_eq!(
        actual["acceptance_criteria"],
        "Exactly one actual Graph task"
    );
    assert_eq!(actual["labels"], json!(["human"]));
    assert_eq!(actual["task_state"], "draft");
    assert_eq!(actual["created_by"], "local-admin");
    assert_eq!(actual["metadata"]["aq:human_actor_id"], json!(f.actor));
    assert_eq!(actual["metadata"]["aq:project_id"], json!(project));
    let (code, recovered) = send(
        &app,
        &format!("{lost_path}/recover"),
        Some("{}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{recovered}");
    status_schema(&recovered);
    assert_eq!(recovered["state"], "confirmed");
    assert_eq!(recovered["receipt"]["version"], "1");
    assert_eq!(calls.load(Ordering::SeqCst), count + 1);
    {
        let all = observed.lock().await;
        assert_eq!(
            all[1], all[2],
            "recovery must reuse exact mapped Graph bytes"
        );
    }
    // A second explicit recovery observes the lease and cannot double-dispatch.
    // Revoking Owner while a real response is held prevents returning a stale-authority receipt.
    let racing = Uuid::new_v4();
    hold.store(true, Ordering::SeqCst);
    let race_app = app.clone();
    let race_path = path.clone();
    let pending = tokio::spawn(async move {
        send(
            &race_app,
            &race_path,
            Some(&request(racing, "1")),
            "owner@localhost",
        )
        .await
    });
    arrived.notified().await;
    let before = calls.load(Ordering::SeqCst);
    let (code, leased) = send(
        &app,
        &format!("/api/human/operations/{racing}/recover"),
        Some("{}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::ACCEPTED, "{leased}");
    assert_eq!(leased["recovery"]["reason"], "dispatch_in_progress");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
            .bind(f.actor)
            .execute(&pool),
    )
    .await
    .unwrap()
    .unwrap();
    release.notify_one();
    let (code, withheld) = pending.await.unwrap();
    assert_eq!(code, StatusCode::CONFLICT, "{withheld}");
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    let (code, durable) = send(
        &app,
        &format!("/api/human/operations/{racing}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{durable}");
    assert_eq!(durable["state"], "confirmed");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    // Unknown remains possible under changed service or project; neither can retry.
    let blocked = Uuid::new_v4();
    lose.store(true, Ordering::SeqCst);
    assert_eq!(
        send(&app, &path, Some(&request(blocked, "1")), "owner@localhost")
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let before = calls.load(Ordering::SeqCst);
    let blocked_path = format!("/api/human/operations/{blocked}");
    let mut changed = config.clone();
    changed.mutation_actor_id = Some("different-service-actor".into());
    let mut other = f.state.clone();
    other.agent_access = Some(Arc::new(Runtime::new(&changed).await.unwrap()));
    let other_app = human::routes(&other).with_state(other);
    let (_, v) = send(
        &other_app,
        &format!("{blocked_path}/recover"),
        Some("{}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(v["state"], "unknown");
    assert_eq!(v["effect"], "possible");
    assert_eq!(v["recovery"]["reason"], "graph_service_changed");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    sqlx::query("UPDATE project_execution_bindings SET revision=2 WHERE project_id=$1")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    let (_, v) = send(
        &app,
        &format!("{blocked_path}/recover"),
        Some("{}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(v["state"], "unknown");
    assert_eq!(v["effect"], "possible");
    assert_eq!(v["recovery"]["reason"], "project_binding_changed");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    // A confirmed receipt is original-workspace proof even after rebinding.
    assert_eq!(
        send(&app, &lookup, None, "owner@localhost").await.1,
        created
    );
    let foreign = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(foreign).execute(&pool).await.unwrap();
    assert_eq!(
        send(&app, &lookup, None, "other@localhost").await.0,
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&app, &lookup, None, "owner@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            &path,
            Some(&request(Uuid::new_v4(), "2")),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(calls.load(Ordering::SeqCst), before);
    server.abort();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
