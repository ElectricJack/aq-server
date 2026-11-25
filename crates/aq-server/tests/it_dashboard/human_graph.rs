//! Actual independent pinned Graph service + PostgreSQL, never browser mocks.
use crate::common;
use aq_server::{
    agent_access::{AgentAccessConfig, Runtime, human},
    test_support::TestDb,
};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{ConnectOptions, PgPool};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "owner@localhost";
/// The service token of the pinned Graph fixture.
const GRAPH_TOKEN: &str = "human-graph-fixture-private-service-token";
/// The service token of the fake all-project task list.
const WORK_TOKEN: &str = "all-project-work-fixture-service-token";

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
/// The status of an owner read.
async fn status(app: &Router, path: &str) -> StatusCode {
    read(app, path, OWNER).await.0
}
/// The body of an owner read that must succeed.
async fn ok(app: &Router, path: &str) -> Value {
    let (status, body) = read(app, path, OWNER).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}
fn assert_schema(schema: &Value, body: &Value, what: &str) {
    assert!(aq_agent_api::check(schema, schema, body), "{what}: {body}");
}
fn find_project(views: &[Value], project: Uuid) -> &Value {
    views
        .iter()
        .find(|v| v["project"]["id"] == json!(project))
        .unwrap()
}

/// Writes a service token file only its owner can read.
fn write_token(dir: &tempfile::TempDir, token: &str) -> PathBuf {
    let path = dir.path().join("token");
    std::fs::write(&path, token).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}
async fn runtime(graph_url: String, graph_token_file: PathBuf) -> Option<Arc<Runtime>> {
    let config = AgentAccessConfig {
        graph_url,
        graph_token_file,
        mutation_actor_id: None,
        credential_ttl_ms: 120000,
    };
    Some(Arc::new(Runtime::new(&config).await.unwrap()))
}
/// Adds project `to` with `from`'s binding, overlaid with `fields`.
async fn copy_binding(pool: &PgPool, from: Uuid, to: Uuid, fields: Value) {
    let mut fields = fields;
    fields["project_id"] = json!(to.to_string());
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $2,1,binding || $3::jsonb,validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$1")
        .bind(from).bind(to).bind(fields).execute(pool).await.unwrap();
}
async fn bump_binding_revision(pool: &PgPool, project: Uuid) {
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(project)
        .execute(pool)
        .await
        .unwrap();
}
async fn set_role(pool: &PgPool, user: Uuid, role: &str) {
    sqlx::query("UPDATE users SET role=$2 WHERE id=$1")
        .bind(user)
        .bind(role)
        .execute(pool)
        .await
        .unwrap();
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
    assert_eq!(status(&app, &path).await, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        status(&app, "/api/human/overview/tasks").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(viewer).execute(&f.db.pool().await).await.unwrap();
    for viewed in [
        path.as_str(),
        "/api/human/overview/tasks",
        "/api/human/work/tasks",
    ] {
        assert_eq!(
            read(&app, viewed, "viewer@localhost").await.0,
            StatusCode::FORBIDDEN,
            "{viewed}"
        );
    }
    assert_eq!(
        status(&app, &format!("{path}?limit=65")).await,
        StatusCode::BAD_REQUEST
    );
}

/// A fake Graph task list over two workspaces, one item a page.
#[derive(Clone)]
struct TaskList {
    calls: Arc<AtomicUsize>,
    workspaces: [Uuid; 2],
    /// The item of the first page, then of the second.
    items: [Value; 2],
}
async fn task_list(
    State(list): State<TaskList>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> ([(&'static str, &'static str); 1], Json<Value>) {
    list.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], format!("Bearer {WORK_TOKEN}"));
    let requested: std::collections::BTreeSet<Uuid> =
        serde_json::from_value(body["workspace_ids"].clone()).unwrap();
    assert_eq!(requested, list.workspaces.into_iter().collect());
    assert_eq!(body["state"], "active");
    assert_eq!(body["q"], "release");
    assert_eq!(body["limit"], 1);
    let [first, second] = &list.items;
    let page = if body["before"].is_null() {
        json!({"items":[first],"unavailable_workspaces":[],"has_more":true})
    } else {
        assert_eq!(body["before"], json!([first["updated_at"], first["id"]]));
        json!({"items":[second],"unavailable_workspaces":[],"has_more":false})
    };
    ([("x-aq-graph-api-version", "1")], Json(page))
}

