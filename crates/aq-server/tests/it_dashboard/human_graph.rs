//! Actual independent pinned Graph service + PostgreSQL, never browser mocks.
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
use sqlx::ConnectOptions;
use std::{os::unix::fs::PermissionsExt, process::Stdio, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
async fn read(app: &Router, path: &str, email: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
#[tokio::test]
async fn absent_graph_and_viewer_scope_do_not_fabricate_empty_success() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let app = human::routes(&f.state).with_state(f.state.clone());
    let path = format!(
        "/api/human/projects/{}/tasks",
        f.selection.lineage.project_id
    );
    assert_eq!(
        read(&app, &path, "owner@localhost").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        read(&app, "/api/human/overview/tasks", "owner@localhost")
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(viewer).execute(&f.db.pool().await).await.unwrap();
    assert_eq!(
        read(&app, &path, "viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&app, "/api/human/overview/tasks", "viewer@localhost")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&app, &format!("{path}?limit=65"), "owner@localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn actual_graph_tasks_dependencies_knowledge_revisions_and_cursor_scope() {
    use sha2::Digest as _;
    let binary = std::env::var("AQ_TEST_GRAPH_BINARY").unwrap();
    assert_eq!(
        hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
        std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
    );
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let graph_db = TestDb::create().await;
    let dir = tempfile::tempdir().unwrap();
    let token = "human-graph-fixture-private-service-token";
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
        tokio::time::sleep(std::time::Duration::from_millis(25)).await
    }
    assert!(ready);
    let root = Uuid::new_v4();
    let dependency = Uuid::new_v4();
    let document = Uuid::new_v4();
    let response=client.post(format!("{origin}/v1/workspaces/{workspace}/mutations")).bearer_auth(token).header("X-AQ-Graph-API-Version","1").json(&json!({"idempotency_key":"actual-human-read-fixture","operations":[{"op":"create_node","id":root,"kind":"task","title":"Implement actual gateway","body":"actual task body","task_state":"active","metadata":{"aq:private_canary":"must-not-escape"}},{"op":"create_node","id":dependency,"kind":"task","title":"Gateway prerequisite","task_state":"active"},{"op":"create_node","id":document,"kind":"knowledge","title":"Gateway design","body":"actual retained knowledge","content_type":"reference","verification":"unverified"},{"op":"add_edge","source":dependency,"target":root,"edge_type":"blocks"}]})).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status} {body}");
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: origin.clone(),
            graph_token_file: token_path.clone(),
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = human::routes(&f.state).with_state(f.state.clone());
    let project = f.selection.lineage.project_id;
    let base = format!("/api/human/projects/{project}");
    let (status, page) = read(&app, &format!("{base}/tasks?limit=1"), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["contract"], "human-graph-1");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, &page),
        "public page schema: {page}"
    );

    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
    assert!(!page.to_string().contains("must-not-escape"));
    assert!(!page.to_string().contains(token));
    let cursor = page["data"]["next_cursor"].as_str().unwrap();
    let (_, second) = read(
        &app,
        &format!("{base}/tasks?limit=1&cursor={cursor}"),
        "owner@localhost",
    )
    .await;
    assert_ne!(
        page["data"]["items"][0]["id"],
        second["data"]["items"][0]["id"]
    );
    assert_eq!(
        read(
            &app,
            &format!("{base}/knowledge?limit=1&cursor={cursor}"),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, task) = read(&app, &format!("{base}/tasks/{root}"), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{task}");
    assert_eq!(task["data"]["version"], "2");
    assert_eq!(task["data"]["body"], "actual task body");
    let (status, graph) = read(
        &app,
        &format!("{base}/tasks/{root}/graph"),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    assert_eq!(graph["data"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(graph["data"]["edges"].as_array().unwrap().len(), 1);
    let (status, knowledge) = read(
        &app,
        &format!("{base}/knowledge/{document}?revision=1"),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{knowledge}");
    assert_eq!(knowledge["data"]["body"], "actual retained knowledge");
    let (status, search) = read(
        &app,
        &format!("{base}/knowledge?q=Gateway"),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{search}");
    assert_eq!(search["data"]["items"].as_array().unwrap().len(), 1);
    let overview_path = format!("/api/human/overview/tasks?project_id={project}");
    let (status, summary) = read(&app, &overview_path, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    assert_eq!(summary["contract"], "human-overview-tasks-1");
    let overview_schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-overview-tasks/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&overview_schema, &overview_schema, &summary),
        "overview schema: {summary}"
    );

    assert_eq!(summary["coverage"], "complete");
    assert_eq!(
        summary["projects"][0]["count_accuracy"],
        "complete_observation"
    );
    assert_eq!(summary["projects"][0]["task_counts"]["active"], 2);
    assert_eq!(summary["projects"][0]["task_counts"]["draft"], 0);
    assert_eq!(summary["projects"][0]["tasks"].as_array().unwrap().len(), 2);
    assert!(!summary.to_string().contains("must-not-escape"));
    assert!(summary["projects"][0]["tasks"][0].get("body").is_none());
    let (status, lower) = read(
        &app,
        &format!("{overview_path}&task_limit=1"),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{lower}");
    assert_eq!(lower["coverage"], "partial");
    assert_eq!(lower["projects"][0]["count_accuracy"], "lower_bound");
    assert_eq!(lower["projects"][0]["task_counts"]["active"], 1);
    assert_eq!(lower["projects"][0]["more_tasks"], true);
    assert_eq!(
        read(
            &app,
            "/api/human/overview/tasks?project_limit=9",
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let other_project = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $2,1,binding || jsonb_build_object('project_id',$2::text,'name','Unavailable fixture project','graph_adapter_reference','other.graph.adapter'),validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$1").bind(project).bind(other_project).execute(&f.db.pool().await).await.unwrap();
    let (status, mixed) = read(&app, "/api/human/overview/tasks", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{mixed}");
    assert_eq!(mixed["coverage"], "partial");
    assert!(
        aq_agent_api::check(&overview_schema, &overview_schema, &mixed),
        "partial overview schema: {mixed}"
    );
    let views = mixed["projects"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    let good = views
        .iter()
        .find(|v| v["project"]["id"] == json!(project))
        .unwrap();
    assert_eq!(good["task_counts"]["active"], 2);
    let missing = views
        .iter()
        .find(|v| v["project"]["id"] == json!(other_project))
        .unwrap();
    assert_eq!(missing["count_accuracy"], "unknown");
    assert!(missing["task_counts"]["active"].is_null());
    assert_eq!(missing["reason"], "unsupported_adapter");
    let (status, bounded) = read(
        &app,
        "/api/human/overview/tasks?project_limit=1",
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bounded}");
    assert_eq!(bounded["coverage"], "partial");
    assert_eq!(bounded["projects"].as_array().unwrap().len(), 1);
    assert_eq!(bounded["next_after"], json!(project));
    let (status, next) = read(
        &app,
        &format!("/api/human/overview/tasks?project_limit=1&after={project}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["projects"][0]["project"]["id"], json!(other_project));
    assert!(next["next_after"].is_null());
    // Current binding changes invalidate old cursors, rather than silently changing workspaces.
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(project)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        read(
            &app,
            &format!("{base}/tasks?limit=1&cursor={cursor}"),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Hold an actual Graph response until current human authorization changes.
    // The final fence must reject it instead of returning protected content.
    let arrived = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let observed = arrived.clone();
    let unblock = release.clone();
    let upstream = origin.clone();
    let proxy = axum::Router::new().route(
        "/{*path}",
        axum::routing::get(move |request: axum::extract::Request| {
            let observed = observed.clone();
            let unblock = unblock.clone();
            let upstream = upstream.clone();
            async move {
                let response = reqwest::Client::new()
                    .get(format!("{upstream}{}", request.uri()))
                    .bearer_auth("human-graph-fixture-private-service-token")
                    .header("X-AQ-Graph-API-Version", "1")
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let bytes = response.bytes().await.unwrap();
                observed.notify_one();
                unblock.notified().await;
                axum::response::Response::builder()
                    .status(status)
                    .header("X-AQ-Graph-API-Version", "1")
                    .body(Body::from(bytes))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_origin = format!("http://{}", listener.local_addr().unwrap());
    let proxy_server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: proxy_origin,
            graph_token_file: token_path,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let race_app = human::routes(&f.state).with_state(f.state.clone());
    let path = format!("{base}/tasks/{root}");
    let pending = tokio::spawn(async move { read(&race_app, &path, "owner@localhost").await });
    arrived.notified().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    release.notify_one();
    let (status, body) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    let overview_app = human::routes(&f.state).with_state(f.state.clone());
    let pending =
        tokio::spawn(async move { read(&overview_app, &overview_path, "owner@localhost").await });
    arrived.notified().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    release.notify_one();
    let (status, body) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    proxy_server.abort();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn read_gate_queues_a_page_burst_and_still_refuses_overload() {
    // QA ISSUE-001/WD-07: the 3rd concurrent read used to fail immediately.
    use aq_server::read_gate::ReadGate;
    let gate = ReadGate::new(2, 4);
    let first = gate.enter().await.unwrap();
    let second = gate.enter().await.unwrap();
    let release = async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop((first, second));
    };
    let (_, third, fourth, fifth, sixth, seventh) = tokio::join!(
        release,
        gate.enter(),
        gate.enter(),
        gate.enter(),
        gate.enter(),
        gate.enter()
    );
    assert!(
        third.is_some() && fourth.is_some(),
        "queued reads run when slots free"
    );
    assert!(
        fifth.is_none() && sixth.is_none(),
        "a wait beyond QUEUE_WAIT is refused"
    );
    assert!(
        seventh.is_none(),
        "beyond running + queued is refused at once"
    );
}
