//! G2 typed task and knowledge commands over a fake public Graph API.
use crate::common;
use aq_server::agent_access::{AgentAccessConfig, Runtime};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, State},
    http::{HeaderMap, Request, StatusCode},
    response::Response,
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;

/// Idempotency key -> (original request bytes, stored receipt).
type Receipts = Arc<Mutex<HashMap<String, (Vec<u8>, Value)>>>;

#[derive(Clone, Default)]
pub(crate) struct Graph {
    pub(crate) nodes: Arc<Mutex<HashMap<Uuid, Value>>>,
    pub(crate) posts: Arc<Mutex<Vec<Value>>>,
    pub(crate) reply: Arc<Mutex<Option<(u16, Value)>>>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    receipts: Receipts,
    timeout_after_commit: Arc<AtomicBool>,
    committed: Arc<tokio::sync::Notify>,
    history: Arc<Mutex<Option<Value>>>,
    tree: Arc<Mutex<Option<Value>>>,
}
fn graph_response(status: u16, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn read_node(State(g): State<Graph>, Path((_, id)): Path<(Uuid, Uuid)>) -> Response {
    match g.nodes.lock().unwrap().get(&id) {
        Some(n) => graph_response(200, n),
        None => graph_response(
            404,
            &json!({"error":{"code":"not_found","message":"missing"}}),
        ),
    }
}
async fn mutate(State(g): State<Graph>, headers: HeaderMap, bytes: Bytes) -> Response {
    assert_eq!(headers["X-AQ-Graph-API-Version"], "1");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    g.posts.lock().unwrap().push(body.clone());
    g.bodies.lock().unwrap().push(bytes.to_vec());
    let key = body["idempotency_key"].as_str().unwrap().to_owned();
    if let Some((original, receipt)) = g.receipts.lock().unwrap().get(&key) {
        assert_eq!(
            original,
            bytes.as_ref(),
            "replay must preserve the exact body"
        );
        return graph_response(200, receipt);
    }
    let (status, mut reply) = g
        .reply
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| g.stage_updates(&body));
    if status != 200 {
        return graph_response(status, &reply);
    }
    apply_create_fields(&body, &mut reply);
    g.commit(key, &bytes, &reply);
    if g.timeout_after_commit.swap(false, Ordering::SeqCst) {
        g.committed.notify_one();
        return std::future::pending().await;
    }
    graph_response(status, &reply)
}

