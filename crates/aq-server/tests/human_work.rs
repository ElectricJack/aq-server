//! G11 Work-view contracts (X8-X13) on human-graph-1 over a fake Graph API.
mod common;
use aq_server::agent_access::{AgentAccessConfig, Runtime, human};
use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    response::Response,
    routing::get,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use uuid::Uuid;

#[derive(Clone, Default)]
struct Graph {
    nodes: Arc<Mutex<HashMap<Uuid, Value>>>,
    contexts: Arc<Mutex<HashMap<Uuid, Value>>>,
    trees: Arc<Mutex<HashMap<Uuid, Value>>>,
    cursors: Arc<Mutex<Vec<Option<String>>>>,
}
fn reply(status: u16, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
fn found(v: Option<&Value>) -> Response {
    match v {
        Some(v) => reply(200, v),
        None => reply(
            404,
            &json!({"error":{"code":"not_found","message":"missing"}}),
        ),
    }
}
async fn read_node(State(g): State<Graph>, Path((_, id)): Path<(Uuid, Uuid)>) -> Response {
    found(g.nodes.lock().unwrap().get(&id))
}
async fn read_context(State(g): State<Graph>, Path((_, id)): Path<(Uuid, Uuid)>) -> Response {
    found(g.contexts.lock().unwrap().get(&id))
}
async fn read_tree(State(g): State<Graph>, Path((_, id)): Path<(Uuid, Uuid)>) -> Response {
    found(g.trees.lock().unwrap().get(&id))
}
async fn list_nodes(State(g): State<Graph>) -> Response {
    let mut items: Vec<Value> = g.nodes.lock().unwrap().values().cloned().collect();
    items.sort_by_key(|n| n["id"].as_str().unwrap().to_owned());
    reply(200, &json!({"items":items,"next_cursor":null}))
}
async fn read_history(
    State(g): State<Graph>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let cursor = q.get("cursor").cloned();
    g.cursors.lock().unwrap().push(cursor.clone());
    let entry = |n: u64| {
        json!({"sequence":n,"entry_id":Uuid::new_v4(),"category":"lifecycle","actor_id":"fixture",
        "operation":"set_task_state","reason":null,"change_set_id":Uuid::new_v4(),
        "data":{"secret":"must-not-escape"},"created_at":"2026-01-01T00:00:00Z"})
    };
    match cursor {
        None => reply(
            200,
            &json!({"items":[entry(2),entry(1)],"next_cursor":"upstream-page-2"}),
        ),
        Some(_) => reply(200, &json!({"items":[],"next_cursor":null})),
    }
}

fn task(id: Uuid, w: Uuid, title: &str, state: &str) -> Value {
    json!({"id":id,"workspace_id":w,"kind":"task","version":1,"content_revision":1,
        "title":title,"body":"","acceptance_criteria":"","labels":[],"metadata":{},"task_state":state,
        "verification":"unverified","provenance":[],"retired":false,"deleted":false,
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","created_by":"fixture",
        "last_verified_by":null,"last_verified_at":null,"retirement_reason":null,"content_type":"note",
        "summary":null,"summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,
        "successor_id":null})
}
fn knowledge(id: Uuid, w: Uuid) -> Value {
    let mut k = task(id, w, "Note", "active");
    k["kind"] = json!("knowledge");
    k["task_state"] = Value::Null;
    k
}
fn edge(w: Uuid, source: Uuid, target: Uuid, edge_type: &str) -> Value {
    json!({"id":Uuid::new_v4(),"workspace_id":w,"source":source,"target":target,"edge_type":edge_type,
        "source_revision":null,"target_revision":null,"version":1,"deleted":false,
        "created_at":"2026-01-01T00:00:00Z","description":null})
}
fn context(root: &Value, nodes: Vec<Value>, edges: Vec<Value>, truncated: bool) -> Value {
    json!({"node":root,"nodes":nodes,"edges":edges,"pinned_revisions":[],"truncated":truncated})
}

struct Fixture {
    f: common::profiled::Fixture,
    graph: Graph,
    app: Router,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn setup() -> Fixture {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let graph = Graph::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/workspaces/{w}/nodes", get(list_nodes))
        .route("/v1/workspaces/{w}/nodes/{id}", get(read_node))
        .route("/v1/workspaces/{w}/nodes/{id}/context", get(read_context))
        .route("/v1/workspaces/{w}/nodes/{id}/tree", get(read_tree))
        .route("/v1/workspaces/{w}/nodes/{id}/history", get(read_history))
        .with_state(graph.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-graph-service-key-000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: url,
            graph_token_file: key,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    // One router: cursors are bound to the router's boot key.
    let app = human::routes(&f.state).with_state(f.state.clone());
    Fixture {
        f,
        graph,
        app,
        server,
        _dir: dir,
    }
}
impl Fixture {
    fn w(&self) -> Uuid {
        self.f.selection.lineage.workspace_id
    }
    /// The fixture's attempt holds this task.
    fn held(&self) -> Uuid {
        self.f.selection.lineage.task_id
    }
    async fn get(&self, path: &str) -> Value {
        let project = self.f.selection.lineage.project_id;
        let res = self
            .app
            .clone()
            .oneshot(
                Request::get(format!("/api/human/projects/{project}{path}"))
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let body: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        let schema: Value =
            serde_json::from_str(include_str!("../../../protocol/human-graph/v1/schema.json"))
                .unwrap();
        assert!(
            aq_agent_api::check(&schema, &schema, &body),
            "schema {path}: {body}"
        );
        assert!(!body.to_string().contains("must-not-escape"));
        body["data"].clone()
    }
}

#[tokio::test]
async fn graph_reports_parent_omitted_counts_and_expansion() {
    let x = setup().await;
    let w = x.w();
    let (root, parent) = (Uuid::new_v4(), Uuid::new_v4());
    let kids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    let note = Uuid::new_v4();
    let root_node = task(root, w, "Root", "active");
    let mut nodes = vec![task(parent, w, "Epic", "active")];
    nodes.extend(
        kids.iter()
            .zip(["completed", "completed", "active"])
            .map(|(k, s)| task(*k, w, "Child", s)),
    );
    nodes.push(knowledge(note, w));
    let mut edges = vec![
        edge(w, parent, root, "parent-child"),
        edge(w, root, note, "references"),
    ];
    edges.extend(kids.iter().map(|k| edge(w, root, *k, "parent-child")));
    x.graph
        .contexts
        .lock()
        .unwrap()
        .insert(root, context(&root_node, nodes, edges, false));

    let g = x.get(&format!("/tasks/{root}/graph?limit=1")).await;
    assert_eq!(g["parent"]["id"], json!(parent));
    assert_eq!(g["parent"]["title"], "Epic");
    assert_eq!(g["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(g["truncated"], true);
    assert_eq!(g["omitted"]["tasks"], "3");
    assert_eq!(g["omitted"]["tasks_by_state"]["completed"], "2");
    assert_eq!(g["omitted"]["tasks_by_state"]["active"], "1");
    assert_eq!(g["omitted"]["other_nodes"], "1");
    assert_eq!(g["omitted"]["edges"], "4");
    assert_eq!(g["omitted"]["exact"], true);
    assert_eq!(g["expansion"]["limit"], "4");

    let full = x.get(&format!("/tasks/{root}/graph")).await;
    assert_eq!(full["nodes"].as_array().unwrap().len(), 5);
    assert_eq!(full["omitted"]["tasks"], "0");
    assert_eq!(full["expansion"], Value::Null);
}

#[tokio::test]
async fn work_explains_readiness_blockers_and_attached_execution() {
    let x = setup().await;
    let w = x.w();
    let held = x.held();
    let (waiting, ready, unknown, blocker, done) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let open = task(blocker, w, "Schema first", "active");
    let closed = task(done, w, "Done first", "completed");
    {
        let mut c = x.graph.contexts.lock().unwrap();
        c.insert(
            held,
            context(
                &task(held, w, "Held", "active"),
                vec![open.clone()],
                vec![edge(w, blocker, held, "blocks")],
                false,
            ),
        );
        c.insert(
            waiting,
            context(
                &task(waiting, w, "Waiting", "active"),
                vec![open],
                vec![edge(w, blocker, waiting, "blocks")],
                false,
            ),
        );
        c.insert(
            ready,
            context(
                &task(ready, w, "Ready", "active"),
                vec![closed],
                vec![edge(w, done, ready, "blocks")],
                false,
            ),
        );
        c.insert(
            unknown,
            context(&task(unknown, w, "Unknown", "active"), vec![], vec![], true),
        );
    }

    let running = x.get(&format!("/tasks/{held}/work")).await;
    assert_eq!(running["readiness"]["state"], "running");
    assert_eq!(running["execution"]["task_id"], json!(held));
    assert!(running["execution"]["session_id"].is_string());

    let blocked = x.get(&format!("/tasks/{waiting}/work")).await;
    assert_eq!(blocked["readiness"]["state"], "waiting");
    assert_eq!(blocked["execution"], Value::Null);
    let b = &blocked["readiness"]["blockers"][0];
    assert_eq!(b["id"], json!(blocker));
    assert_eq!(b["title"], "Schema first");
    assert_eq!(b["satisfied"], false);

    let free = x.get(&format!("/tasks/{ready}/work")).await;
    assert_eq!(free["readiness"]["state"], "ready");
    assert_eq!(free["readiness"]["blockers"][0]["satisfied"], true);

    let unsure = x.get(&format!("/tasks/{unknown}/work")).await;
    assert_eq!(unsure["readiness"]["state"], "unknown");
    assert_eq!(unsure["readiness"]["complete"], false);
}

#[tokio::test]
async fn task_pages_carry_attached_executions() {
    let x = setup().await;
    let w = x.w();
    let held = x.held();
    let other = Uuid::new_v4();
    x.graph.nodes.lock().unwrap().extend([
        (held, task(held, w, "Held", "active")),
        (other, task(other, w, "Idle", "active")),
    ]);
    let page = x.get("/tasks?limit=10").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let executions = page["executions"].as_array().unwrap();
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0]["task_id"], json!(held));
}

#[tokio::test]
async fn history_pages_graph_entries_and_attempts_with_bound_cursor() {
    let x = setup().await;
    let w = x.w();
    let held = x.held();
    x.graph
        .nodes
        .lock()
        .unwrap()
        .insert(held, task(held, w, "Held", "active"));
    let h = x.get(&format!("/tasks/{held}/history")).await;
    assert_eq!(h["items"].as_array().unwrap().len(), 2);
    assert_eq!(h["items"][0]["sequence"], "2");
    assert_eq!(h["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(h["attempts"][0]["holds_task"], true);
    let cursor = h["next_cursor"].as_str().unwrap();
    assert_ne!(cursor, "upstream-page-2");
    let next = x
        .get(&format!("/tasks/{held}/history?cursor={cursor}"))
        .await;
    assert_eq!(next["next_cursor"], Value::Null);
    assert_eq!(
        x.graph.cursors.lock().unwrap().clone(),
        vec![None, Some("upstream-page-2".to_owned())]
    );
}

#[tokio::test]
async fn summary_counts_descendant_completion_and_marks_truncation() {
    let x = setup().await;
    let w = x.w();
    let (root, a, b, c) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let leaf = |id, s| json!({"node":task(id, w, "Leaf", s),"children":[],"truncated":false});
    let tree = json!({"node":task(root, w, "Epic", "active"),"truncated":false,"children":[
        leaf(a, "completed"),
        {"node":task(b, w, "Mid", "active"),"truncated":true,"children":[leaf(c, "completed")]},
    ]});
    x.graph.trees.lock().unwrap().insert(root, tree);
    let s = x.get(&format!("/tasks/{root}/summary")).await;
    assert_eq!(s["implementation"]["total"], "3");
    assert_eq!(s["implementation"]["by_state"]["completed"], "2");
    assert_eq!(s["implementation"]["by_state"]["active"], "1");
    assert_eq!(s["implementation"]["complete"], false);
    assert_eq!(s["delivery"]["state"], "unavailable");
}
