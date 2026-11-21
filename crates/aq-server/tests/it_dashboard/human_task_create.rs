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
    let path = format!(
        "/api/human/projects/{}/tasks",
        f.selection.lineage.project_id
    );
    assert_eq!(
        send(&mounted, &path, Some("{}"), "owner@localhost").await.0,
        StatusCode::BAD_REQUEST,
        "advertised command route must actually exist"
    );
    assert_eq!(
        send(&mounted, &path, Some("{}"), "viewer@localhost")
            .await
            .0,
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
            &mounted,
            &format!("/api/human/operations/{op}"),
            None,
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM human_graph_operations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "capability inspection and refused requests have no Graph intent"
    );
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
    let app = aq_server::build_admin_router(f.state.clone());
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
    let action = |id: Uuid, revision: &str| {
        let payload: Value = serde_json::from_str(&request(id, revision)).unwrap();
        json!({"operation_id":id,"kind":"task.create","subject":{"type":"project","id":project,"revision":revision},"expected_versions":{"project":revision},"payload":payload}).to_string()
    };
    let (code, union) = send(
        &app,
        "/api/v1/actions",
        Some(&action(op, "1")),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, 200, "{union}");
    assert_eq!(union["data"]["disposition"], "confirmed");
    assert_eq!(union["data"]["affected_entities"][0]["id"], json!(op));
    assert_eq!(
        send(
            &app,
            &format!("/api/v1/operations/{op}"),
            None,
            "owner@localhost"
        )
        .await
        .1["data"],
        union["data"]
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
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
    assert!(
        aq_human_work::task_git_author::lookup(&pool, project, workspace, lost)
            .await
            .unwrap()
            .is_none()
    );
    let count = calls.load(Ordering::SeqCst);
    let lost_path = format!("/api/human/operations/{lost}");
    assert_eq!(
        send(&app, &lost_path, None, "owner@localhost").await.1,
        unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), count);
    let (code, unresolved) = send(
        &app,
        &format!("/api/v1/operations/{lost}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(code, 200, "{unresolved}");
    assert_eq!(unresolved["data"]["disposition"], "unknown");
    let bootstrap = send(&app, "/api/v1/bootstrap", None, "owner@localhost")
        .await
        .1;
    assert!(
        bootstrap["pending_operations"]
            .as_array()
            .unwrap()
            .contains(&json!(lost))
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
    let author = aq_human_work::task_git_author::lookup(&pool, project, workspace, lost)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(author.name, "owner");
    assert_eq!(author.email, "owner@localhost");
    assert_eq!(
        send(&app, &lost_path, None, "owner@localhost").await.1,
        recovered
    );
    assert_eq!(
        aq_human_work::task_git_author::lookup(&pool, project, workspace, lost)
            .await
            .unwrap(),
        Some(author)
    );
    assert_eq!(calls.load(Ordering::SeqCst), count + 1);
    {
        let all = observed.lock().await;
        assert_eq!(
            all[1], all[2],
            "recovery must reuse exact mapped Graph bytes"
        );
    }
    let resolved = send(
        &app,
        &format!("/api/v1/operations/{lost}"),
        None,
        "owner@localhost",
    )
    .await
    .1;
    assert_eq!(resolved["data"]["disposition"], "confirmed");
    assert_eq!(resolved["data"]["operation_id"], json!(lost));
    assert!(
        !send(&app, "/api/v1/bootstrap", None, "owner@localhost")
            .await
            .1["pending_operations"]
            .as_array()
            .unwrap()
            .contains(&json!(lost))
    );
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
    let (code, reconciled) = send(
        &app,
        &format!("/api/v1/operations/{racing}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{reconciled}");
    assert_eq!(
        reconciled["data"]["disposition"], "confirmed",
        "a committed native task must remain reconcilable"
    );
    // The union route must withhold admission metadata after owner revocation too.
    let union_race = Uuid::new_v4();
    hold.store(true, Ordering::SeqCst);
    let race_app = app.clone();
    let union_body = action(union_race, "1");
    let pending = tokio::spawn(async move {
        send(
            &race_app,
            "/api/v1/actions",
            Some(&union_body),
            "owner@localhost",
        )
        .await
    });
    arrived.notified().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    release.notify_one();
    let (code, withheld) = pending.await.unwrap();
    assert_eq!(code, StatusCode::FORBIDDEN, "{withheld}");
    assert!(withheld.get("data").is_none(), "{withheld}");
    assert!(withheld.get("pending_operations").is_none(), "{withheld}");
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    let (code, reconciled) = send(
        &app,
        &format!("/api/v1/operations/{union_race}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{reconciled}");
    assert_eq!(reconciled["data"]["disposition"], "confirmed");

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
    assert_eq!(
        send(
            &app,
            &format!("/api/v1/operations/{lost}"),
            None,
            "owner@localhost"
        )
        .await
        .0,
        404
    );
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE project_execution_bindings SET revision=1 WHERE project_id=$1")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    let union_id = Uuid::new_v4();
    let (code, first) = send(
        &app,
        "/api/v1/actions",
        Some(&action(union_id, "1")),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, 200, "{first}");
    assert_eq!(first["data"]["disposition"], "confirmed");
    let (code, native) = send(
        &app,
        &path,
        Some(&request(union_id, "1")),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, 200, "{native}");
    assert_eq!(native["task_id"], json!(union_id));
    assert_eq!(calls.load(Ordering::SeqCst), before + 1);

    // Model a retained operation admitted before the union journal existed.
    // Only the isolated test database's union identity trigger is disabled.
    sqlx::query("ALTER TABLE dashboard_operations DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM dashboard_operations WHERE operation_id=$1")
        .bind(blocked)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE dashboard_operations ENABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    let count = calls.load(Ordering::SeqCst);
    let bootstrap = send(&app, "/api/v1/bootstrap", None, "owner@localhost")
        .await
        .1;
    assert!(
        bootstrap["pending_operations"]
            .as_array()
            .unwrap()
            .contains(&json!(blocked)),
        "{bootstrap}"
    );
    assert_eq!(
        send(
            &app,
            &format!("/api/v1/operations/{blocked}"),
            None,
            "other@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (code, historical) = send(
        &app,
        &format!("/api/v1/operations/{blocked}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{historical}");
    assert_eq!(historical["data"]["disposition"], "unknown");
    assert_eq!(historical["data"]["operation_id"], json!(blocked));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        count,
        "historical discovery must never redispatch"
    );
    // A selected-worker launch is a normal retained Graph task, pinned before readiness.
    let cfg = Arc::make_mut(&mut f.state.config);
    cfg.profiled_execution = Some(Arc::new(
        aq_server::profiled_execution::config::ProfiledConfig {
            revision: 1,
            qualified_runtimes: vec![],
            definition_bodies: vec![],
            projects: vec![aq_server::profiled_execution::config::ProjectPolicy {
                project_id: project,
                enabled: true,
                priority: 0,
                source: aq_admission::SourceSelection::NamedRef {
                    reference: aq_admission::Label::new("refs/heads/main").unwrap(),
                },
                retention_handle: Uuid::new_v4(),
                placement: None,
                agent_grants: None,
            }],
        },
    ));
    sqlx::query("UPDATE nodes SET last_seen_at=$1 WHERE id=$2")
        .bind((f.state.clock)())
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let launch_app = aq_server::build_admin_router(f.state.clone());
    let launch_path = format!(
        "/api/human/projects/{project}/workers/{}/launch",
        f.node.node_id
    );
    let launch_id = Uuid::new_v4();
    let launch_body = request(launch_id, "1").replace("\"draft\"", "\"active\"");
    let (code, launched) = send(
        &launch_app,
        &launch_path,
        Some(&launch_body),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, 200, "{launched}");
    let payload: Vec<u8> = sqlx::query_scalar(
        "SELECT payload_bytes FROM human_graph_operations WHERE operation_id=$1",
    )
    .bind(launch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload: Value = serde_json::from_slice(&payload).unwrap();
    let metadata = &payload["operations"][0]["metadata"];
    assert_eq!(metadata["aq:requested_node_id"], json!(f.node.node_id));
    assert!(
        aq_machine_control::launch::eligible(&pool, launch_id, f.node.node_id, metadata)
            .await
            .unwrap()
    );
    assert!(
        !aq_machine_control::launch::eligible(&pool, launch_id, Uuid::new_v4(), metadata)
            .await
            .unwrap()
    );
    let calls_before = calls.load(Ordering::SeqCst);
    assert_eq!(
        send(
            &launch_app,
            &launch_path,
            Some(&launch_body),
            "owner@localhost"
        )
        .await
        .1,
        launched
    );
    let other_path = format!(
        "/api/human/projects/{project}/workers/{}/launch",
        Uuid::new_v4()
    );
    assert_eq!(
        send(
            &launch_app,
            &other_path,
            Some(&launch_body),
            "owner@localhost"
        )
        .await
        .0,
        409
    );
    assert_eq!(calls.load(Ordering::SeqCst), calls_before);
    assert_eq!(
        send(
            &launch_app,
            &launch_path,
            Some(&request(Uuid::new_v4(), "1")),
            "owner@localhost"
        )
        .await
        .0,
        400
    );
    assert_eq!(
        send(
            &launch_app,
            &other_path,
            Some(&request(Uuid::new_v4(), "1").replace("\"draft\"", "\"active\"")),
            "owner@localhost"
        )
        .await
        .0,
        409
    );
    let picker = format!("/api/human/workers/{}/launch-projects", f.node.node_id);
    let list = send(&launch_app, &picker, None, "owner@localhost").await;
    assert_eq!(list.1["projects"].as_array().unwrap().len(), 1);
    check_epic_retirement_registration(&f, &origin, config.graph_token_file.clone(), token).await;
    let stop = json!({"operation_id":Uuid::new_v4(),"expected_revision":1}).to_string();
    let stopped = send(
        &launch_app,
        &format!("/api/projects/{project}/disable"),
        Some(&stop),
        "owner@localhost",
    )
    .await;
    assert_eq!(stopped.0, 200, "{}", stopped.1);
    assert_eq!(
        send(&launch_app, &picker, None, "owner@localhost").await.1["projects"],
        json!([])
    );
    let new_launch = request(Uuid::new_v4(), "2").replace("\"draft\"", "\"active\"");
    assert_eq!(
        send(
            &launch_app,
            &launch_path,
            Some(&new_launch),
            "owner@localhost"
        )
        .await
        .0,
        409
    );
    assert_eq!(
        send(
            &launch_app,
            &launch_path,
            Some(&launch_body),
            "owner@localhost"
        )
        .await
        .1,
        launched,
        "Original launch receipts remain replayable after Stop"
    );
    server.abort();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

async fn check_epic_retirement_registration(
    f: &common::profiled::Fixture,
    origin: &str,
    token_file: std::path::PathBuf,
    token: &str,
) {
    let mut state = f.state.clone();
    state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: origin.into(),
            graph_token_file: token_file,
            mutation_actor_id: Some("local-admin".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(state.clone());
    let project = f.selection.lineage.project_id;
    let workspace = f.selection.lineage.workspace_id;
    let [epic, approval, cleanup] = std::array::from_fn(|_| Uuid::new_v4());
    let tasks = format!("/api/human/projects/{project}/tasks");
    for id in [epic, approval, cleanup] {
        let (_, result) = send(&app, &tasks, Some(&request(id, "1")), "owner@localhost").await;
        assert_eq!(result["state"], "confirmed", "{result}");
    }
    let blocker = Uuid::new_v4();
    let dependency = json!({"operation_id":blocker,"params":{"project_id":project,"task_id":cleanup,
        "expected_version":"1","blocker_id":approval}});
    let (_, linked) = send(
        &app,
        "/api/v1/commands/task.dependency.add",
        Some(&dependency.to_string()),
        "owner@localhost",
    )
    .await;
    assert_eq!(linked["data"]["disposition"], "confirmed", "{linked}");

    let op = Uuid::new_v4();
    let request = json!({"operation_id":op,"params":{"project_id":project,"epic_id":epic,
        "expected_version":"1","generation":1,"repositories":repositories(),
        "ordinary_default":false,"permanent_behavior":"Enable A in ordinary builds",
        "adopt":{"approval":approval,"cleanup":cleanup}}});
    let route = "/api/v1/commands/epic.retirement.register";
    let denied = send(&app, route, Some(&request.to_string()), "viewer@localhost").await;
    assert!(!denied.0.is_success(), "{denied:?}");
    let mut invalid = request.clone();
    invalid["params"]["choice"] = json!("approve");
    let refused = send(&app, route, Some(&invalid.to_string()), "owner@localhost").await;
    assert!(!refused.0.is_success(), "{refused:?}");

    let (_, registered) = send(&app, route, Some(&request.to_string()), "owner@localhost").await;
    assert_eq!(
        registered["data"]["disposition"], "confirmed",
        "{registered}"
    );
    let query = format!("/api/v1/operations/{op}");
    let (_, replay) = send(
        &aq_server::build_admin_router(state),
        &query,
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(
        registered["data"]["receipt_refs"],
        replay["data"]["receipt_refs"]
    );

    let principal: Uuid =
        sqlx::query_scalar("SELECT owner_id FROM dashboard_operations WHERE operation_id=$1")
            .bind(op)
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(principal, f.actor);
    let client = reqwest::Client::new();
    let registration = Uuid::new_v5(
        &workspace,
        format!("aq/epic-retirement/1/{project}/{epic}/1").as_bytes(),
    );
    for (id, kind) in [
        (registration, "knowledge"),
        (approval, "task"),
        (cleanup, "task"),
        (Uuid::new_v5(&registration, b"closure-gate"), "task"),
    ] {
        let graph: Value = client
            .get(format!("{origin}/v1/workspaces/{workspace}/nodes/{id}"))
            .bearer_auth(token)
            .header("X-AQ-Graph-API-Version", "1")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(graph["kind"], kind, "{graph}");
        if id == approval {
            assert_eq!(graph["task_state"], "draft");
            assert_eq!(graph["version"], 2);
            assert_eq!(graph["verification"], "unverified");
            assert!(
                graph["metadata"]
                    .get("aq:epic_feature_retirement")
                    .is_none()
            );
            continue;
        }
        assert_eq!(graph["verification"], "verified", "{graph}");
        let retirement = &graph["metadata"]["aq:epic_feature_retirement"];
        if kind == "task" {
            assert_eq!(graph["task_state"], "draft");
            assert_eq!(retirement["registration"], json!(registration));
        } else {
            assert_eq!(graph["created_by"], "local-admin");
            assert_eq!(
                retirement["spec"]["owner_authority"]["principal_id"],
                json!(principal)
            );
            assert_eq!(retirement["spec"]["approval"], json!(approval));
            assert_eq!(retirement["spec"]["cleanup"], json!(cleanup));
            assert!(retirement.get("approved").is_none());
        }
    }
    let ids = Retirement {
        project,
        epic,
        approval,
        cleanup,
        registration,
    };
    check_epic_retirement_settlement(&app, origin, workspace, token, &ids).await;
    let at = At {
        app: &app,
        origin,
        workspace,
        token,
    };
    check_epic_retirement_released_before_decision(&at, project).await;
}

const REPOSITORIES: [&str; 5] = [
    "Acme/service",
    "Acme/dashboard",
    "Acme/worker",
    "Acme/playbooks",
    "Acme/git",
];
fn repositories() -> Value {
    REPOSITORIES
        .iter()
        .map(|repo| (repo.to_string(), json!("feature_a")))
        .collect()
}
struct Retirement {
    project: Uuid,
    epic: Uuid,
    approval: Uuid,
    cleanup: Uuid,
    registration: Uuid,
}
async fn graph_node(origin: &str, workspace: Uuid, token: &str, id: Uuid, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!(
            "{origin}/v1/workspaces/{workspace}/nodes/{id}{path}"
        ))
        .bearer_auth(token)
        .header("X-AQ-Graph-API-Version", "1")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
/// One owner command, as the CLI posts it; returns its operation and result.
async fn owner_command(app: &Router, kind: &str, params: Value, email: &str) -> (Uuid, Value) {
    let op = Uuid::new_v4();
    let body = json!({"operation_id":op,"params":params}).to_string();
    let (_, result) = send(app, &format!("/api/v1/commands/{kind}"), Some(&body), email).await;
    (op, result)
}
fn artifact(seed: u8) -> Value {
    json!({"artifact_id":Uuid::from_bytes([seed; 16]),"sha256":format!("{seed:02x}").repeat(32)})
}
fn commit(seed: usize) -> String {
    format!("{:040x}", 0x1000 + seed)
}

/// Spec 0020 settlement of the AQ-owned registration on disposable Graph:
/// the dashboard deletes the verified closure gate, the owner restores it,
/// approves, records five merges and ordinary builds plus the combined check,
/// and closes the epic once the cleanup task has completed.
async fn check_epic_retirement_settlement(
    app: &Router,
    origin: &str,
    workspace: Uuid,
    token: &str,
    ids: &Retirement,
) {
    let node = |id: Uuid| graph_node(origin, workspace, token, id, "");
    let version = |node: &Value| node["version"].to_string();
    let closure = Uuid::new_v5(&ids.registration, b"closure-gate");
    let gate = node(closure).await;
    let guarded = gate["content_revision"].as_i64().unwrap();
    let (_, deleted) = owner_command(
        app,
        "task.delete",
        json!({"project_id":ids.project,"task_id":closure,"expected_version":version(&gate)}),
        "owner@localhost",
    )
    .await;
    assert_eq!(deleted["data"]["disposition"], "confirmed", "{deleted}");
    assert_eq!(node(closure).await["deleted"], true);

    let settle = |kind: &'static str, extra: Value, email: &'static str| async move {
        let current = node(ids.registration).await;
        let mut params = json!({"project_id":ids.project,"registration_id":ids.registration,
            "expected_version":version(&current)});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        owner_command(app, kind, params, email).await
    };
    let disposition = |result: &Value| result["data"]["disposition"].clone();
    // Only the owner, only the current registration version, only the
    // guarded revision.
    let restore = json!({"revision":guarded});
    let (_, denied) = settle(
        "epic.retirement.restore",
        restore.clone(),
        "viewer@localhost",
    )
    .await;
    assert_ne!(disposition(&denied), "confirmed", "{denied}");
    let stale = json!({"project_id":ids.project,"registration_id":ids.registration,
        "expected_version":"1","revision":guarded});
    let (_, stale) = owner_command(app, "epic.retirement.restore", stale, "owner@localhost").await;
    assert_eq!(disposition(&stale), "conflict", "{stale}");
    let (_, unverified) = settle(
        "epic.retirement.restore",
        json!({"revision":1}),
        "owner@localhost",
    )
    .await;
    assert_eq!(disposition(&unverified), "rejected", "{unverified}");
    let (_, early) = settle("epic.retirement.close", json!({}), "owner@localhost").await;
    assert_eq!(disposition(&early), "rejected", "{early}");

    let (op, restored) = settle("epic.retirement.restore", restore, "owner@localhost").await;
    assert_eq!(disposition(&restored), "confirmed", "{restored}");
    let (_, replay) = send(
        app,
        &format!("/api/v1/operations/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(
        replay["data"]["receipt_refs"],
        restored["data"]["receipt_refs"]
    );
    let gate = node(closure).await;
    assert_eq!(gate["deleted"], false);
    assert_eq!(gate["task_state"], "draft");
    assert_eq!(gate["verification"], "verified", "{gate}");
    assert_eq!(gate["last_verified_by"], "local-admin");
    assert_eq!(
        gate["metadata"]["aq:epic_feature_retirement"],
        json!({"registration":ids.registration,"role":"closure_gate"})
    );
    let context = graph_node(origin, workspace, token, closure, "/context").await;
    assert!(
        context["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["edge_type"] == "parent-child"
                && e["source"] == json!(ids.epic)
                && e["target"] == json!(closure))
    );

    let vetted: Value = (REPOSITORIES.iter().enumerate())
        .map(|(i, repo)| (repo.to_string(), json!(commit(i))))
        .collect();
    let decision =
        json!({"revision":1,"choice":"approve","vetted":vetted,"evidence":artifact(0xd0)});
    let (_, decided) = settle("epic.retirement.decide", decision, "owner@localhost").await;
    assert_eq!(disposition(&decided), "confirmed", "{decided}");
    assert_eq!(node(ids.approval).await["task_state"], "completed");
    assert_eq!(node(ids.cleanup).await["task_state"], "active");

    let mut merges = serde_json::Map::new();
    let mut builds = serde_json::Map::new();
    for (i, repo) in REPOSITORIES.iter().enumerate() {
        let sha = commit(100 + i);
        let merge = json!({"kind":"merge","approval_revision":1,"evidence":artifact(0x10),
            "repository":repo,"pull_request":format!("https://github.com/{repo}/pull/1"),"sha":sha});
        let (_, merged) = settle("epic.retirement.evidence", merge, "owner@localhost").await;
        assert_eq!(disposition(&merged), "confirmed", "{merged}");
        let build = artifact(0x20 + i as u8);
        let ordinary = json!({"kind":"ordinary","approval_revision":1,"evidence":build,
            "repository":repo,"sha":sha,"passed":true,"flag_absent":true});
        let (_, built) = settle("epic.retirement.evidence", ordinary, "owner@localhost").await;
        assert_eq!(disposition(&built), "confirmed", "{built}");
        merges.insert(repo.to_string(), json!(sha));
        builds.insert(repo.to_string(), build);
    }
    let mut partial = builds.clone();
    partial.remove(REPOSITORIES[4]);
    let integrated = |builds: serde_json::Map<String, Value>| {
        json!({"kind":"integrated","approval_revision":1,"evidence":artifact(0xee),
            "revisions":merges.clone(),"builds":builds,"passed":true})
    };
    let (_, refused) = settle(
        "epic.retirement.evidence",
        integrated(partial),
        "owner@localhost",
    )
    .await;
    assert_eq!(disposition(&refused), "rejected", "{refused}");
    let (_, combined) = settle(
        "epic.retirement.evidence",
        integrated(builds),
        "owner@localhost",
    )
    .await;
    assert_eq!(disposition(&combined), "confirmed", "{combined}");
    let (_, pending) = settle("epic.retirement.close", json!({}), "owner@localhost").await;
    assert_eq!(disposition(&pending), "rejected", "{pending}");

    // Stand-in for the cleanup's normal delivery settlement.
    let cleanup = node(ids.cleanup).await;
    let settled = reqwest::Client::new()
        .post(format!("{origin}/v1/workspaces/{workspace}/mutations"))
        .bearer_auth(token)
        .header("X-AQ-Graph-API-Version", "1")
        .json(&json!({"idempotency_key":format!("settle-{}", ids.cleanup),"reason":"Cleanup settled",
            "operations":[{"op":"update_node","id":ids.cleanup,"expected_version":cleanup["version"],
                "patch":{"task_state":"completed"}}]}))
        .send()
        .await
        .unwrap();
    assert!(settled.status().is_success());
    let (op, closed) = settle("epic.retirement.close", json!({}), "owner@localhost").await;
    assert_eq!(disposition(&closed), "confirmed", "{closed}");
    let (_, replay) = send(
        app,
        &format!("/api/v1/operations/{op}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(
        replay["data"]["receipt_refs"],
        closed["data"]["receipt_refs"]
    );
    assert_eq!(node(ids.epic).await["task_state"], "completed");
    let gate = node(closure).await;
    assert_eq!(gate["task_state"], "completed");
    assert_eq!(gate["verification"], "verified");
    let state = &node(ids.registration).await["metadata"]["aq:epic_feature_retirement"];
    assert_eq!(
        state["closed"],
        json!(Uuid::new_v5(&ids.registration, b"close"))
    );
}

/// The Graph workspace and admin router one settlement check runs against.
struct At<'a> {
    app: &'a Router,
    origin: &'a str,
    workspace: Uuid,
    token: &'a str,
}
impl At<'_> {
    async fn node(&self, id: Uuid) -> Value {
        graph_node(self.origin, self.workspace, self.token, id, "").await
    }
    /// One task edit outside the retirement workflow, as ordinary work and
    /// its settlement make it.
    async fn edit(&self, id: Uuid, patch: Value) {
        let node = self.node(id).await;
        let edited = reqwest::Client::new()
            .post(format!("{}/v1/workspaces/{}/mutations", self.origin, self.workspace))
            .bearer_auth(self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .json(&json!({"idempotency_key":format!("edit-{id}-{}", node["version"]),"reason":"Ordinary work",
                "operations":[{"op":"update_node","id":id,"expected_version":node["version"],"patch":patch}]}))
            .send()
            .await
            .unwrap();
        let status = edited.status();
        let text = edited.text().await.unwrap();
        assert!(status.is_success(), "{id}: {text}");
    }
    /// An owner action on the registration at its current version.
    async fn settle(
        &self,
        ids: &Retirement,
        kind: &str,
        extra: Value,
        email: &str,
    ) -> (Uuid, Value) {
        let current = self.node(ids.registration).await;
        let mut params = json!({"project_id":ids.project,"registration_id":ids.registration,
            "expected_version":current["version"].to_string()});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        owner_command(self.app, kind, params, email).await
    }
}

/// The registered Epic 0 shape on disposable Graph: an AQ owner registration
/// whose gate was deleted and restored, and whose approval task completed as
/// ordinary work and released the cleanup before any decision was recorded.
/// Only an explicit, vetted owner decision settles it, without touching the
/// original tasks; then the ordinary five-repository evidence closes the epic.
async fn check_epic_retirement_released_before_decision(at: &At<'_>, project: Uuid) {
    let owner = "owner@localhost";
    let disposition = |result: &Value| result["data"]["disposition"].clone();
    let [epic, approval, cleanup] = std::array::from_fn(|_| Uuid::new_v4());
    let tasks = format!("/api/human/projects/{project}/tasks");
    for id in [epic, approval, cleanup] {
        let (_, result) = send(at.app, &tasks, Some(&request(id, "1")), owner).await;
        assert_eq!(result["state"], "confirmed", "{result}");
    }
    let dependency = json!({"project_id":project,"task_id":cleanup,"expected_version":"1","blocker_id":approval});
    let (_, linked) = owner_command(at.app, "task.dependency.add", dependency, owner).await;
    assert_eq!(disposition(&linked), "confirmed", "{linked}");
    let register = json!({"project_id":project,"epic_id":epic,"expected_version":"1","generation":1,
        "repositories":repositories(),"ordinary_default":false,
        "permanent_behavior":"Enable B in ordinary builds","adopt":{"approval":approval,"cleanup":cleanup}});
    let (_, registered) = owner_command(at.app, "epic.retirement.register", register, owner).await;
    assert_eq!(disposition(&registered), "confirmed", "{registered}");
    let registration = Uuid::new_v5(
        &at.workspace,
        format!("aq/epic-retirement/1/{project}/{epic}/1").as_bytes(),
    );
    let ids = Retirement {
        project,
        epic,
        approval,
        cleanup,
        registration,
    };
    let closure = Uuid::new_v5(&registration, b"closure-gate");
    let gate = at.node(closure).await;
    let guarded = gate["content_revision"].as_i64().unwrap();
    let delete = json!({"project_id":project,"task_id":closure,"expected_version":gate["version"].to_string()});
    let (_, deleted) = owner_command(at.app, "task.delete", delete, owner).await;
    assert_eq!(disposition(&deleted), "confirmed", "{deleted}");
    let restore = json!({"revision":guarded});
    let (_, restored) = at
        .settle(&ids, "epic.retirement.restore", restore, owner)
        .await;
    assert_eq!(disposition(&restored), "confirmed", "{restored}");
    at.edit(approval, json!({"task_state":"active"})).await;
    at.edit(approval, json!({"task_state":"completed"})).await;
    at.edit(cleanup, json!({"task_state":"active"})).await;
    let before = [at.node(approval).await, at.node(cleanup).await];
    let state = &at.node(registration).await["metadata"]["aq:epic_feature_retirement"];
    assert!(state.get("decision").is_none(), "{state}");

    let vetted: Value = (REPOSITORIES.iter().enumerate())
        .map(|(i, repo)| (repo.to_string(), json!(commit(200 + i))))
        .collect();
    let decision = |revision: u32, choice: &str, vetted: &Value| json!({"revision":revision,"choice":choice,"vetted":vetted,"evidence":artifact(0xd1)});
    let early = json!({"kind":"merge","approval_revision":1,"evidence":artifact(0x10),
        "repository":REPOSITORIES[0],"pull_request":"https://github.com/Acme/service/pull/2","sha":commit(300)});
    let (_, early) = at
        .settle(&ids, "epic.retirement.evidence", early, owner)
        .await;
    assert_eq!(
        disposition(&early),
        "rejected",
        "task state is not approval: {early}"
    );
    let approve = decision(1, "approve", &vetted);
    let (_, denied) = at
        .settle(
            &ids,
            "epic.retirement.decide",
            approve.clone(),
            "viewer@localhost",
        )
        .await;
    assert_ne!(disposition(&denied), "confirmed", "{denied}");
    let changes = decision(1, "request_changes", &vetted);
    let (_, changes) = at
        .settle(&ids, "epic.retirement.decide", changes, owner)
        .await;
    assert_eq!(disposition(&changes), "rejected", "{changes}");
    let mut partial = vetted.clone();
    partial.as_object_mut().unwrap().remove(REPOSITORIES[4]);
    let partial = decision(1, "approve", &partial);
    let (_, partial) = at
        .settle(&ids, "epic.retirement.decide", partial, owner)
        .await;
    assert_eq!(disposition(&partial), "rejected", "{partial}");
    let mut stale = approve.clone();
    let fence = json!({"project_id":project,"registration_id":registration,"expected_version":"1"});
    stale
        .as_object_mut()
        .unwrap()
        .extend(fence.as_object().unwrap().clone());
    let (_, stale) = owner_command(at.app, "epic.retirement.decide", stale, owner).await;
    assert_eq!(disposition(&stale), "conflict", "{stale}");

    let (op, decided) = at
        .settle(&ids, "epic.retirement.decide", approve, owner)
        .await;
    assert_eq!(disposition(&decided), "confirmed", "{decided}");
    let (_, replay) = send(at.app, &format!("/api/v1/operations/{op}"), None, owner).await;
    assert_eq!(
        replay["data"]["receipt_refs"],
        decided["data"]["receipt_refs"]
    );
    for (id, was) in [approval, cleanup].into_iter().zip(&before) {
        let now = at.node(id).await;
        for key in ["version", "task_state", "metadata"] {
            assert_eq!(now[key], was[key], "{id} {key}");
        }
    }
    let anchor = Uuid::new_v5(&registration, b"decision/1");
    let state = at.node(registration).await["metadata"]["aq:epic_feature_retirement"].clone();
    assert_eq!(state["approved"], json!(anchor));
    assert_eq!(state["reconciled"], json!(anchor));
    assert_eq!(at.node(anchor).await["verification"], "verified");
    let again = decision(2, "approve", &vetted);
    let (_, again) = at
        .settle(&ids, "epic.retirement.decide", again, owner)
        .await;
    assert_eq!(disposition(&again), "rejected", "{again}");

    let mut merges = serde_json::Map::new();
    let mut builds = serde_json::Map::new();
    for (i, repo) in REPOSITORIES.iter().enumerate() {
        let sha = commit(300 + i);
        let merge = json!({"kind":"merge","approval_revision":1,"evidence":artifact(0x10),
            "repository":repo,"pull_request":format!("https://github.com/{repo}/pull/2"),"sha":sha});
        let (_, merged) = at
            .settle(&ids, "epic.retirement.evidence", merge, owner)
            .await;
        assert_eq!(disposition(&merged), "confirmed", "{merged}");
        let build = artifact(0x30 + i as u8);
        let ordinary = json!({"kind":"ordinary","approval_revision":1,"evidence":build,
            "repository":repo,"sha":sha,"passed":true,"flag_absent":true});
        let (_, built) = at
            .settle(&ids, "epic.retirement.evidence", ordinary, owner)
            .await;
        assert_eq!(disposition(&built), "confirmed", "{built}");
        merges.insert(repo.to_string(), json!(sha));
        builds.insert(repo.to_string(), build);
    }
    let combined = json!({"kind":"integrated","approval_revision":1,"evidence":artifact(0xef),
        "revisions":merges,"builds":builds,"passed":true});
    let (_, combined) = at
        .settle(&ids, "epic.retirement.evidence", combined, owner)
        .await;
    assert_eq!(disposition(&combined), "confirmed", "{combined}");
    // Stand-in for the cleanup's normal delivery settlement.
    at.edit(cleanup, json!({"task_state":"completed"})).await;
    let (_, closed) = at
        .settle(&ids, "epic.retirement.close", json!({}), owner)
        .await;
    assert_eq!(disposition(&closed), "confirmed", "{closed}");
    assert_eq!(at.node(epic).await["task_state"], "completed");
    assert_eq!(at.node(closure).await["task_state"], "completed");
}