impl Graph {
    fn stage_updates(&self, body: &Value) -> (u16, Value) {
        assert!(self.tree.lock().unwrap().is_some(), "configured reply");
        // Stage every result without storing any changes until all expected
        // versions pass, as Graph does in its mutation transaction.
        let nodes = self.nodes.lock().unwrap();
        let mut results = Vec::new();
        for op in body["operations"].as_array().unwrap() {
            let id: Uuid = op["id"].as_str().unwrap().parse().unwrap();
            let mut node = nodes[&id].clone();
            assert_eq!(op["op"], "update_node");
            if node["version"] != op["expected_version"] {
                return (
                    409,
                    json!({"error":{"code":"conflict","message":"stale version"}}),
                );
            }
            if op["patch"] == json!({"task_state":"active"}) {
                assert_eq!(node["task_state"], "draft");
                node["task_state"] = json!("active");
                node["version"] = json!(node["version"].as_u64().unwrap() + 1);
            } else {
                assert_eq!(op["patch"], json!({}));
            }
            results.push(node);
        }
        (200, change_set(results))
    }
    fn commit(&self, key: String, bytes: &[u8], reply: &Value) {
        // The real Graph commits state and the actor/workspace/key receipt in
        // one transaction. A repeated request returns that original receipt.
        self.receipts
            .lock()
            .unwrap()
            .insert(key, (bytes.to_vec(), reply.clone()));
        for result in reply["results"].as_array().unwrap() {
            if result.get("kind").is_some() {
                let id = result["id"].as_str().unwrap().parse().unwrap();
                self.nodes.lock().unwrap().insert(id, result.clone());
            }
        }
    }
}
fn apply_create_fields(body: &Value, reply: &mut Value) {
    // Model public Graph create defaults and echo the admitted create fields.
    // Native task.create validates every returned task field against intent.
    for result in reply["results"].as_array_mut().unwrap() {
        let Some(op) = body["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|op| op["op"] == "create_node" && op["id"] == result["id"])
        else {
            continue;
        };
        for key in [
            "title",
            "body",
            "acceptance_criteria",
            "labels",
            "task_state",
            "metadata",
        ] {
            if let Some(value) = op.get(key) {
                result[key] = value.clone();
            }
        }
        result["created_by"] = json!("dashboard-fixture-service");
    }
}

/// A page of nodes of `kind`, in id order; the cursor is the last id served.
async fn list_nodes(
    State(g): State<Graph>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let limit: usize = q["limit"].parse().unwrap();
    let mut nodes: Vec<Value> = g.nodes.lock().unwrap().values().cloned().collect();
    nodes.retain(|n| {
        n["kind"] == q["kind"].as_str()
            && q.get("cursor")
                .is_none_or(|c| n["id"].as_str().unwrap() > c.as_str())
    });
    nodes.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let next = (nodes.len() > limit).then(|| nodes[limit - 1]["id"].clone());
    nodes.truncate(limit);
    graph_response(200, &json!({"items":nodes,"next_cursor":next}))
}

async fn read_tree(State(g): State<Graph>) -> Response {
    graph_response(
        200,
        g.tree.lock().unwrap().as_ref().expect("subtree fixture"),
    )
}

async fn read_history(State(g): State<Graph>) -> Response {
    let page = g.history.lock().unwrap().clone();
    graph_response(200, &page.unwrap_or(json!({"items":[],"next_cursor":null})))
}

struct Fixture {
    f: common::profiled::Fixture,
    graph: Graph,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn setup() -> Fixture {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    setup_runtime(f).await
}
async fn setup_runtime(mut f: common::profiled::Fixture) -> Fixture {
    let (graph, server, dir) = graph_runtime(&mut f.state).await;
    Fixture {
        f,
        graph,
        server,
        _dir: dir,
    }
}
/// A fake public Graph API on a local port, and the agent access runtime in
/// `state` that reaches it. The caller keeps the server and key directory.
pub(crate) async fn graph_runtime(
    state: &mut aq_server::AppState,
) -> (Graph, tokio::task::JoinHandle<()>, tempfile::TempDir) {
    let graph = Graph::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/workspaces/{w}/nodes", get(list_nodes))
        .route("/v1/workspaces/{w}/nodes/{id}", get(read_node))
        .route("/v1/workspaces/{w}/nodes/{id}/history", get(read_history))
        .route("/v1/workspaces/{w}/nodes/{id}/tree", get(read_tree))
        .route("/v1/workspaces/{w}/mutations", post(mutate))
        .with_state(graph.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-graph-service-key-000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = Runtime::new(&AgentAccessConfig {
        graph_url: url,
        graph_token_file: key,
        mutation_actor_id: Some("dashboard-fixture-service".into()),
        credential_ttl_ms: 120000,
    })
    .await
    .unwrap();
    state.agent_access = Some(Arc::new(runtime));
    (graph, server, dir)
}
impl Fixture {
    fn workspace(&self) -> Uuid {
        self.f.selection.lineage.workspace_id
    }
    fn project(&self) -> Uuid {
        self.f.selection.lineage.project_id
    }
    fn put(&self, node: Value) {
        let id = node["id"].as_str().unwrap().parse().unwrap();
        self.graph.nodes.lock().unwrap().insert(id, node);
    }
    fn reply(&self, status: u16, body: Value) {
        *self.graph.reply.lock().unwrap() = Some((status, body));
    }
    fn posts(&self) -> Vec<Value> {
        self.graph.posts.lock().unwrap().clone()
    }
    async fn act(&self, action: &Value) -> (StatusCode, Value) {
        let app = aq_server::build_admin_router(self.f.state.clone());
        let res = app
            .oneshot(
                Request::post("/api/v1/actions")
                    .header("x-aq-dev-email", "owner@localhost")
                    .header("x-aq-request", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(action.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    async fn lookup(&self, id: Uuid) -> Value {
        // A fresh router/registry proves recovery does not depend on adapter memory.
        let res = aq_server::build_admin_router(self.f.state.clone())
            .oneshot(
                Request::get(format!("/api/v1/operations/{id}"))
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
}
pub(crate) fn node(id: Uuid, workspace: Uuid, kind: &str, version: u64) -> Value {
    let knowledge = kind == "knowledge";
    json!({"id":id,"workspace_id":workspace,"kind":kind,"version":version,"content_revision":version,
        "title":"Original title","body":"Original body","acceptance_criteria":"","labels":[],
        "metadata":{"example:weight":2},"task_state":if knowledge {Value::Null} else {json!("active")},
        "verification":"unverified","provenance":[],"retired":false,"deleted":false,
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","created_by":"fixture",
        "last_verified_by":null,"last_verified_at":null,"retirement_reason":null,
        "content_type":"note","summary":null,
        "summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,"successor_id":null})
}
pub(crate) enum Subject<'a> {
    Task(Uuid, &'a str),
    Project(Uuid, &'a str),
    Knowledge(Uuid, &'a str),
}
pub(crate) fn action(kind: &str, subject: Subject<'_>, payload: Value) -> Value {
    let (entity, id, revision) = match subject {
        Subject::Task(id, revision) => ("task", id, revision),
        Subject::Project(id, revision) => ("project", id, revision),
        Subject::Knowledge(id, revision) => ("knowledge", id, revision),
    };
    json!({"operation_id":payload["operation_id"],"kind":kind,
        "subject":{"type":entity,"id":id,"revision":revision},
        "expected_versions":{entity:revision},"payload":payload})
}
pub(crate) fn change_set(results: Vec<Value>) -> Value {
    json!({"change_set_id":Uuid::new_v4(),"results":results})
}

impl Fixture {
    fn route_action(&self, kind: &str, (task, version): (Uuid, &str), fields: Value) -> Value {
        let op = Uuid::new_v4();
        let mut payload =
            json!({"operation_id":op,"project_id":self.project(),"expected_version":version});
        payload
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        action(kind, Subject::Task(task, version), payload)
    }
}

mod history;
mod knowledge;
mod lifecycle;
mod routing;
mod supervisors;
mod task_creation;
use task_creation::{edge, graph_create, single_create};

const UNKNOWN_TESTER: &str = "Role `tester` does not exist. The roles are: developer, reviewer.";

fn assert_unknown_role(status: StatusCode, body: &Value, message: &str) {
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_role", "{body}");
    assert_eq!(body["message"], message);
}