#[tokio::test]
async fn all_project_work_combines_bound_workspaces_and_rejects_changed_cursors() {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    let workspace = f.selection.lineage.workspace_id;
    let other_project = Uuid::new_v4();
    let other_workspace = Uuid::new_v4();
    let binding = |name: &str, space: Uuid, adapter: &str| {
        let space = space.to_string();
        json!({"name":name,"graph_workspace_id":space,"graph_adapter_reference":adapter})
    };
    copy_binding(
        &pool,
        project,
        other_project,
        binding("Other project", other_workspace, "aq.graph.configured"),
    )
    .await;
    copy_binding(
        &pool,
        project,
        Uuid::new_v4(),
        binding("Unsupported project", workspace, "other.graph.adapter"),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_token(&dir, WORK_TOKEN);
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let list = TaskList {
        calls: Arc::new(AtomicUsize::new(0)),
        workspaces: [workspace, other_workspace],
        items: [
            json!({"id":first,"workspace_id":workspace,"title":"Latest release task","task_state":"active","updated_at":"2026-11-01T12:00:00.123456Z"}),
            json!({"id":second,"workspace_id":other_workspace,"title":"Other release task","task_state":"active","updated_at":"2026-11-01T12:00:00.123455Z"}),
        ],
    };
    let calls = list.calls.clone();
    let graph = Router::new()
        .route("/v1/task-list", axum::routing::post(task_list))
        .with_state(list);
    let graph_url = format!("http://{}", socket.local_addr().unwrap());
    f.state.agent_access = runtime(graph_url, token_path).await;
    let graph = tokio::spawn(async move { axum::serve(socket, graph).await.unwrap() });
    let app = human::routes(&f.state).with_state(f.state.clone());
    let path = "/api/human/work/tasks?limit=1&q=release&state=active";
    let page = ok(&app, path).await;
    assert_eq!(page["contract"], "human-work-task-list-1");
    assert_eq!(page["items"][0]["id"], json!(first));
    assert_eq!(page["projects"].as_array().unwrap().len(), 1);
    assert_eq!(page["projects"][0]["id"], json!(project));
    assert_eq!(page["unavailable_projects"], 1);
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = format!("{path}&cursor={cursor}");
    let page = ok(&app, &next).await;
    assert_eq!(page["items"][0]["id"], json!(second));
    assert_eq!(page["projects"][0]["id"], json!(other_project));
    assert!(page["next_cursor"].is_null());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let changed_filter = next.replace("state=active", "state=completed");
    assert_eq!(status(&app, &changed_filter).await, StatusCode::CONFLICT);
    bump_binding_revision(&pool, other_project).await;
    assert_eq!(status(&app, &next).await, StatusCode::CONFLICT);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    graph.abort();
}

/// The ids the pinned Graph fixture is seeded with.
struct Seeded {
    root: Uuid,
    document: Uuid,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn actual_graph_tasks_dependencies_knowledge_revisions_and_cursor_scope() {
    let binary = pinned_graph_binary();
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let graph_db = TestDb::create().await;
    let dir = tempfile::tempdir().unwrap();
    let token_path = write_token(&dir, GRAPH_TOKEN);
    let workspace = f.selection.lineage.workspace_id;
    let (mut child, origin) = spawn_graph(&binary, &graph_db, workspace, &token_path).await;
    let seeded = seed_graph(&origin, workspace).await;
    f.state.agent_access = runtime(origin.clone(), token_path.clone()).await;
    let app = human::routes(&f.state).with_state(f.state.clone());
    let project = f.selection.lineage.project_id;
    let base = format!("/api/human/projects/{project}");
    let cursor = project_reads_page_and_bind_cursors(&app, &base, &seeded).await;
    let overview_path = format!("/api/human/overview/tasks?project_id={project}");
    overview_counts_tasks_and_bounds(&app, &overview_path).await;
    overview_marks_an_unsupported_project_and_pages(&app, &pool, project).await;
    // Current binding changes invalidate old cursors, rather than silently changing workspaces.
    bump_binding_revision(&pool, project).await;
    assert_eq!(
        status(&app, &format!("{base}/tasks?limit=1&cursor={cursor}")).await,
        StatusCode::CONFLICT
    );
    // Hold an actual Graph response until current human authorization changes.
    // The final fence must reject it instead of returning protected content.
    let hold = Hold::new(pool.clone(), f.actor);
    let (proxy_origin, proxy_server) = hold.proxy(origin).await;
    f.state.agent_access = runtime(proxy_origin, token_path).await;
    let race_app = human::routes(&f.state).with_state(f.state.clone());
    let task = format!("{base}/tasks/{}", seeded.root);
    hold.role_change_conflicts(race_app, task).await;
    set_role(&pool, f.actor, "owner").await;
    let overview_app = human::routes(&f.state).with_state(f.state.clone());
    hold.role_change_conflicts(overview_app, overview_path)
        .await;
    proxy_server.abort();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

/// The Graph binary named by the environment, after checking its pinned digest.
fn pinned_graph_binary() -> String {
    use sha2::Digest as _;
    let binary = std::env::var("AQ_TEST_GRAPH_BINARY").unwrap();
    assert_eq!(
        hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
        std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
    );
    binary
}

/// Starts the Graph service on a free port and waits (bounded) until it is ready.
async fn spawn_graph(
    binary: &str,
    graph_db: &TestDb,
    workspace: Uuid,
    token_path: &std::path::Path,
) -> (tokio::process::Child, String) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
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
    for _ in 0..200 {
        let health = client.get(format!("{origin}/health/ready")).send().await;
        if health.is_ok_and(|r| r.status().is_success()) {
            return (child, origin);
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(25)).await
    }
    panic!("the Graph service did not become ready");
}

/// A task with a private canary, its blocking prerequisite and a document.
async fn seed_graph(origin: &str, workspace: Uuid) -> Seeded {
    let root = Uuid::new_v4();
    let dependency = Uuid::new_v4();
    let document = Uuid::new_v4();
    let response=reqwest::Client::new().post(format!("{origin}/v1/workspaces/{workspace}/mutations")).bearer_auth(GRAPH_TOKEN).header("X-AQ-Graph-API-Version","1").json(&json!({"idempotency_key":"actual-human-read-fixture","operations":[{"op":"create_node","id":root,"kind":"task","title":"Implement actual gateway","body":"actual task body","task_state":"active","metadata":{"aq:private_canary":"must-not-escape"}},{"op":"create_node","id":dependency,"kind":"task","title":"Gateway prerequisite","task_state":"active"},{"op":"create_node","id":document,"kind":"knowledge","title":"Gateway design","body":"actual retained knowledge","content_type":"reference","verification":"unverified"},{"op":"add_edge","source":dependency,"target":root,"edge_type":"blocks"}]})).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status} {body}");
    Seeded { root, document }
}

/// Task pages, a task, its graph and knowledge read through; a task cursor
/// is refused on the knowledge list. Returns the first task page's cursor.
async fn project_reads_page_and_bind_cursors(app: &Router, base: &str, seeded: &Seeded) -> String {
    let page = ok(app, &format!("{base}/tasks?limit=1")).await;
    assert_eq!(page["contract"], "human-graph-1");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    assert_schema(&schema, &page, "public page schema");
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
    assert!(!page.to_string().contains("must-not-escape"));
    assert!(!page.to_string().contains(GRAPH_TOKEN));
    let cursor = page["data"]["next_cursor"].as_str().unwrap();
    let (_, second) = read(app, &format!("{base}/tasks?limit=1&cursor={cursor}"), OWNER).await;
    assert_ne!(
        page["data"]["items"][0]["id"],
        second["data"]["items"][0]["id"]
    );
    assert_eq!(
        status(app, &format!("{base}/knowledge?limit=1&cursor={cursor}")).await,
        StatusCode::CONFLICT
    );
    let Seeded { root, document } = seeded;
    let task = ok(app, &format!("{base}/tasks/{root}")).await;
    assert_eq!(task["data"]["version"], "2");
    assert_eq!(task["data"]["body"], "actual task body");
    let graph = ok(app, &format!("{base}/tasks/{root}/graph")).await;
    assert_eq!(graph["data"]["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(graph["data"]["edges"].as_array().unwrap().len(), 1);
    let knowledge = ok(app, &format!("{base}/knowledge/{document}?revision=1")).await;
    assert_eq!(knowledge["data"]["body"], "actual retained knowledge");
    let search = ok(app, &format!("{base}/knowledge?q=Gateway")).await;
    assert_eq!(search["data"]["items"].as_array().unwrap().len(), 1);
    cursor.to_owned()
}

fn overview_schema() -> Value {
    serde_json::from_str(include_str!(
        "../../../../protocol/human-overview-tasks/v1/schema.json"
    ))
    .unwrap()
}

/// The overview counts the project's tasks completely, and as a lower
/// bound once a task limit cuts the list.
async fn overview_counts_tasks_and_bounds(app: &Router, overview_path: &str) {
    let summary = ok(app, overview_path).await;
    assert_eq!(summary["contract"], "human-overview-tasks-1");
    assert_schema(&overview_schema(), &summary, "overview schema");
    assert_eq!(summary["coverage"], "complete");
    let view = &summary["projects"][0];
    assert_eq!(view["count_accuracy"], "complete_observation");
    assert_eq!(view["task_counts"]["active"], 2);
    assert_eq!(view["task_counts"]["draft"], 0);
    assert_eq!(view["tasks"].as_array().unwrap().len(), 2);
    assert!(!summary.to_string().contains("must-not-escape"));
    assert!(view["tasks"][0].get("body").is_none());
    let lower = ok(app, &format!("{overview_path}&task_limit=1")).await;
    assert_eq!(lower["coverage"], "partial");
    let view = &lower["projects"][0];
    assert_eq!(view["count_accuracy"], "lower_bound");
    assert_eq!(view["task_counts"]["active"], 1);
    assert_eq!(view["more_tasks"], true);
    assert_eq!(
        status(app, "/api/human/overview/tasks?project_limit=9").await,
        StatusCode::BAD_REQUEST
    );
}

/// A project on an unsupported adapter makes the overview partial, with an
/// unknown count, and the project pages go one project at a time.
async fn overview_marks_an_unsupported_project_and_pages(
    app: &Router,
    pool: &PgPool,
    project: Uuid,
) {
    let other_project = Uuid::new_v4();
    let unavailable = json!({"name":"Unavailable fixture project","graph_adapter_reference":"other.graph.adapter"});
    copy_binding(pool, project, other_project, unavailable).await;
    let mixed = ok(app, "/api/human/overview/tasks").await;
    assert_eq!(mixed["coverage"], "partial");
    assert_schema(&overview_schema(), &mixed, "partial overview schema");
    let views = mixed["projects"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(find_project(views, project)["task_counts"]["active"], 2);
    let missing = find_project(views, other_project);
    assert_eq!(missing["count_accuracy"], "unknown");
    assert!(missing["task_counts"]["active"].is_null());
    assert_eq!(missing["reason"], "unsupported_adapter");
    let bounded = ok(app, "/api/human/overview/tasks?project_limit=1").await;
    assert_eq!(bounded["coverage"], "partial");
    assert_eq!(bounded["projects"].as_array().unwrap().len(), 1);
    assert_eq!(bounded["next_after"], json!(project));
    let next = ok(
        app,
        &format!("/api/human/overview/tasks?project_limit=1&after={project}"),
    )
    .await;
    assert_eq!(next["projects"][0]["project"]["id"], json!(other_project));
    assert!(next["next_after"].is_null());
}

/// Holds each proxied Graph response until released, so that `user`'s
/// role can change while a read is in flight.
struct Hold {
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    pool: PgPool,
    user: Uuid,
}
impl Hold {
    fn new(pool: PgPool, user: Uuid) -> Self {
        Hold {
            arrived: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
            pool,
            user,
        }
    }
    /// Serves a proxy of `upstream` that reads each response in full, says
    /// it arrived, and holds it until released.
    async fn proxy(&self, upstream: String) -> (String, tokio::task::JoinHandle<()>) {
        let observed = self.arrived.clone();
        let unblock = self.release.clone();
        let proxy = Router::new().route(
            "/{*path}",
            axum::routing::get(move |request: axum::extract::Request| {
                let observed = observed.clone();
                let unblock = unblock.clone();
                let upstream = upstream.clone();
                async move {
                    let response = reqwest::Client::new()
                        .get(format!("{upstream}{}", request.uri()))
                        .bearer_auth(GRAPH_TOKEN)
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
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
        (origin, server)
    }
    /// Reads `path` as the owner, demotes the user to viewer while the
    /// Graph response is held, and expects the read to conflict.
    async fn role_change_conflicts(&self, app: Router, path: String) {
        let pending = tokio::spawn(async move { read(&app, &path, OWNER).await });
        self.arrived.notified().await;
        set_role(&self.pool, self.user, "viewer").await;
        self.release.notify_one();
        let (status, body) = pending.await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
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
