//! G11 Work-view contracts (X8-X13) on human-graph-1 over a fake Graph API.
use crate::common;
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
    /// A first history page served instead of the synthetic one.
    history: Arc<Mutex<Option<Value>>>,
    revisions: Arc<Mutex<HashMap<u64, Value>>>,
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
    if let Some(page) = g.history.lock().unwrap().clone() {
        return reply(200, &page);
    }
    let secret = |n: u64| history_entry(n, "lifecycle", "set_task_state", secret_data());
    match cursor {
        None => reply(
            200,
            &json!({"items":[secret(2),secret(1)],"next_cursor":"upstream-page-2"}),
        ),
        Some(_) => reply(200, &json!({"items":[],"next_cursor":null})),
    }
}

async fn read_revision(
    State(g): State<Graph>,
    Path((_, _, n)): Path<(Uuid, Uuid, u64)>,
) -> Response {
    found(g.revisions.lock().unwrap().get(&n))
}

/// Entry data the human read must never pass through.
fn secret_data() -> Value {
    json!({"secret":"must-not-escape"})
}
fn history_entry(n: u64, category: &str, operation: &str, data: Value) -> Value {
    json!({"sequence":n,"entry_id":Uuid::new_v4(),"category":category,"actor_id":"fixture",
        "operation":operation,"reason":null,"change_set_id":Uuid::new_v4(),
        "data":data,"created_at":"2026-01-01T00:00:00Z"})
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
/// A complete Context with no neighbours.
fn alone(root: &Value) -> Value {
    context(root, vec![], vec![], false)
}
/// A complete Context of `root`, blocked by the node `blocker` alone.
fn blocked_by(root: &Value, blocker: &Value) -> Value {
    let uuid = |v: &Value| v.as_str().unwrap().parse::<Uuid>().unwrap();
    let w = uuid(&root["workspace_id"]);
    let blocks = edge(w, uuid(&blocker["id"]), uuid(&root["id"]), "blocks");
    context(root, vec![blocker.clone()], vec![blocks], false)
}

/// A child task under a parent and a grandparent.
struct Chain {
    child: Uuid,
    parent: Uuid,
    grand: Uuid,
    nodes: Vec<Value>,
    edges: Vec<Value>,
}
impl Chain {
    /// The parent and grandparent take `states`, in that order.
    fn new(w: Uuid, states: [&str; 2]) -> Self {
        let (child, parent, grand) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let nodes = vec![
            task(parent, w, "Parent", states[0]),
            task(grand, w, "Grand", states[1]),
        ];
        let edges = vec![
            edge(w, parent, child, "parent-child"),
            edge(w, grand, parent, "parent-child"),
        ];
        Chain {
            child,
            parent,
            grand,
            nodes,
            edges,
        }
    }
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
fn fake_graph(graph: &Graph) -> Router {
    Router::new()
        .route("/v1/workspaces/{w}/nodes", get(list_nodes))
        .route("/v1/workspaces/{w}/nodes/{id}", get(read_node))
        .route("/v1/workspaces/{w}/nodes/{id}/context", get(read_context))
        .route("/v1/workspaces/{w}/nodes/{id}/tree", get(read_tree))
        .route("/v1/workspaces/{w}/nodes/{id}/history", get(read_history))
        .route(
            "/v1/workspaces/{w}/nodes/{id}/revisions/{n}",
            get(read_revision),
        )
        .with_state(graph.clone())
}
async fn graph_runtime(url: String, dir: &tempfile::TempDir) -> Runtime {
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-graph-service-key-000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    Runtime::new(&AgentAccessConfig {
        graph_url: url,
        graph_token_file: key,
        mutation_actor_id: None,
        credential_ttl_ms: 120000,
    })
    .await
    .unwrap()
}
async fn setup() -> Fixture {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let graph = Graph::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = fake_graph(&graph);
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    f.state.agent_access = Some(Arc::new(graph_runtime(url, &dir).await));
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
    fn put_context(&self, id: Uuid, context: Value) {
        self.graph.contexts.lock().unwrap().insert(id, context);
    }
    /// Serves each node, and a complete Context of it that holds the others.
    fn put_mutual(&self, nodes: &[(Uuid, Value)]) {
        self.graph.nodes.lock().unwrap().extend(nodes.to_vec());
        for (id, node) in nodes {
            let others = nodes.iter().filter(|(other, _)| other != id);
            let others: Vec<Value> = others.map(|(_, n)| n.clone()).collect();
            self.put_context(*id, context(node, others, vec![], false));
        }
    }
    /// Serves `chain`'s child, in `state`, with its parent and grandparent.
    fn put_chain(&self, chain: Chain, state: &str, truncated: bool) -> Uuid {
        let child = task(chain.child, self.w(), "Child", state);
        let c = context(&child, chain.nodes, chain.edges, truncated);
        self.put_context(chain.child, c);
        chain.child
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
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../protocol/human-graph/v1/schema.json"
        ))
        .unwrap();
        assert!(
            aq_agent_api::check(&schema, &schema, &body),
            "schema {path}: {body}"
        );
        assert!(!body.to_string().contains("must-not-escape"));
        body["data"].clone()
    }
    async fn work(&self, id: Uuid) -> Value {
        self.get(&format!("/tasks/{id}/work")).await
    }
    async fn pool(&self) -> sqlx::PgPool {
        self.f.db.pool().await
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
    x.put_context(root, context(&root_node, nodes, edges, false));

    let g = x.get(&format!("/tasks/{root}/graph?limit=1")).await;
    assert_eq!(g["parent"]["id"], json!(parent));
    assert_eq!(g["parent"]["title"], "Epic");
    assert_eq!(g["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(g["truncated"], true);
    let omitted = &g["omitted"];
    assert_eq!(omitted["tasks"], "3");
    assert_eq!(omitted["tasks_by_state"]["completed"], "2");
    assert_eq!(omitted["tasks_by_state"]["active"], "1");
    assert_eq!(omitted["other_nodes"], "1");
    assert_eq!(omitted["edges"], "4");
    assert_eq!(omitted["exact"], true);
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
    x.put_context(held, blocked_by(&task(held, w, "Held", "active"), &open));
    x.put_context(
        waiting,
        blocked_by(&task(waiting, w, "Waiting", "active"), &open),
    );
    x.put_context(
        ready,
        blocked_by(&task(ready, w, "Ready", "active"), &closed),
    );
    let unsure = task(unknown, w, "Unknown", "active");
    x.put_context(unknown, context(&unsure, vec![], vec![], true));

    let running = x.work(held).await;
    assert_eq!(running["readiness"]["state"], "running");
    assert_eq!(running["execution"]["task_id"], json!(held));
    assert!(running["execution"]["session_id"].is_string());

    let blocked = x.work(waiting).await;
    assert_eq!(blocked["readiness"]["state"], "waiting");
    assert_eq!(blocked["execution"], Value::Null);
    let b = &blocked["readiness"]["blockers"][0];
    assert_eq!(b["id"], json!(blocker));
    assert_eq!(b["title"], "Schema first");
    assert_eq!(b["satisfied"], false);

    let free = x.work(ready).await;
    assert_eq!(free["readiness"]["state"], "ready");
    assert_eq!(free["readiness"]["blockers"][0]["satisfied"], true);

    let unsure = x.work(unknown).await;
    assert_eq!(unsure["readiness"]["state"], "unknown");
    assert_eq!(unsure["readiness"]["complete"], false);
}

/// Graph's ready query leaves out a task under a draft or retired ancestor
/// and a container with live children; the work read must not say `ready`.
#[tokio::test]
async fn work_names_the_hierarchy_that_keeps_an_active_task_out_of_the_queue() {
    let x = setup().await;
    draft_parent_holds_a_child_with_a_satisfied_prerequisite(&x).await;
    draft_or_retired_grandparent_holds_the_child(&x).await;
    active_ancestors_leave_a_runnable_leaf_ready(&x).await;
    live_children_hold_an_active_container(&x).await;
    unreturned_parent_or_truncated_context_is_unknown(&x).await;
    terminal_tasks_and_a_running_attempt_keep_their_state(&x).await;
}

/// The reproduced case: a satisfied prerequisite and a draft parent.
async fn draft_parent_holds_a_child_with_a_satisfied_prerequisite(x: &Fixture) {
    let w = x.w();
    let mut chain = Chain::new(w, ["draft", "active"]);
    let parent = chain.parent;
    let done = Uuid::new_v4();
    chain.nodes.push(task(done, w, "Done", "completed"));
    chain.edges.push(edge(w, done, chain.child, "blocks"));
    let child = x.put_chain(chain, "active", false);
    let r = x.work(child).await;
    assert_eq!(r["readiness"]["state"], "waiting", "{r}");
    assert_eq!(r["readiness"]["blockers"][0]["satisfied"], true);
    let h = &r["readiness"]["hierarchy"];
    assert_eq!(h.as_array().unwrap().len(), 1, "{h}");
    assert_eq!(h[0]["relationship"], "ancestor");
    assert_eq!(
        (&h[0]["code"], &h[0]["level"]),
        (&json!("ancestor_draft"), &json!(1))
    );
    assert_eq!(
        (&h[0]["id"], &h[0]["task_state"]),
        (&json!(parent), &json!("draft"))
    );
    assert_eq!(r["parent"]["id"], json!(parent));
}

/// The grandparent is the third level; a retired one blocks as well.
async fn draft_or_retired_grandparent_holds_the_child(x: &Fixture) {
    let chain = Chain::new(x.w(), ["active", "draft"]);
    let grand = chain.grand;
    let child = x.put_chain(chain, "active", false);
    let r = x.work(child).await;
    assert_eq!(r["readiness"]["state"], "waiting", "{r}");
    assert_eq!(r["readiness"]["hierarchy"][0]["id"], json!(grand));
    assert_eq!(r["readiness"]["hierarchy"][0]["level"], 2);

    let mut chain = Chain::new(x.w(), ["active", "active"]);
    let grand = chain.grand;
    chain.nodes[1]["retired"] = json!(true);
    let child = x.put_chain(chain, "active", false);
    let r = x.work(child).await;
    assert_eq!(r["readiness"]["state"], "waiting", "{r}");
    assert_eq!(r["readiness"]["hierarchy"][0]["code"], "ancestor_retired");
    assert_eq!(r["readiness"]["hierarchy"][0]["id"], json!(grand));
}

/// Active ancestors: a genuinely runnable leaf stays ready, with no new field.
async fn active_ancestors_leave_a_runnable_leaf_ready(x: &Fixture) {
    let child = x.put_chain(Chain::new(x.w(), ["active", "active"]), "active", false);
    let r = x.work(child).await;
    assert_eq!(r["readiness"]["state"], "ready", "{r}");
    assert!(r["readiness"].get("hierarchy").is_none());
}

/// An active container is not a leaf: its live children run instead.
async fn live_children_hold_an_active_container(x: &Fixture) {
    let w = x.w();
    let (container, kid) = (Uuid::new_v4(), Uuid::new_v4());
    x.put_context(
        container,
        context(
            &task(container, w, "Child", "active"),
            vec![task(kid, w, "Kid", "active")],
            vec![edge(w, container, kid, "parent-child")],
            false,
        ),
    );
    let r = x.work(container).await;
    assert_eq!(r["readiness"]["state"], "waiting", "{r}");
    let h = &r["readiness"]["hierarchy"][0];
    assert_eq!(
        (&h["relationship"], &h["code"]),
        (&json!("child"), &json!("has_live_children"))
    );
    assert_eq!(h["id"], json!(kid));
}

/// A parent Graph did not return, or a truncated Context, is unknown.
async fn unreturned_parent_or_truncated_context_is_unknown(x: &Fixture) {
    let w = x.w();
    let (child, parent) = (Uuid::new_v4(), Uuid::new_v4());
    x.put_context(
        child,
        context(
            &task(child, w, "Child", "active"),
            vec![],
            vec![edge(w, parent, child, "parent-child")],
            false,
        ),
    );
    let r = x.work(child).await;
    assert_eq!(r["readiness"]["state"], "unknown", "{r}");
    assert_eq!(r["readiness"]["complete"], false);
    let child = x.put_chain(Chain::new(w, ["active", "active"]), "active", true);
    assert_eq!(x.work(child).await["readiness"]["state"], "unknown");
}

/// Terminal tasks and a running attempt keep their own state.
async fn terminal_tasks_and_a_running_attempt_keep_their_state(x: &Fixture) {
    let w = x.w();
    for state in ["completed", "canceled", "draft"] {
        let child = x.put_chain(Chain::new(w, ["draft", "draft"]), state, false);
        let r = x.work(child).await;
        assert_eq!(r["readiness"]["state"], state, "{r}");
        assert!(r["readiness"].get("hierarchy").is_none());
    }
    let held = x.held();
    let chain = Chain::new(w, ["draft", "active"]);
    let held_under_draft = context(
        &task(held, w, "Child", "active"),
        chain.nodes,
        vec![edge(w, chain.parent, held, "parent-child")],
        false,
    );
    x.put_context(held, held_under_draft);
    let r = x.work(held).await;
    assert_eq!(r["readiness"]["state"], "running", "{r}");
}

fn role(id: &str, source: &str) -> Value {
    json!({"id":id,"source":source})
}

/// Spec 0014 §5.5: every task read shows the effective role and where it
/// comes from, and the work read holds a task whose role is disabled or gone.
#[tokio::test]
async fn task_reads_show_the_effective_role_and_hold_a_disabled_or_missing_one() {
    let x = setup().await;
    let (w, pool) = (x.w(), x.pool().await);
    let named = |id: Uuid, title: &str, role: &str| {
        let mut t = task(id, w, title, "active");
        t["metadata"] = json!({"aq:role":role,"aq:task_type":"feature"});
        t
    };
    let (plain, review, gone, draft) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    common::save_role(&pool, x.f.actor, "developer", "tester", true).await;
    let mut drafted = named(draft, "Draft", "reviewer");
    drafted["task_state"] = json!("draft");
    x.put_mutual(&[
        (plain, task(plain, w, "Plain", "active")),
        (review, named(review, "Review", "reviewer")),
        (gone, named(gone, "Gone", "tester")),
        (draft, drafted),
    ]);
    let expected = [
        (plain, role("developer", "project_default")),
        (review, role("reviewer", "task")),
        (gone, role("tester", "task")),
        (draft, role("reviewer", "task")),
    ];
    every_task_read_shows_the_role(&x, plain, &expected).await;
    assert_eq!(x.work(review).await["readiness"]["state"], "ready");

    // A disabled role and a role that no longer exists hold their tasks.
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", false).await;
    common::delete_role(&pool, x.f.actor, "tester").await;
    disabled_and_missing_roles_hold_their_tasks(&x, review, gone).await;
    // A task on the default role is not affected, and a draft is not held.
    let free = x.work(plain).await;
    assert_eq!(free["readiness"]["state"], "ready");
    assert_eq!(free["readiness"]["reasons"], json!([]));
    let idle = x.work(draft).await;
    assert_eq!(idle["readiness"]["state"], "draft");
    assert_eq!(idle["readiness"]["reasons"], json!([]));
    // Enabling the role again releases the task.
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", true).await;
    assert_eq!(x.work(review).await["readiness"]["state"], "ready");
}

/// Task detail, the task page, the graph (around `root`) and the work read
/// agree on each task's role, and nothing holds a task yet.
async fn every_task_read_shows_the_role(x: &Fixture, root: Uuid, expected: &[(Uuid, Value)]) {
    let page = x.get("/tasks?limit=10").await;
    let graph = x.get(&format!("/tasks/{root}/graph")).await;
    for (id, role) in expected {
        assert_eq!(&x.get(&format!("/tasks/{id}")).await["role"], role);
        for listed in [&page["items"], &graph["nodes"]] {
            let found = listed.as_array().unwrap().iter();
            let found = found.clone().find(|n| n["id"] == json!(id)).unwrap();
            assert_eq!(&found["role"], role, "{found}");
        }
        let work = x.work(*id).await;
        assert_eq!(&work["role"], role);
        assert_eq!(work["readiness"]["reasons"], json!([]), "{work}");
    }
}

async fn disabled_and_missing_roles_hold_their_tasks(x: &Fixture, review: Uuid, gone: Uuid) {
    let held = x.work(review).await;
    assert_eq!(held["readiness"]["state"], "held", "{held}");
    let reason = &held["readiness"]["reasons"][0];
    assert_eq!(reason["code"], "role_disabled");
    assert_eq!(reason["role"], "reviewer");
    assert_eq!(
        reason["message"],
        "AQ does not offer this task: its role `reviewer` is disabled. Enable the role in Settings or change the task's role."
    );
    let missing = x.work(gone).await;
    assert_eq!(missing["readiness"]["state"], "held", "{missing}");
    assert_eq!(missing["role"], role("tester", "task"));
    let reason = &missing["readiness"]["reasons"][0];
    assert_eq!(
        (&reason["code"], &reason["role"]),
        (&json!("role_missing"), &json!("tester"))
    );
    assert!(
        reason["message"]
            .as_str()
            .unwrap()
            .contains("its role `tester` does not exist")
    );
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
    let held = x.held();
    let node = task(held, x.w(), "Held", "active");
    x.graph.nodes.lock().unwrap().insert(held, node);
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
    // Spec 0014 §7.1: an attempt of the timeline shows what it ran with, in
    // the shape of the attached execution; without a summary, no key.
    assert!(h["attempts"][0].get("ran_with").is_none());
    let attempt: Uuid = h["attempts"][0]["attempt_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let summary = record_ran_with(&x, attempt).await;
    let shown = x.get(&format!("/tasks/{held}/history")).await;
    assert_eq!(
        shown["attempts"][0]["ran_with"],
        serde_json::to_value(&summary).unwrap()
    );
}

async fn record_ran_with(x: &Fixture, attempt: Uuid) -> aq_role_settings::attempts::RanWith {
    let summary = aq_role_settings::attempts::RanWith {
        role: "developer".into(),
        level: "standard-high".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-5-5".into(),
        reasoning: "high".into(),
        harness: "claude".into(),
        execution_profile: "sandbox".into(),
        snapshot_digest: None,
    };
    let pool = x.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    aq_role_settings::attempts::record(&mut conn, attempt, &summary)
        .await
        .unwrap();
    summary
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
    let implementation = &s["implementation"];
    assert_eq!(implementation["total"], "3");
    assert_eq!(implementation["by_state"]["completed"], "2");
    assert_eq!(implementation["by_state"]["active"], "1");
    assert_eq!(implementation["complete"], false);
    assert_eq!(s["delivery"]["state"], "unavailable");
}

#[tokio::test]
async fn work_reports_a_stopped_holding_attempt_as_held_with_its_reason() {
    // QA P1-01: the MS1 cancel drill's attempt was cancelled and stopped, but
    // keeps the task claim; readiness said `running`.
    let x = setup().await;
    let held = x.held();
    let attempt = x.f.profiled.offer.binding.attempt_id;
    x.put_context(held, alone(&task(held, x.w(), "Held", "active")));
    let running = x.work(held).await;
    assert_eq!(running["readiness"]["state"], "running");
    assert_eq!(running["readiness"]["reasons"], json!([]));

    cancel_and_stop_keeping_the_claim(&x, attempt).await;

    let held_work = x.work(held).await;
    let readiness = &held_work["readiness"];
    assert_eq!(readiness["state"], "held", "{held_work}");
    assert_eq!(
        readiness["reasons"][0]["code"],
        "cancelled_attempt_holds_task"
    );
    assert_eq!(readiness["reasons"][0]["attempt_id"], json!(attempt));
    assert_eq!(held_work["execution"]["status"], "finished");
}

/// Records the attempt's authority as finished, cancelled and stopped, with
/// its launch owner and claim still in place.
async fn cancel_and_stop_keeping_the_claim(x: &Fixture, attempt: Uuid) {
    use aq_server::execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Status, Stop},
    };
    let pool = x.pool().await;
    let mut hot = HotState::from_compat(&x.f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)")
        .bind(attempt).bind(json!(hot)).execute(&pool).await.unwrap();
    hot.status = Status::Finished;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    hot.stop = Stop::Stopped {
        receipt: common::profiled::artifact("stop", b"stop"),
    };
    hot.cancellation_id = Some(Uuid::new_v4());
    hot.revoked = true;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn history_entries_say_what_changed() {
    // QA P1-08: entries named only the operation and category.
    let x = setup().await;
    let w = x.w();
    let held = x.held();
    let blocker = Uuid::new_v4();
    let snapshot = |revision: u64, priority: i64, title: &str| {
        let mut n = task(held, w, title, "active");
        n["content_revision"] = json!(revision);
        n["metadata"] = json!({"aq:priority": priority});
        n
    };
    x.graph
        .nodes
        .lock()
        .unwrap()
        .insert(held, task(held, w, "Held", "active"));
    x.graph.revisions.lock().unwrap().insert(
        1,
        json!({"node":snapshot(1, 0, "Held"),"links":null,"relationships_available":false}),
    );
    let transition = json!({"from":"active","to":"completed","version":3});
    *x.graph.history.lock().unwrap() = Some(json!({"items":[
        history_entry(5, "revision", "updated", snapshot(2, 5, "Held")),
        history_entry(6, "lifecycle", "transitioned", transition),
        history_entry(7, "edge", "added", edge(w, blocker, held, "blocks")),
        history_entry(8, "lifecycle", "set_task_state", secret_data()),
    ],"next_cursor":null}));
    let h = x.get(&format!("/tasks/{held}/history")).await;
    let items = h["items"].as_array().unwrap();
    assert_eq!(
        items[0]["change"]["fields"],
        json!([{"field":"priority","from":"0","to":"5"}])
    );
    assert_eq!(
        items[0]["change"]["complete"], true,
        "revision 1 read from Graph"
    );
    assert_eq!(items[1]["change"]["summary"], "active → completed");
    assert_eq!(
        items[2]["change"]["edge"],
        json!({"edge_type":"blocks","direction":"incoming","node_id":blocker})
    );
    assert_eq!(items[3]["change"], Value::Null);
}

#[test]
fn work_and_history_examples_match_the_schema() {
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    for example in [
        include_str!("../../../../protocol/human-graph/v1/examples/work.json"),
        include_str!("../../../../protocol/human-graph/v1/examples/history.json"),
    ] {
        let body: Value = serde_json::from_str(example).unwrap();
        assert!(aq_agent_api::check(&schema, &schema, &body), "{body}");
    }
    // An unknown readiness state or hold code is rejected.
    let mut work: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-graph/v1/examples/work.json"
    ))
    .unwrap();
    work["data"]["readiness"]["reasons"][0]["code"] = json!("held_for_fun");
    assert!(!aq_agent_api::check(&schema, &schema, &work));
}

async fn set_routing(pool: &sqlx::PgPool, enabled: bool) {
    sqlx::query("UPDATE routing_state SET enabled=$1 WHERE singleton")
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn work_read_reports_waiting_then_routed() {
    let x = setup().await;
    let (w, id) = (x.w(), Uuid::new_v4());
    let node = task(id, w, "Unrouted", "active");
    x.put_context(id, alone(&node));
    let pool = x.pool().await;
    set_routing(&pool, true).await;
    let waiting = x.work(id).await;
    assert_eq!(waiting["readiness"]["state"], "ready");
    // The read has no ready time, so the node's updated_at (2026-01-01T00:00:00Z) is used.
    assert_eq!(
        waiting["routing"],
        json!({"state":"waiting","since_ms":1767225600000_i64})
    );
    // Routing on, not ready (an open blocker), no route: nothing is held back for routing.
    let (blocked, blocker) = (Uuid::new_v4(), Uuid::new_v4());
    x.put_context(
        blocked,
        blocked_by(
            &task(blocked, w, "Blocked", "active"),
            &task(blocker, w, "First", "active"),
        ),
    );
    let held = x.work(blocked).await;
    assert_eq!(held["readiness"]["state"], "waiting");
    assert_eq!(held["routing"], json!({"state":"unrouted"}));
    // Routing off and no route: the owner can still route by hand.
    set_routing(&pool, false).await;
    assert_eq!(x.work(id).await["routing"], json!({"state":"unrouted"}));
    set_routing(&pool, true).await;

    route_by_owner(&x, &node).await;
    assert_eq!(
        x.work(id).await["routing"],
        json!({"state":"routed","since_ms":1790000000000_i64,"route":{"class":"standard-high",
            "harnesses":["codex"],"providers":["openai"],"prefer":[],"lane":"backend",
            "provider_intent":"any","source":"owner","policy_digest":null}})
    );
    swarm_ceiling_holds_a_routed_task_until_it_expires(&x, id).await;
}

/// Records an owner route for `node` and serves the node with it.
async fn route_by_owner(x: &Fixture, node: &Value) {
    let id: Uuid = node["id"].as_str().unwrap().parse().unwrap();
    let route: aq_routing::Route = serde_json::from_value(json!({"class":"standard-high",
        "harnesses":["codex"],"providers":["openai"],"prefer":[],"lane":"backend",
        "provider_intent":"any","reasons":[],"classification":null,"policy_digest":null,
        "structure_revision":null,"source":"owner"}))
    .unwrap();
    sqlx::query(
        "INSERT INTO task_routes(task_id,sequence,route,at_ms) VALUES($1,1,$2,1790000000000)",
    )
    .bind(id)
    .bind(sqlx::types::Json(&route))
    .execute(&x.pool().await)
    .await
    .unwrap();
    let mut routed = node.clone();
    routed["metadata"] =
        aq_routing::with_route(&node["metadata"], Some(&route.compact(1).unwrap())).unwrap();
    x.put_context(id, alone(&routed));
}

async fn swarm_ceiling_holds_a_routed_task_until_it_expires(x: &Fixture, id: Uuid) {
    let pool = x.pool().await;
    sqlx::query("INSERT INTO swarm_waiting(task_id,provider,intelligence_class,reason,observed_at_ms,expires_at_ms) VALUES($1,'openai','standard-high','swarm_concurrency_limit_reached',0,$2)")
        .bind(id).bind(i64::MAX).execute(&pool).await.unwrap();
    let queued = x.work(id).await;
    assert_eq!(queued["readiness"]["state"], "waiting");
    assert_eq!(queued["routing"]["state"], "routed");
    let reason = &queued["readiness"]["reasons"][0];
    assert_eq!(reason["code"], aq_swarm_settings::admission::CEILING);
    assert!(
        reason["message"]
            .as_str()
            .unwrap()
            .contains("openai standard-high")
    );
    sqlx::query("UPDATE swarm_waiting SET expires_at_ms=1 WHERE task_id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(x.work(id).await["readiness"]["state"], "ready");
}

#[tokio::test]
async fn routes_history_lists_reasons_newest_first() {
    let x = setup().await;
    let id = Uuid::new_v4();
    let pool = x.pool().await;
    let route = json!({"class":"complex-max","harnesses":[],"providers":[],"prefer":["codex"],
        "lane":null,"provider_intent":"pinned",
        "reasons":[{"code":"class_raised_for_risk","detail":"Risk is high, so the class is raised."}],
        "classification":null,"policy_digest":"ab12","structure_revision":3,"source":"router"});
    for (sequence, route, at) in [(1, Some(route.clone()), 1000_i64), (2, None, 2000)] {
        sqlx::query("INSERT INTO task_routes(task_id,sequence,route,at_ms) VALUES($1,$2,$3,$4)")
            .bind(id)
            .bind(sequence)
            .bind(route.map(sqlx::types::Json))
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
    }
    let app = aq_server::build_admin_router(x.f.state.clone());
    let request = Request::get(format!("/api/tasks/{id}/routes"))
        .header("x-aq-dev-email", "owner@localhost")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body,
        json!({"items":[{"sequence":2,"at_ms":2000,"route":null},
            {"sequence":1,"at_ms":1000,"route":route}]})
    );
}

/// Spec 0017 §5.4: every task read shows what the task needs (`aq:needs`,
/// sorted; empty when it names none).
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn task_reads_show_the_needs() {
    let x = setup().await;
    let w = x.w();
    let (plain, web) = (Uuid::new_v4(), Uuid::new_v4());
    let mut needing = task(web, w, "Web", "active");
    needing["metadata"] = json!({"aq:needs":["playwright","node"]});
    x.put_mutual(&[(plain, task(plain, w, "Plain", "active")), (web, needing)]);
    let page = x.get("/tasks?limit=10").await;
    let graph = x.get(&format!("/tasks/{plain}/graph")).await;
    for (id, needs) in [(plain, json!([])), (web, json!(["node", "playwright"]))] {
        assert_eq!(x.get(&format!("/tasks/{id}")).await["needs"], needs);
        assert_eq!(x.work(id).await["needs"], needs);
        for listed in [&page["items"], &graph["nodes"]] {
            let mut found = listed.as_array().unwrap().iter();
            let found = found.find(|n| n["id"] == json!(id)).unwrap();
            assert_eq!(found["needs"], needs, "{found}");
        }
    }
}
