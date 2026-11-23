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
    let (status, mut reply) = g.reply.lock().unwrap().clone().unwrap_or_else(|| {
        assert!(g.tree.lock().unwrap().is_some(), "configured reply");
        // Stage every result without storing any changes until all expected
        // versions pass, as Graph does in its mutation transaction.
        let nodes = g.nodes.lock().unwrap();
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
    });
    if status == 200 {
        // Model public Graph create defaults and echo the admitted create fields.
        // Native task.create validates every returned task field against intent.
        for result in reply["results"].as_array_mut().unwrap() {
            if let Some(op) = body["operations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|op| op["op"] == "create_node" && op["id"] == result["id"])
            {
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
    }
    if status == 200 {
        // The real Graph commits state and the actor/workspace/key receipt in
        // one transaction. A repeated request returns that original receipt.
        g.receipts
            .lock()
            .unwrap()
            .insert(key, (bytes.to_vec(), reply.clone()));
        for result in reply["results"].as_array().unwrap() {
            if result.get("kind").is_some() {
                let id = result["id"].as_str().unwrap().parse().unwrap();
                g.nodes.lock().unwrap().insert(id, result.clone());
            }
        }
        if g.timeout_after_commit.swap(false, Ordering::SeqCst) {
            g.committed.notify_one();
            return std::future::pending().await;
        }
    }
    graph_response(status, &reply)
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
pub(crate) fn action(kind: &str, entity: &str, id: Uuid, revision: &str, payload: Value) -> Value {
    json!({"operation_id":payload["operation_id"],"kind":kind,
        "subject":{"type":entity,"id":id,"revision":revision},
        "expected_versions":{entity:revision},"payload":payload})
}
pub(crate) fn change_set(results: Vec<Value>) -> Value {
    json!({"change_set_id":Uuid::new_v4(),"results":results})
}

#[tokio::test]
async fn knowledge_correct_with_stale_version_conflicts() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "knowledge.correct",
            "knowledge",
            id,
            "2",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"2","title":"Corrected"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "conflict", "{status} {body}");
    assert_eq!(body["data"]["current_versions"], json!({"knowledge":"3"}));
    assert_eq!(body["data"]["recovery"], "refresh_and_review");
    assert!(x.posts().is_empty(), "a stale version never reaches Graph");
}

#[tokio::test]
async fn knowledge_correct_confirms_with_change_set() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let mut updated = node(id, x.workspace(), "knowledge", 4);
    updated["title"] = json!("Corrected");
    updated["metadata"]["fraction"] = json!(0.125);
    let result = change_set(vec![updated]);
    x.reply(200, result.clone());
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "knowledge.correct",
            "knowledge",
            id,
            "3",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3","title":"Corrected"}),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    assert_eq!(body["data"]["receipt_refs"][0]["type"], "graph_change_set");
    assert_eq!(
        body["data"]["receipt_refs"][0]["id"],
        result["change_set_id"]
    );
    assert_eq!(
        body["data"]["affected_entities"][0],
        json!({"type":"knowledge","id":id,"revision":"4"})
    );
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0]["idempotency_key"],
        format!("aq-dashboard-action:{op}")
    );
    assert_eq!(
        posts[0]["operations"],
        json!([{"op":"update_node","id":id,"expected_version":3,"patch":{"title":"Corrected"}}])
    );
}

#[tokio::test]
async fn task_reprioritize_preserves_other_metadata() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    let mut original = node(task, x.workspace(), "task", 5);
    original["metadata"]["example:weight"] = json!(2.5);
    x.put(original);
    let mut updated = node(task, x.workspace(), "task", 6);
    updated["metadata"]["aq:priority"] = json!(7);
    x.reply(200, change_set(vec![updated]));
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "task.reprioritize",
            "task",
            task,
            "5",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"5","priority":7}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    assert_eq!(
        posts[0]["operations"][0]["patch"],
        json!({"metadata":{"example:weight":2.5,"aq:priority":7}})
    );
}

fn route_action(kind: &str, task: Uuid, version: &str, x: &Fixture, fields: Value) -> Value {
    let op = Uuid::new_v4();
    let mut payload =
        json!({"operation_id":op,"project_id":x.project(),"expected_version":version});
    payload
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    action(kind, "task", task, version, payload)
}

#[tokio::test]
async fn task_route_set_writes_an_owner_route_and_is_refused_for_a_class_without_a_level() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x
        .act(&route_action(
            "task.route.set",
            task,
            "5",
            &x,
            json!({"class":"standard-high","harnesses":["codex"]}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    let metadata = &posts[0]["operations"][0]["patch"]["metadata"];
    assert_eq!(metadata["example:weight"], 2);
    assert_eq!(metadata["aq:route"]["c"], "standard-high");
    assert_eq!(metadata["aq:route"]["h"], json!(["codex"]));
    assert_eq!(metadata["aq:route"]["s"], "owner");
    let history = aq_server::routing::history(&x.f.db.pool().await, task)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    let route = history[0].route.as_ref().unwrap();
    assert_eq!(route.reasons[0].code, "owner_set");
    assert_eq!(route.policy_digest, None);

    let (status, body) = x
        .act(&route_action(
            "task.route.set",
            task,
            "6",
            &x,
            json!({"class":"deep-max"}),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        x.posts().len(),
        1,
        "a class with no level document never reaches Graph"
    );
}

#[tokio::test]
async fn task_route_clear_removes_it() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (_, body) = x
        .act(&route_action(
            "task.route.set",
            task,
            "5",
            &x,
            json!({"class":"standard-high"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut routed = node(task, x.workspace(), "task", 6);
    routed["metadata"] = x.posts()[0]["operations"][0]["patch"]["metadata"].clone();
    x.put(routed);
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x
        .act(&route_action("task.route.clear", task, "6", &x, json!({})))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let posts = x.posts();
    let metadata = &posts[1]["operations"][0]["patch"]["metadata"];
    assert!(metadata.get("aq:route").is_none(), "{metadata}");
    assert_eq!(metadata["example:weight"], 2);
    let history = aq_server::routing::history(&x.f.db.pool().await, task)
        .await
        .unwrap();
    assert!(
        history[0].route.is_none(),
        "the clear is the newest history row"
    );
}

#[tokio::test]
async fn stale_task_version_is_reported_as_stale() {
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 7));
    for (kind, fields) in [
        ("task.route.set", json!({"class":"standard-high"})),
        ("task.route.clear", json!({})),
    ] {
        let (status, body) = x.act(&route_action(kind, task, "5", &x, fields)).await;
        assert_eq!(
            body["data"]["disposition"], "conflict",
            "{kind} {status} {body}"
        );
        assert_eq!(body["data"]["current_versions"], json!({"task":"7"}));
    }
    assert!(x.posts().is_empty());
}

#[tokio::test]
async fn graph_timeout_after_commit_reconciles_with_original_change_set() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    let mut updated = node(id, x.workspace(), "knowledge", 4);
    updated["metadata"]["fraction"] = json!(0.25);
    let receipt = change_set(vec![updated]);
    x.reply(200, receipt.clone());
    x.graph.timeout_after_commit.store(true, Ordering::SeqCst);
    let op = Uuid::new_v4();
    let request = action(
        "knowledge.correct",
        "knowledge",
        id,
        "3",
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"3","title":"Corrected"}),
    );
    let app = aq_server::build_admin_router(x.f.state.clone());
    let pending = tokio::spawn(
        app.oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        ),
    );
    x.graph.committed.notified().await;
    // Exercise the production POST deadline without a real three-second sleep.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    tokio::time::resume();
    let res = pending.await.unwrap().unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["data"]["disposition"], "unknown", "{body}");

    let pool = x.f.db.pool().await;
    let (key, retained): (String, Vec<u8>) = sqlx::query_as(
        "SELECT idempotency_key,body FROM dashboard_graph_intents WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(key, format!("aq-dashboard-action:{op}"));
    assert_eq!(retained, x.graph.bodies.lock().unwrap()[0]);
    assert!(
        sqlx::query("UPDATE dashboard_graph_intents SET body=$2 WHERE operation_id=$1")
            .bind(op)
            .bind(b"changed".as_slice())
            .execute(&pool)
            .await
            .is_err()
    );

    // Later edits must not change either the replay body or the original receipt.
    x.put(node(id, x.workspace(), "knowledge", 99));
    let resolved = x.lookup(op).await;
    assert_eq!(resolved["data"]["disposition"], "confirmed", "{resolved}");
    assert_eq!(
        resolved["data"]["receipt_refs"][0]["id"],
        receipt["change_set_id"]
    );
    assert_eq!(resolved["data"]["affected_entities"][0]["revision"], "4");
    assert_eq!(x.posts().len(), 2);
    assert_eq!(
        x.graph.receipts.lock().unwrap().len(),
        1,
        "one native mutation committed"
    );
    assert_eq!(
        x.graph.bodies.lock().unwrap().as_slice(),
        &[retained.clone(), retained]
    );
    let terminal = x.lookup(op).await;
    assert_eq!(terminal["data"], resolved["data"]);
    assert_eq!(x.posts().len(), 2, "confirmed receipts never replay");
}

#[tokio::test]
async fn graph_reconcile_keeps_unknown_on_new_rejection_then_recovers() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    x.reply(
        503,
        json!({"error":{"code":"overloaded","message":"unavailable"}}),
    );
    let op = Uuid::new_v4();
    let (_, admitted) = x.act(&action("knowledge.correct", "knowledge", id, "3",
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"3","title":"Corrected"}))).await;
    assert_eq!(admitted["data"]["disposition"], "unknown");
    x.reply(
        403,
        json!({"error":{"code":"forbidden","message":"credential revoked"}}),
    );
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    x.reply(
        200,
        change_set(vec![node(id, x.workspace(), "knowledge", 4)]),
    );
    assert_eq!(x.lookup(op).await["data"]["disposition"], "confirmed");
    let bodies = x.graph.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(bodies.iter().all(|body| body == &bodies[0]));
}

#[tokio::test]
async fn graph_reconcile_refuses_changed_service_or_project_binding() {
    let mut x = setup().await;
    let pool = x.f.db.pool().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 3));
    x.reply(
        503,
        json!({"error":{"code":"overloaded","message":"unavailable"}}),
    );
    let op = Uuid::new_v4();
    let (_, admitted) = x.act(&action("knowledge.correct", "knowledge", id, "3",
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"3","title":"Corrected"}))).await;
    assert_eq!(admitted["data"]["disposition"], "unknown");
    let service: Value = sqlx::query_scalar(
        "SELECT service_identity FROM dashboard_graph_intents WHERE operation_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    let original_runtime = x.f.state.agent_access.clone();
    x.f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: service["origin"].as_str().unwrap().to_owned(),
            graph_token_file: x._dir.path().join("graph-key"),
            mutation_actor_id: Some("another-actor".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    assert_eq!(
        x.posts().len(),
        1,
        "a new actor has a different receipt namespace"
    );
    x.f.state.agent_access = original_runtime;
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(x.project())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(x.lookup(op).await["data"]["disposition"], "unknown");
    assert_eq!(
        x.posts().len(),
        1,
        "a changed binding cannot replay old intent"
    );
}

#[tokio::test]
async fn knowledge_verify_forbidden_is_rejected() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.put(node(id, x.workspace(), "knowledge", 2));
    x.reply(
        403,
        json!({"error":{"code":"forbidden","message":"admin scope required"}}),
    );
    let op = Uuid::new_v4();
    let mut request = action(
        "knowledge.verify",
        "knowledge",
        id,
        "2",
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"2","verification":"verified",
            "evidence":[{"source_id":"review-1","kind":"url","url":"https://example.test/review","observed_at":"2026-01-01T00:00:00Z","retained":false}]}),
    );
    let (status, missing_reason) = x.act(&request).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "verification needs a reason: {missing_reason}"
    );
    request["reason"] = json!("Reviewed against the source");
    let (status, body) = x.act(&request).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    assert_eq!(x.posts()[0]["operations"][0]["op"], "verify_node");
}

#[tokio::test]
async fn knowledge_author_creates_unverified_note() {
    let x = setup().await;
    let op = Uuid::new_v4();
    let mut created = node(op, x.workspace(), "knowledge", 1);
    created["title"] = json!("New note");
    x.reply(200, change_set(vec![created]));
    let (status, body) = x
        .act(&action(
            "knowledge.author",
            "project",
            x.project(),
            "1",
            json!({"operation_id":op,"project_id":x.project(),"expected_project_revision":"1","title":"New note","body":"Durable finding"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operation = &x.posts()[0]["operations"][0];
    assert_eq!(operation["op"], "create_node");
    assert_eq!(operation["id"], json!(op));
    assert_eq!(operation["kind"], "knowledge");
    assert_eq!(operation["verification"], "unverified");
    assert_eq!(operation["content_type"], "note");
    assert_eq!(operation["metadata"]["aq:project_id"], json!(x.project()));
}

#[tokio::test]
async fn task_retry_creates_replacement_attempt() {
    use aq_server::{
        execution::{
            authority::hot::{HotState, LaunchOwner},
            profiled::{Effects, Retention, Status, Stop},
        },
        profiled_execution::replacement::{self, Assessment},
    };
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let predecessor = x.f.profiled.offer.binding.attempt_id;
    x.put(node(task, x.workspace(), "task", 1));
    // Synthetic B4 history: the fixture launch was offered at NOW.
    sqlx::query("ALTER TABLE profiled_launches DISABLE TRIGGER profiled_launch_guard")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',jsonb_build_object('offered',jsonb_build_object('clock',jsonb_build_object('server_time_ms',$2::BIGINT)))) WHERE attempt_id=$1")
        .bind(predecessor).bind(common::profiled::NOW as i64).execute(&pool).await.unwrap();
    sqlx::query("ALTER TABLE profiled_launches ENABLE TRIGGER profiled_launch_guard")
        .execute(&pool)
        .await
        .unwrap();
    let mut hot = HotState::from_compat(&x.f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(predecessor).bind(json!(hot)).execute(&pool).await.unwrap();
    let holds = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1",
        )
        .bind(predecessor)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let retry = |op: Uuid| {
        action(
            "task.retry",
            "task",
            task,
            "1",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"1"}),
        )
    };

    // B4 alone decides: an unsettled predecessor is never force-replaced.
    let (status, body) = x.act(&retry(Uuid::new_v4())).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    // Review M-5: the receipt names the reason.
    assert_eq!(
        body["data"]["reason"], "replacement_prior_execution_uncertain",
        "{body}"
    );
    assert!(holds().await, "rejected retry leaves the claim untouched");

    hot.status = Status::Finished;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    hot.stop = Stop::Stopped {
        receipt: common::profiled::artifact("stop", b"stop"),
    };
    hot.retention = Retention::Retained {
        receipt: common::profiled::artifact("retained", b"retained"),
    };
    hot.effects = Effects::Quiescent {
        receipt: common::profiled::artifact("effects", b"effects"),
    };
    hot.validate_against(&x.f.profiled.offer.binding).unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(predecessor)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    // Retry decides as the offer path does (M2), which also requires that the
    // predecessor's input pins the current project revision: a synthetic
    // linked source export (guards bypassed in this disposable database).
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM project_execution_bindings WHERE project_id=$1")
            .bind(x.project())
            .fetch_one(&pool)
            .await
            .unwrap();
    let export = Uuid::new_v4();
    let request = json!({"project_revision": revision}).to_string();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_source_exports(operation_id,project_id,request_bytes,request_digest,reserved_bytes,charged_bytes,state) VALUES($1,$2,convert_to($3,'UTF8'),encode(sha256(convert_to($3,'UTF8')),'hex'),1048577,1048577,'pending')")
        .bind(export).bind(x.project()).bind(&request).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO profiled_launch_source_exports(attempt_id,operation_id) VALUES($1,$2)",
    )
    .bind(predecessor)
    .bind(export)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let operation = Uuid::new_v4();
    let confirmed_retry = retry(operation);
    // Interrupt the release transaction, leaving the original approval in the journal.
    sqlx::raw_sql(
        "CREATE FUNCTION fail_retry_release() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN IF NEW.released_at IS NOT NULL THEN RAISE EXCEPTION 'interrupted release'; END IF;
      RETURN NEW; END $$;
      CREATE TRIGGER fail_retry_release BEFORE UPDATE ON owner_task_retry_intents
      FOR EACH ROW EXECUTE FUNCTION fail_retry_release();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = x.act(&confirmed_retry).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["data"]["disposition"], "unknown", "{body}");
    assert!(holds().await, "interrupted release rolled back");
    let schedule = || async {
        sqlx::query("UPDATE owner_task_retry_intents SET next_check_at=to_timestamp(1) WHERE operation_id=$1")
            .bind(operation).execute(&pool).await.unwrap();
    };
    let wait_for_check = async |n| {
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            loop {
                let count: i32 = sqlx::query_scalar(
                    "SELECT checks FROM owner_task_retry_intents WHERE operation_id=$1",
                )
                .bind(operation)
                .fetch_one(&pool)
                .await
                .unwrap();
                if count >= n {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("AQ checked the original approval");
    };
    sqlx::raw_sql("DROP TRIGGER fail_retry_release ON owner_task_retry_intents;
      CREATE FUNCTION fail_retry_receipt() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN IF NEW.kind='task.retry' AND NEW.disposition='confirmed' THEN RAISE EXCEPTION 'lost acknowledgement'; END IF;
      RETURN NEW; END $$;
      CREATE TRIGGER fail_retry_receipt BEFORE UPDATE ON dashboard_operations
      FOR EACH ROW EXECUTE FUNCTION fail_retry_receipt();")
        .execute(&pool).await.unwrap();
    let set_role = async |role| {
        sqlx::query("UPDATE users SET role=$2 WHERE id=(SELECT principal_id FROM dashboard_operations WHERE operation_id=$1)")
            .bind(operation).bind(role).execute(&pool).await.unwrap();
    };
    set_role("viewer").await;
    schedule().await;
    let reason = aq_human_work::owner_retry_wait(&pool, x.project(), task)
        .await
        .unwrap();
    assert_eq!(reason["operation_id"], json!(operation));
    assert_eq!(
        reason["wake_condition"],
        "original_retry_authority_restored"
    );

    let (stop, receiver) = tokio::sync::watch::channel(false);
    let sweep = tokio::spawn(aq_server::profiled_execution::expiry::run(
        x.f.state.clone(),
        receiver,
    ));
    wait_for_check(1).await;
    assert!(
        holds().await,
        "retry remains held while owner permission is missing"
    );
    set_role("owner").await;
    schedule().await;
    wait_for_check(2).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        while holds().await {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("AQ releases the same predecessor when permission returns");

    sqlx::query("DROP TRIGGER fail_retry_receipt ON dashboard_operations")
        .execute(&pool)
        .await
        .unwrap();
    schedule().await;
    let body: Value = tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            let receipt: Value = sqlx::query_scalar(
                "SELECT response FROM dashboard_operations WHERE operation_id=$1",
            )
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
            if receipt["disposition"] == "confirmed" {
                break json!({"data":receipt});
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("AQ completes the original operation without another request");
    let _ = stop.send(true);
    sweep.await.unwrap();
    assert_eq!(
        body["data"]["receipt_refs"][0],
        json!({"type":"replacement_release","id":predecessor})
    );
    assert!(!holds().await, "B4 released the failed predecessor");
    let released: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_replacement_releases WHERE attempt_id=$1 AND task_id=$2",
    )
    .bind(predecessor)
    .bind(task)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(released, 1);
    let Assessment::Replace(next) =
        replacement::assess_task(&x.f.state, task, common::profiled::NOW)
            .await
            .unwrap()
    else {
        panic!("scheduler may start the replacement attempt");
    };
    assert_eq!(next.predecessor, predecessor);
    assert!(!next.needs_release);
    assert!(
        x.posts().is_empty(),
        "retry never writes Graph task lifecycle"
    );
    // Review M-3: once the task is held (here quarantined), a NEW retry is refused with
    // the hold, but a replay of the admitted operation keeps its recorded outcome.
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'synthetic stall','owner','{}',$3,1)")
        .bind(predecessor).bind(task).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    let (status, body) = x.act(&retry(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "stall_quarantined", "{body}");
    let (status, body) = x.act(&confirmed_retry).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
}

fn graph_create(project: Uuid, first: Uuid, second: Uuid, edge: Uuid) -> Value {
    action(
        "task.graph.create",
        "project",
        project,
        "1",
        json!({
            "operation_id":Uuid::new_v4(), "project_id":project, "expected_version":"1",
            "tasks":[
                {"id":first,"title":"First","body":"Implementation","priority":300,"role":"reviewer","intelligence_hint":"standard-high","task_type":"feature"},
                {"id":second,"title":"Second"}
            ],
            "edges":[{"id":edge,"source":first,"target":second}]
        }),
    )
}
fn edge(id: Uuid, workspace: Uuid, source: Uuid, target: Uuid) -> Value {
    json!({"id":id,"workspace_id":workspace,"source":source,"target":target,"edge_type":"blocks","description":"","version":1,
        "source_revision":null,"target_revision":null,"deleted":false,"created_at":"2026-01-01T00:00:00Z"})
}

#[tokio::test]
async fn owner_creates_graph_with_metadata_and_retained_identity() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), a, b),
        ]),
    );
    let request = graph_create(x.project(), a, b, e);
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    assert_eq!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 3);
    assert_eq!(posts[0]["operations"][0]["metadata"]["aq:priority"], 300);
    assert_eq!(posts[0]["operations"][0]["metadata"]["aq:role"], "reviewer");
    // A task that names no role stores none: the project's default applies.
    let unnamed = posts[0]["operations"][1]["metadata"].as_object().unwrap();
    assert!(!unnamed.contains_key("aq:role"), "{unnamed:?}");
    assert!(!posts[0].to_string().contains("profile_hint"));
    assert_eq!(
        posts[0]["operations"][0]["metadata"]["aq:intelligence_hint"],
        "standard-high"
    );
    assert_eq!(
        posts[0]["operations"][0]["metadata"]["aq:task_type"],
        "feature"
    );
    let (_, replay) = x.act(&request).await;
    assert_eq!(replay["data"], receipt["data"]);
    let mut changed = request.clone();
    changed["payload"]["tasks"][0]["priority"] = json!(1);
    assert_eq!(x.act(&changed).await.0, 409);
    assert_eq!(x.posts().len(), 1);
    let lookup = x
        .lookup(request["operation_id"].as_str().unwrap().parse().unwrap())
        .await;
    assert_eq!(lookup["data"], receipt["data"]);
    let pool = x.f.db.pool().await;
    for task in [a, b] {
        let author =
            aq_human_work::task_git_author::lookup(&pool, x.project(), x.workspace(), task)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(author.email, "owner@localhost");
        let response = aq_server::build_admin_router(x.f.state.clone())
            .oneshot(
                Request::get(format!("/api/human/projects/{}/tasks/{task}", x.project()))
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let reply: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(
            reply["data"]["requested_by"],
            json!({"name":"owner","email":"owner@localhost"})
        );
        assert_eq!(reply["data"]["created_by"], "owner@localhost");
    }
}

#[tokio::test]
async fn owner_single_create_accepts_enqueue_metadata() {
    let x = setup().await;
    let id = Uuid::new_v4();
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let request = action(
        "task.create",
        "project",
        x.project(),
        "1",
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Single","body":"","acceptance_criteria":"","labels":[],"task_state":"active",
            "priority":1,"role":"developer","intelligence_hint":"standard-high","task_type":"feature"
        }),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    assert_eq!(x.posts()[0]["operations"][0]["metadata"]["aq:priority"], 1);
    assert_eq!(
        x.posts()[0]["operations"][0]["metadata"]["aq:role"],
        "developer"
    );
}

fn single_create(x: &Fixture, id: Uuid, extra: Value) -> Value {
    let mut payload = json!({"operation_id":id,"expected_project_revision":"1","title":"Single","body":"",
        "acceptance_criteria":"","labels":[],"task_state":"active"});
    (payload.as_object_mut().unwrap()).extend(extra.as_object().unwrap().clone());
    action("task.create", "project", x.project(), "1", payload)
}
const UNKNOWN_TESTER: &str = "Role `tester` does not exist. The roles are: developer, reviewer.";

/// Spec 0014 §5.5: a filed task names its role; it must exist, and a disabled
/// role may be named.
#[tokio::test]
async fn a_filed_task_names_an_existing_role_and_profile_hint_is_gone() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    // Without a role nothing is stored.
    let plain = Uuid::new_v4();
    x.reply(200, change_set(vec![node(plain, x.workspace(), "task", 1)]));
    let (status, body) = x.act(&single_create(&x, plain, json!({}))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = x.posts()[0]["operations"][0]["metadata"].clone();
    assert!(metadata.get("aq:role").is_none(), "{metadata}");
    // An unknown role is refused with a typed error that lists the roles.
    let (status, body) = x
        .act(&single_create(&x, Uuid::new_v4(), json!({"role":"tester"})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_role", "{body}");
    assert_eq!(body["message"], UNKNOWN_TESTER);
    // The old way of naming a profile is an unknown field.
    let (status, body) = x
        .act(&single_create(
            &x,
            Uuid::new_v4(),
            json!({"profile_hint":"coding"}),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "invalid_request", "{body}");
    assert_eq!(x.posts().len(), 1, "a refused create never reaches Graph");
    // A disabled role may be named: the task is filed (and then held).
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", false).await;
    let held = Uuid::new_v4();
    x.reply(200, change_set(vec![node(held, x.workspace(), "task", 1)]));
    let (status, body) = x
        .act(&single_create(&x, held, json!({"role":"reviewer"})))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    assert_eq!(
        x.posts()[1]["operations"][0]["metadata"]["aq:role"],
        "reviewer"
    );
}

#[tokio::test]
async fn a_graph_create_names_a_role_per_task_and_refuses_an_unknown_one() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    common::save_role(&pool, x.f.actor, "developer", "tester", true).await;
    let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
    let graph = |roles: [Option<&str>; 3]| {
        let tasks: Vec<Value> = ids
            .iter()
            .zip(roles)
            .map(|(id, role)| match role {
                Some(role) => json!({"id":id,"title":"Task","role":role}),
                None => json!({"id":id,"title":"Task"}),
            })
            .collect();
        let payload = json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),
            "expected_version":"1","tasks":tasks});
        action("task.graph.create", "project", x.project(), "1", payload)
    };
    let (status, body) = x
        .act(&graph([Some("reviewer"), Some("designer"), None]))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_role", "{body}");
    assert_eq!(
        body["message"],
        "Role `designer` does not exist. The roles are: developer, reviewer, tester."
    );
    assert!(
        x.posts().is_empty(),
        "nothing of a refused graph is created"
    );
    let created = ids.iter().map(|id| node(*id, x.workspace(), "task", 1));
    x.reply(200, change_set(created.collect()));
    let (status, body) = x
        .act(&graph([Some("reviewer"), Some("tester"), None]))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operations = x.posts()[0]["operations"].clone();
    assert_eq!(operations[0]["metadata"]["aq:role"], "reviewer");
    assert_eq!(operations[1]["metadata"]["aq:role"], "tester");
    assert!(operations[2]["metadata"].get("aq:role").is_none());
}

fn role_set(x: &Fixture, task: Uuid, version: &str, role: Value) -> Value {
    route_action("task.role.set", task, version, x, json!({"role":role}))
}

/// The owner changes a task's role while no attempt of it has been granted.
#[tokio::test]
async fn task_role_set_sets_and_clears_the_role_and_is_refused_after_a_grant() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let state = |state: &'static str| {
        let pool = pool.clone();
        async move {
            let set = format!(
                "UPDATE execution_attempts SET state=state||'{state}'::jsonb WHERE task_id=$1"
            );
            sqlx::query(&set).bind(task).execute(&pool).await.unwrap();
            sqlx::query("DELETE FROM profiled_authority_hot WHERE attempt_id IN (SELECT attempt_id FROM execution_attempts WHERE task_id=$1)")
                .bind(task).execute(&pool).await.unwrap();
        }
    };
    // An attempt that was offered and not granted does not fix the role.
    state(r#"{"status":"offered","grant":null}"#).await;
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x.act(&role_set(&x, task, "5", json!("reviewer"))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    // One Graph revision, with every other metadata key kept: the task's
    // history shows it as a `role` change (aq-human-work).
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"][0]["op"], "update_node");
    assert_eq!(posts[0]["operations"][0]["expected_version"], 5);
    let metadata = &posts[0]["operations"][0]["patch"]["metadata"];
    assert_eq!(metadata["aq:role"], "reviewer");
    assert_eq!(metadata["example:weight"], 2);

    // An unknown role is refused; a stale version conflicts.
    let mut named = node(task, x.workspace(), "task", 6);
    named["metadata"] = metadata.clone();
    x.put(named);
    let (status, body) = x.act(&role_set(&x, task, "6", json!("tester"))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_role", "{body}");
    assert_eq!(body["message"], UNKNOWN_TESTER);
    let (_, body) = x.act(&role_set(&x, task, "5", json!("developer"))).await;
    assert_eq!(body["data"]["disposition"], "conflict", "{body}");
    assert_eq!(body["data"]["current_versions"], json!({"task":"6"}));
    assert_eq!(x.posts().len(), 1);

    // Clearing removes the key, so the project's default role applies.
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x.act(&role_set(&x, task, "6", Value::Null)).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let cleared = &x.posts()[1]["operations"][0]["patch"]["metadata"];
    assert!(cleared.get("aq:role").is_none(), "{cleared}");
    assert_eq!(cleared["example:weight"], 2);

    // Once an attempt has been granted the role is fixed.
    state(r#"{"status":"granted","grant":{}}"#).await;
    x.put(node(task, x.workspace(), "task", 7));
    for role in [json!("reviewer"), Value::Null] {
        let (status, body) = x.act(&role_set(&x, task, "7", role)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    }
    // A finished attempt was granted too.
    state(r#"{"status":"finished","grant":null}"#).await;
    let (_, body) = x.act(&role_set(&x, task, "7", json!("reviewer"))).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
    assert_eq!(x.posts().len(), 2, "a refused change never reaches Graph");
}

async fn supervisor_token(x: &Fixture) -> String {
    use aq_central_authority::{
        ArtifactRef, CentralPlan, Clock, Digest, Document, Event, HostLaunchRef, Run, Text,
        owner::*,
    };
    struct Verifier;
    #[async_trait::async_trait]
    impl EvidenceVerifier for Verifier {
        async fn authorize_plan(
            &self,
            _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
            _: &CentralPlan,
        ) -> OwnerResult<()> {
            Ok(())
        }
        async fn verify_event(
            &self,
            _: &mut sqlx::Transaction<'_, sqlx::Postgres>,
            _: &Run,
            _: &Event,
        ) -> OwnerResult<()> {
            Ok(())
        }
    }
    let examples: Value = serde_json::from_str(include_str!(
        "../../../aq-central-authority/artifacts/examples.json"
    ))
    .unwrap();
    let Document::Plan(plan) =
        aq_central_authority::decode(&serde_json::to_vec(&examples["plan"]).unwrap()).unwrap()
    else {
        panic!("plan")
    };
    let actor = plan.subject.actor_id();
    let pool = x.f.db.pool().await;
    // Managed credentials resolve scope through the reservation actor registry.
    // This synthetic session fixture must configure that actor just as the
    // production supervisor controller does before issuing its credential.
    use aq_server::agent_access::supervisor_conversation::reservations::{
        Configure, LocalPolicy, PolicyKind, ReservationOwner,
    };
    let user = x.f.state.store.find_user(x.f.actor).await.unwrap().unwrap();
    let conversation: Uuid =
        sqlx::query_scalar("SELECT conversation_id FROM global_supervisor_conversation")
            .fetch_one(&pool)
            .await
            .unwrap();
    ReservationOwner::new(&x.f.state)
        .configure_actor(
            &user,
            &serde_json::to_vec(&Configure {
                operation_id: Uuid::new_v4(),
                actor_id: actor,
                expected_actor_revision: 0,
                policy: LocalPolicy {
                    schema_version: 1,
                    kind: PolicyKind::GlobalSupervisorReservation,
                    actor_id: actor,
                    conversation_id: conversation,
                    enabled: true,
                },
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let sessions = SessionOwner::new(pool.clone(), Verifier);
    let clock = Clock {
        machine_boot_id: Uuid::from_u128(9),
        boot_ms: 1000,
    };
    sessions.prepare(plan.clone(), clock.clone()).await.unwrap();
    let launch = HostLaunchRef {
        binding: plan.binding().unwrap(),
        launch_id: Uuid::new_v4(),
        intent: ArtifactRef {
            id: Text::new("fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture"),
        },
    };
    sessions
        .start(&plan.binding().unwrap(), launch, 60_000, clock)
        .await
        .unwrap();
    aq_server::agent_access::supervisor_conversation::reply::issue_credential(
        &pool,
        actor,
        60_000,
        (x.f.state.clock)().timestamp_millis() as u64,
    )
    .await
    .unwrap()
}
async fn machine(
    x: &Fixture,
    token: &str,
    path: &str,
    request: Option<&Value>,
) -> (StatusCode, Value) {
    let res = aq_server::build_worker_router(x.f.state.clone())
        .oneshot(
            Request::builder()
                .method(if request.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    request.map(Value::to_string).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}

#[tokio::test]
async fn supervisor_creates_graph_with_dependencies_idempotently() {
    let x = setup().await;
    let token = supervisor_token(&x).await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), a, b),
        ]),
    );
    let request = graph_create(x.project(), a, b, e);
    let (status, receipt) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    assert_eq!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(x.posts().len(), 1);
    assert_eq!(x.posts()[0]["operations"].as_array().unwrap().len(), 3);
    assert_eq!(
        x.posts()[0]["operations"][0]["metadata"]["aq:priority"],
        300
    );
    let (_, replay) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(replay["data"], receipt["data"]);
    let path = format!(
        "/api/v1/operations/{}",
        request["operation_id"].as_str().unwrap()
    );
    let (status, lookup) = machine(&x, &token, &path, None).await;
    assert_eq!(status, 200, "{lookup}");
    assert_eq!(lookup["data"], receipt["data"]);
    let mut changed = request.clone();
    changed["payload"]["tasks"][0]["title"] = json!("Changed");
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&changed))
            .await
            .0,
        409
    );
    assert_eq!(x.act(&request).await.0, 404);
    assert_eq!(
        machine(&x, "invalid", "/api/v1/actions", Some(&request))
            .await
            .0,
        401
    );
    // Synthetic reduced grant: production credentials are otherwise immutable.
    sqlx::query("ALTER TABLE central_supervisor_credentials DISABLE TRIGGER central_supervisor_credential_guard").execute(&x.f.db.pool().await).await.unwrap();
    sqlx::query("UPDATE central_supervisor_credentials SET scopes=ARRAY['graph.read']")
        .execute(&x.f.db.pool().await)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE central_supervisor_credentials ENABLE TRIGGER central_supervisor_credential_guard").execute(&x.f.db.pool().await).await.unwrap();
    assert_eq!(machine(&x, &token, &path, None).await.0, 403);
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&request))
            .await
            .0,
        403
    );
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn graph_create_rejects_invalid_batches_before_any_effect() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let base = graph_create(x.project(), a, b, e);
    let mut cases = vec![];
    for priority in [0, 301] {
        let mut v = base.clone();
        v["payload"]["tasks"][0]["priority"] = json!(priority);
        cases.push(v);
    }
    let mut v = base.clone();
    v["payload"]["tasks"][1]["id"] = json!(a);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["edges"][0]["target"] = json!(a);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["tasks"] = json!([]);
    cases.push(v);
    let mut v = base.clone();
    v["payload"]["tasks"] = json!(
        (0..100)
            .map(|i| json!({"id":Uuid::new_v4(),"title":format!("{i}")}))
            .collect::<Vec<_>>()
    );
    cases.push(v);
    for request in cases {
        let (status, body) = x.act(&request).await;
        assert_eq!(status, 422, "{body}");
    }
    assert!(x.posts().is_empty());
    let mut stale = base;
    stale["payload"]["expected_version"] = json!("2");
    stale["subject"]["revision"] = json!("2");
    stale["expected_versions"]["project"] = json!("2");
    let (status, receipt) = x.act(&stale).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "conflict");
    assert!(x.posts().is_empty());
}

#[tokio::test]
async fn graph_create_accepts_existing_dependencies_and_refuses_other_projects() {
    let x = setup().await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let existing = Uuid::new_v4();
    let mut n = node(existing, x.workspace(), "task", 1);
    n["metadata"]["aq:project_id"] = json!(x.project());
    x.put(n.clone());
    let mut request = graph_create(x.project(), a, b, e);
    request["payload"]["edges"][0]["source"] = json!(existing);
    x.reply(
        200,
        change_set(vec![
            node(a, x.workspace(), "task", 1),
            node(b, x.workspace(), "task", 1),
            edge(e, x.workspace(), existing, b),
        ]),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    assert_eq!(x.posts()[0]["operations"][2]["source"], json!(existing));
    n["metadata"]["aq:project_id"] = json!(Uuid::new_v4());
    x.put(n);
    request["operation_id"] = json!(Uuid::new_v4());
    request["payload"]["operation_id"] = request["operation_id"].clone();
    assert_eq!(x.act(&request).await.0, 403);
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn supervisor_single_create_and_credential_denials() {
    let mut x = setup().await;
    let token = supervisor_token(&x).await;
    let id = Uuid::new_v4();
    let request = action(
        "task.create",
        "project",
        x.project(),
        "1",
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Scoped single","body":"","acceptance_criteria":"","labels":[],"task_state":"draft","priority":42
        }),
    );
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let (status, receipt) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    let metadata = x.posts()[0]["operations"][0]["metadata"].clone();
    assert_eq!(metadata["aq:priority"], 42);
    assert!(metadata.get("aq:human_actor_id").is_none());
    assert!(metadata["aq:supervisor_session_id"].is_string());
    let path = format!("/api/v1/operations/{id}");
    let pool = x.f.db.pool().await;
    let principal: (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT owner_id,supervisor_session_id FROM dashboard_operations WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(principal.0, None);
    assert_eq!(
        json!(principal.1.unwrap()),
        metadata["aq:supervisor_session_id"]
    );
    // An Owner account credential never authenticates on the machine route.
    assert_eq!(machine(&x, "owner@localhost", &path, None).await.0, 401);
    let mut edit = request.clone();
    edit["kind"] = json!("task.edit");
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&edit)).await.0,
        422
    );
    let now = (x.f.state.clock)().timestamp_millis();
    sqlx::query("UPDATE central_supervisor_credentials SET revoked_at_ms=$1")
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(machine(&x, &token, &path, None).await.0, 401);
    assert_eq!(
        machine(&x, &token, "/api/v1/actions", Some(&request))
            .await
            .0,
        401
    );
    // Issue a replacement credential, then advance the injected wall clock.
    let actor: Uuid =
        sqlx::query_scalar("SELECT actor_id FROM central_supervisor_credentials LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let expired = aq_server::agent_access::supervisor_conversation::reply::issue_credential(
        &pool, actor, 60_000, now as u64,
    )
    .await
    .unwrap();
    x.f.state.clock =
        Arc::new(move || chrono::DateTime::from_timestamp_millis(now + 60_000).unwrap());
    assert_eq!(machine(&x, &expired, &path, None).await.0, 401);
    assert_eq!(
        machine(&x, &expired, "/api/v1/actions", Some(&request))
            .await
            .0,
        401
    );
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn graph_create_counts_parent_edges_at_atomic_limit() {
    let x = setup().await;
    let parent = Uuid::new_v4();
    let mut tasks: Vec<_> = (0..99)
        .map(|i| json!({"id":Uuid::new_v4(),"title":format!("Task {i}")}))
        .collect();
    tasks[1]["parent_id"] = tasks[0]["id"].clone();
    let request = action(
        "task.graph.create",
        "project",
        x.project(),
        "1",
        json!({
            "operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","tasks":tasks
        }),
    );
    // 99 nodes + one parent edge fit; another parent would exceed the limit.
    let mut oversized = request.clone();
    oversized["payload"]["tasks"][2]["parent_id"] = json!(parent);
    assert_eq!(x.act(&oversized).await.0, 422);
    assert!(x.posts().is_empty());
    let mut results: Vec<_> = tasks
        .iter()
        .map(|t| {
            node(
                t["id"].as_str().unwrap().parse().unwrap(),
                x.workspace(),
                "task",
                1,
            )
        })
        .collect();
    results.push(edge(Uuid::new_v4(), x.workspace(), parent, parent));
    x.reply(200, change_set(results));
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 100);
    assert_eq!(posts[0]["operations"][99]["edge_type"], "parent-child");
    assert_eq!(posts[0]["operations"][99]["source"], tasks[0]["id"]);
    assert_eq!(posts[0]["operations"][99]["target"], tasks[1]["id"]);
}

#[tokio::test]
async fn supervisor_graph_lost_response_replays_original_batch() {
    let x = setup().await;
    let token = supervisor_token(&x).await;
    let (a, b, e) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let request = graph_create(x.project(), a, b, e);
    let op: Uuid = request["operation_id"].as_str().unwrap().parse().unwrap();
    let receipt = change_set(vec![
        node(a, x.workspace(), "task", 1),
        node(b, x.workspace(), "task", 1),
        edge(e, x.workspace(), a, b),
    ]);
    x.reply(200, receipt.clone());
    x.graph.timeout_after_commit.store(true, Ordering::SeqCst);
    let app = aq_server::build_worker_router(x.f.state.clone());
    let mut pending = tokio::spawn(
        app.oneshot(
            Request::post("/api/v1/actions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .unwrap(),
        ),
    );
    tokio::select! {
        _ = x.graph.committed.notified() => {},
        result = &mut pending => panic!("request ended before Graph committed: {result:?}"),
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(3)).await;
    tokio::time::resume();
    let response = pending.await.unwrap().unwrap();
    assert_eq!(response.status(), 202);
    let unknown: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(unknown["data"]["disposition"], "unknown");
    let (status, recovered) = machine(&x, &token, &format!("/api/v1/operations/{op}"), None).await;
    assert_eq!(status, 200, "{recovered}");
    assert_eq!(recovered["data"]["disposition"], "confirmed");
    assert_eq!(
        recovered["data"]["receipt_refs"][0]["id"],
        receipt["change_set_id"]
    );
    let (_, replay) = machine(&x, &token, "/api/v1/actions", Some(&request)).await;
    assert_eq!(replay["data"], recovered["data"]);
    let bodies = x.graph.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(
        x.graph.receipts.lock().unwrap().len(),
        1,
        "one atomic commit"
    );
}

#[tokio::test]
async fn graph_create_retains_terminal_conflict_without_resend() {
    let x = setup().await;
    let request = graph_create(x.project(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    x.reply(409, json!({"error":{"code":"conflict","message":"cycle"}}));
    let (_, receipt) = x.act(&request).await;
    assert_eq!(receipt["data"]["disposition"], "conflict");
    assert!(
        receipt["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, replay) = x.act(&request).await;
    assert_eq!(replay["data"], receipt["data"]);
    let op = request["operation_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(x.lookup(op).await["data"], receipt["data"]);
    assert_eq!(x.posts().len(), 1);
}

#[tokio::test]
async fn owner_single_refuses_cross_project_parent_before_dispatch() {
    let x = setup().await;
    let parent = Uuid::new_v4();
    let id = Uuid::new_v4();
    let mut n = node(parent, x.workspace(), "task", 1);
    n["metadata"]["aq:project_id"] = json!(Uuid::new_v4());
    x.put(n);
    x.reply(200, change_set(vec![node(id, x.workspace(), "task", 1)]));
    let request = action(
        "task.create",
        "project",
        x.project(),
        "1",
        json!({
            "operation_id":id,"expected_project_revision":"1","title":"Scoped child","body":"","acceptance_criteria":"","labels":[],"task_state":"active","parent_id":parent
        }),
    );
    let (status, body) = x.act(&request).await;
    assert_eq!(status, 403, "{body}");
    assert!(
        x.posts().is_empty(),
        "another project's parent is never dispatched"
    );
}

#[tokio::test]
async fn enqueue_principal_migration_preserves_retired_tombstones() {
    use aq_server::test_support::{TestDb, migrate_profile};
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 56).await.unwrap();
    let principal = Uuid::new_v4();
    let operation = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'migration@example.test','owner',now())")
        .bind(principal)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at,retired_at) VALUES($1,$2,'task.create','{\"kind\":\"project\"}', $3,'confirmed',NULL,'1970-01-01','1970-01-01','1971-01-01')")
        .bind(operation).bind(principal).bind(format!("sha256:{}", "0".repeat(64))).execute(&pool).await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(d) FROM dashboard_operations d WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    migrate_profile(&pool, aq_server::STATE_FORMAT_VERSION)
        .await
        .expect("retired operation receipts must upgrade");
    let mut after: Value =
        sqlx::query_scalar("SELECT to_jsonb(d) FROM dashboard_operations d WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after["owner_id"], json!(principal));
    assert!(after["supervisor_session_id"].is_null());
    after.as_object_mut().unwrap().remove("owner_id");
    after
        .as_object_mut()
        .unwrap()
        .remove("supervisor_session_id");
    assert_eq!(
        before, after,
        "immutable tombstone identity must survive exactly"
    );
    assert!(
        sqlx::query("UPDATE dashboard_operations SET scope='{}' WHERE operation_id=$1")
            .bind(operation)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE dashboard_operations SET owner_id=NULL WHERE operation_id=$1")
            .bind(operation)
            .execute(&pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retiring_a_retired_record_is_rejected_without_a_graph_write() {
    // QA N-03: a second retirement used to append a revision and overwrite the reason.
    let x = setup().await;
    let id = Uuid::new_v4();
    let mut retired = node(id, x.workspace(), "knowledge", 4);
    retired["retired"] = json!(true);
    retired["retirement_reason"] = json!("Superseded");
    x.put(retired);
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "knowledge.retire",
            "knowledge",
            id,
            "4",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"4","retirement_reason":"Again"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    assert!(x.posts().is_empty(), "nothing reaches Graph");
}

#[tokio::test]
async fn human_reads_attribute_service_writes_to_the_acting_owner() {
    // QA N-04: Graph records the one AQ mutation identity as creator and verifier.
    let x = setup().await;
    let op = Uuid::new_v4();
    x.reply(
        200,
        change_set(vec![node(op, x.workspace(), "knowledge", 1)]),
    );
    let (_, body) = x
        .act(&action(
            "knowledge.author",
            "project",
            x.project(),
            "1",
            json!({"operation_id":op,"project_id":x.project(),"expected_project_revision":"1","title":"Original title","body":"Original body"}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut verified = x.graph.nodes.lock().unwrap()[&op].clone();
    verified["version"] = json!(2);
    verified["content_revision"] = json!(2);
    verified["verification"] = json!("verified");
    verified["last_verified_by"] = json!("dashboard-fixture-service");
    verified["last_verified_at"] = json!("2026-01-02T00:00:00Z");
    x.reply(200, change_set(vec![verified]));
    let mut verify = action(
        "knowledge.verify",
        "knowledge",
        op,
        "1",
        json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","verification":"verified",
            "evidence":[{"source_id":"review-1","kind":"url","url":"https://example.test/review","observed_at":"2026-01-01T00:00:00Z","retained":false}]}),
    );
    verify["reason"] = json!("Reviewed");
    let (_, body) = x.act(&verify).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let other = Uuid::new_v4();
    x.put(node(other, x.workspace(), "knowledge", 1));
    let read = |id: Uuid| {
        let request = Request::get(format!(
            "/api/human/projects/{}/knowledge/{id}",
            x.project()
        ))
        .header("x-aq-dev-email", "owner@localhost")
        .body(Body::empty())
        .unwrap();
        let app = aq_server::build_admin_router(x.f.state.clone());
        async move {
            let res = app.oneshot(request).await.unwrap();
            assert_eq!(res.status(), StatusCode::OK);
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()["data"].clone()
        }
    };
    let record = read(op).await;
    assert_eq!(record["created_by"], "owner@localhost", "{record}");
    assert_eq!(record["last_verified_by"], "owner@localhost", "{record}");
    let foreign = read(other).await;
    assert_eq!(
        foreign["created_by"], "fixture",
        "other actors keep their attribution"
    );
}

#[tokio::test]
async fn task_history_attributes_dashboard_writes_to_the_acting_owner() {
    // QA P1-07: history named the service identity for the owner's reprioritize.
    let x = setup().await;
    let task = x.f.selection.lineage.task_id;
    x.put(node(task, x.workspace(), "task", 5));
    let mut updated = node(task, x.workspace(), "task", 6);
    updated["metadata"]["aq:priority"] = json!(7);
    x.reply(200, change_set(vec![updated.clone()]));
    let op = Uuid::new_v4();
    let (status, body) = x
        .act(&action(
            "task.reprioritize",
            "task",
            task,
            "5",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"5","priority":7}),
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let set = body["data"]["receipt_refs"][0]["id"].clone();
    let mut before = node(task, x.workspace(), "task", 5);
    before["content_revision"] = json!(5);
    let entry = |n: u64, actor: &str, change_set: Value, data: Value| {
        json!({"sequence":n,"entry_id":Uuid::new_v4(),"category":"revision","actor_id":actor,
            "operation":"updated","reason":format!("AQ dashboard action task.reprioritize {op}"),
            "change_set_id":change_set,"data":data,"created_at":chrono::Utc::now().to_rfc3339()})
    };
    *x.graph.history.lock().unwrap() = Some(json!({"items":[
        entry(1, "dashboard-fixture-service", json!(Uuid::new_v4()), before),
        entry(2, "dashboard-fixture-service", set, updated),
    ],"next_cursor":null}));
    let res = aq_server::build_admin_router(x.f.state.clone())
        .oneshot(
            Request::get(format!(
                "/api/human/projects/{}/tasks/{task}/history",
                x.project()
            ))
            .header("x-aq-dev-email", "owner@localhost")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let page: Value = serde_json::from_slice(&bytes).unwrap();
    let items = &page["data"]["items"];
    assert_eq!(items[1]["actor_id"], "owner@localhost", "{page}");
    assert_eq!(
        items[0]["actor_id"], "dashboard-fixture-service",
        "a change set no owner operation confirmed keeps Graph's actor"
    );
    assert_eq!(
        items[1]["change"]["fields"],
        json!([{"field":"priority","from":null,"to":"7"}])
    );
}

#[tokio::test]
async fn owner_activates_a_draft_task_and_only_a_draft() {
    let x = setup().await;
    let request = |id: Uuid| {
        let op = Uuid::new_v4();
        action(
            "task.activate",
            "task",
            id,
            "3",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        )
    };
    // A draft becomes active with one Graph write at the expected version.
    let id = Uuid::new_v4();
    let mut before = node(id, x.workspace(), "task", 3);
    before["task_state"] = json!("draft");
    x.put(before.clone());
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("active");
    x.reply(200, change_set(vec![after]));
    let activate = request(id);
    let (status, body) = x.act(&activate).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let native = x.posts().last().unwrap()["operations"][0].clone();
    assert_eq!(
        (
            native["op"].clone(),
            native["expected_version"].clone(),
            native["patch"].clone()
        ),
        (
            json!("update_node"),
            json!(3),
            json!({"task_state":"active"})
        )
    );
    // Replaying the operation returns its result and writes nothing more.
    let posts = x.posts().len();
    let (status, replay) = x.act(&activate).await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert_eq!(replay["data"]["disposition"], "confirmed", "{replay}");
    assert_eq!(x.posts().len(), posts);
    // Any other state is rejected before Graph is asked to change anything:
    // reopening a finished task is not activation.
    for state in ["active", "completed", "canceled"] {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        x.put(task);
        let posts = x.posts().len();
        let (status, body) = x.act(&request(id)).await;
        assert_eq!(
            body["data"]["disposition"], "rejected",
            "{state}: {status} {body}"
        );
        assert_eq!(x.posts().len(), posts, "{state}");
    }
}

#[tokio::test]
async fn owner_reopens_a_finished_task_only_if_it_never_ran() {
    let x = setup().await;
    let request = |id: Uuid| {
        let op = Uuid::new_v4();
        action(
            "task.reopen",
            "task",
            id,
            "3",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        )
    };
    for state in ["completed", "canceled"] {
        let id = Uuid::new_v4();
        let mut before = node(id, x.workspace(), "task", 3);
        before["task_state"] = json!(state);
        x.put(before.clone());
        let mut after = before;
        after["version"] = json!(4);
        after["task_state"] = json!("active");
        x.reply(200, change_set(vec![after]));
        let (status, body) = x.act(&request(id)).await;
        assert_eq!(status, StatusCode::OK, "{state}: {body}");
        assert_eq!(body["data"]["disposition"], "confirmed", "{state}: {body}");
        let native = x.posts().last().unwrap()["operations"][0].clone();
        assert_eq!(native["patch"], json!({"task_state":"active"}), "{state}");
    }
    // A task that is not finished is not reopened.
    for state in ["draft", "active"] {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        x.put(task);
        let posts = x.posts().len();
        let (_, body) = x.act(&request(id)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{state}: {body}");
        assert_eq!(x.posts().len(), posts, "{state}");
    }
    // The fixture's task has an attempt: finished or not, it is not reopened.
    let ran = x.f.selection.lineage.task_id;
    let mut task = node(ran, x.workspace(), "task", 3);
    task["task_state"] = json!("completed");
    x.put(task);
    let posts = x.posts().len();
    let (_, body) = x.act(&request(ran)).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
    assert_eq!(x.posts().len(), posts);
}

#[tokio::test]
async fn owner_completes_a_task_by_hand_only_when_nothing_else_owns_its_outcome() {
    let x = setup().await;
    let request = |id: Uuid, reason: Option<&str>| {
        let op = Uuid::new_v4();
        let mut request = action(
            "task.complete",
            "task",
            id,
            "3",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        );
        if let Some(reason) = reason {
            request["reason"] = json!(reason);
        }
        request
    };
    let put = |state: &str, metadata: Value| {
        let id = Uuid::new_v4();
        let mut task = node(id, x.workspace(), "task", 3);
        task["task_state"] = json!(state);
        task["metadata"] = metadata;
        x.put(task.clone());
        (id, task)
    };
    // An active task nothing holds is completed with one Graph write that
    // carries the owner's reason.
    let (id, before) = put("active", json!({}));
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("completed");
    x.reply(200, change_set(vec![after]));
    let (status, body) = x.act(&request(id, Some("Done by hand"))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let posted = x.posts().last().unwrap().clone();
    assert_eq!(posted["reason"], "Done by hand");
    assert_eq!(
        posted["operations"][0]["patch"],
        json!({"task_state":"completed"})
    );
    // Without a reason nothing is admitted.
    let (id, _) = put("active", json!({}));
    let posts = x.posts().len();
    let (status, body) = x.act(&request(id, None)).await;
    assert!(status.is_client_error(), "{status} {body}");
    let (status, body) = x.act(&request(id, Some("  "))).await;
    assert!(status.is_client_error(), "{status} {body}");
    assert_eq!(x.posts().len(), posts);
    // Rejected, with nothing written: not active; managed by a playbook;
    // held by an attempt (the fixture's task has one).
    let (draft, _) = put("draft", json!({}));
    let (done, _) = put("completed", json!({}));
    let (managed, _) = put("active", json!({"aq:playbook_namespace":"ns"}));
    let held = x.f.selection.lineage.task_id;
    x.put(node(held, x.workspace(), "task", 3));
    for (what, id) in [
        ("draft", draft),
        ("completed", done),
        ("playbook", managed),
        ("held", held),
    ] {
        let posts = x.posts().len();
        let (_, body) = x.act(&request(id, Some("Done by hand"))).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{what}: {body}");
        assert_eq!(x.posts().len(), posts, "{what}");
    }
    // Graph refuses a parent with an open child as a conflict at the same
    // version: that is a rejection, not a stale version to retry.
    let (parent, _) = put("active", json!({}));
    x.reply(
        409,
        json!({"error":{"code":"conflict","message":"open child prevents parent completion"}}),
    );
    let (_, body) = x.act(&request(parent, Some("Done by hand"))).await;
    assert_eq!(body["data"]["disposition"], "rejected", "{body}");
}

#[tokio::test]
async fn owner_cancels_tasks_without_reason_or_attempt_and_deletes_without_gate() {
    let x = setup().await;
    for (state, kind) in [
        ("draft", "task.cancel"),
        ("active", "task.cancel"),
        ("completed", "task.delete"),
        ("canceled", "task.delete"),
    ] {
        let id = Uuid::new_v4();
        let mut before = node(id, x.workspace(), "task", 3);
        before["task_state"] = json!(state);
        if kind == "task.delete" {
            before["retired"] = json!(true);
            before["retirement_reason"] = json!("Owner retired task");
        }
        x.put(before.clone());
        let mut after = before;
        after["version"] = json!(4);
        if kind == "task.delete" {
            after["deleted"] = json!(true);
        } else {
            after["task_state"] = json!("canceled");
        }
        x.reply(200, change_set(vec![after]));
        let op = Uuid::new_v4();
        let request = action(
            kind,
            "task",
            id,
            "3",
            json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
        );
        let (status, body) = x.act(&request).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{state} {kind} {body}; native posts: {:?}",
            x.posts()
        );
        assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
        let native = x.posts().last().unwrap()["operations"][0].clone();
        assert_eq!(
            native["op"],
            if kind == "task.delete" {
                "delete_node"
            } else {
                "update_node"
            }
        );
        if kind == "task.cancel" {
            assert_eq!(native["patch"]["task_state"], "canceled");
        }
        let (status, replay) = x.act(&request).await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["data"], body["data"]);
    }
}

#[tokio::test]
async fn canceled_task_cleanup_retries_after_restart_and_releases_only_after_actual_stop() {
    use aq_server::profiled_execution::{started, stopped};
    use common::native_started;
    let (f, start) = native_started::setup(true).await;
    let auth = native_started::context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let x = setup_runtime(f).await;
    let task = x.f.selection.lineage.task_id;
    let before = node(task, x.workspace(), "task", 3);
    x.put(before.clone());
    let mut after = before;
    after["version"] = json!(4);
    after["task_state"] = json!("canceled");
    x.reply(200, change_set(vec![after]));
    let op = Uuid::new_v4();
    let request = action(
        "task.cancel",
        "task",
        task,
        "3",
        json!({"operation_id":op,"project_id":x.project(),"expected_version":"3"}),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed");
    let mut sweep = aq_server::agent_access::human::task_termination::Sweep::default();
    sweep.tick(&x.f.state).await.unwrap();
    let pool = x.f.db.pool().await;
    let (held,cancels):(bool,i64) = sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_cancellations WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(held, "a cancel request is not a stop proof");
    assert_eq!(
        cancels, 1,
        "background cleanup must request the native stop"
    );
    let mut stop = native_started::stop_request(&start);
    stop.evidence.reason = aq_admission::transport::StopReason::Cancellation;
    stopped::record(&x.f.state, &auth, &stop).await.unwrap();
    // A new cursor models a server restart; durable owner intent is sufficient.
    let mut restarted = aq_server::agent_access::human::task_termination::Sweep::default();
    restarted.tick(&x.f.state).await.unwrap();
    let (held,dispositions):(bool,i64) = sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_stopped_dispositions WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(!held, "verified stopped cancellation must not stay held");
    assert_eq!(dispositions, 1);
    restarted.tick(&x.f.state).await.unwrap();
}

/// Spec 0017 §5.4: `task.create` and `task.graph.create` take `needs`, stored
/// as `aq:needs`; an empty list stores nothing and a malformed one is refused.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn filed_tasks_name_their_needs() {
    let x = setup().await;
    let single = Uuid::new_v4();
    x.reply(
        200,
        change_set(vec![node(single, x.workspace(), "task", 1)]),
    );
    let named = json!({"needs":["node","playwright"]});
    let (status, body) = x.act(&single_create(&x, single, named)).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = &x.posts()[0]["operations"][0]["metadata"];
    assert_eq!(metadata["aq:needs"], json!(["node", "playwright"]));
    for bad in [json!(["node", "node"]), json!(["c++"]), json!([""])] {
        let (status, body) = x
            .act(&single_create(&x, Uuid::new_v4(), json!({"needs":bad})))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
    }
    let ids: Vec<Uuid> = (0..2).map(|_| Uuid::new_v4()).collect();
    let tasks = json!([{"id":ids[0],"title":"Task","needs":["rust"]},{"id":ids[1],"title":"Task","needs":[]}]);
    let payload = json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"1","tasks":tasks});
    let created = ids.iter().map(|id| node(*id, x.workspace(), "task", 1));
    x.reply(200, change_set(created.collect()));
    let (status, body) = x
        .act(&action(
            "task.graph.create",
            "project",
            x.project(),
            "1",
            payload,
        ))
        .await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let operations = x.posts()[1]["operations"].clone();
    assert_eq!(operations[0]["metadata"]["aq:needs"], json!(["rust"]));
    assert!(operations[1]["metadata"].get("aq:needs").is_none());
    assert_eq!(x.posts().len(), 2, "a refused create never reaches Graph");
}

/// Spec 0017 §5.4: the owner sets or clears a task's needs until an attempt
/// of it has been granted.
#[cfg(feature = "project_environments")]
#[tokio::test]
async fn task_needs_set_sets_and_clears_needs_and_is_refused_after_a_grant() {
    let x = setup().await;
    let pool = x.f.db.pool().await;
    let task = x.f.selection.lineage.task_id;
    let set = |version: &str, needs: Value| {
        route_action("task.needs.set", task, version, &x, json!({"needs":needs}))
    };
    x.put(node(task, x.workspace(), "task", 5));
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 6)]));
    let (status, body) = x.act(&set("5", json!(["node"]))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let metadata = x.posts()[0]["operations"][0]["patch"]["metadata"].clone();
    assert_eq!(metadata["aq:needs"], json!(["node"]));
    assert_eq!(metadata["example:weight"], 2);
    let (status, body) = x.act(&set("5", json!(["node", "node"]))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // An empty list clears the key.
    let mut named = node(task, x.workspace(), "task", 6);
    named["metadata"] = metadata;
    x.put(named);
    x.reply(200, change_set(vec![node(task, x.workspace(), "task", 7)]));
    let (status, body) = x.act(&set("6", json!([]))).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{status} {body}");
    let cleared = &x.posts()[1]["operations"][0]["patch"]["metadata"];
    assert!(cleared.get("aq:needs").is_none(), "{cleared}");
    assert_eq!(cleared["example:weight"], 2);

    // Once an attempt has been granted the needs are fixed.
    let granted = r#"UPDATE execution_attempts SET state=state||'{"status":"granted","grant":{}}'::jsonb WHERE task_id=$1"#;
    sqlx::query(granted)
        .bind(task)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM profiled_authority_hot WHERE attempt_id IN (SELECT attempt_id FROM execution_attempts WHERE task_id=$1)")
        .bind(task).execute(&pool).await.unwrap();
    x.put(node(task, x.workspace(), "task", 7));
    for needs in [json!(["rust"]), Value::Null] {
        let (status, body) = x.act(&set("7", needs)).await;
        assert_eq!(body["data"]["disposition"], "rejected", "{status} {body}");
    }
    assert_eq!(x.posts().len(), 2, "a refused change never reaches Graph");
}

#[tokio::test]
async fn subtree_activation_activates_four_and_skips_active_branch() {
    let x = setup().await;
    let fixture = activation_fixture(&x);
    let (_, body) = x.act(&fixture.request).await;
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    assert_eq!(
        body["data"]["activation"],
        json!({"activated":4,"skipped":1})
    );
    assert_eq!(
        body["data"]["affected_entities"].as_array().unwrap().len(),
        4
    );
    for draft in fixture.drafts {
        let live = &x.graph.nodes.lock().unwrap()[&draft];
        assert_eq!(live["task_state"], "active");
        assert_eq!(live["version"], 6);
    }
    let (_, replay) = x.act(&fixture.request).await;
    assert_eq!(replay["data"], body["data"]);
    let nodes = x.graph.nodes.lock().unwrap();
    assert_eq!(nodes[&fixture.root]["task_state"], "draft");
    assert_eq!(nodes[&fixture.root]["version"], 3);
    assert_eq!(nodes[&fixture.branch]["task_state"], "active");
    assert_eq!(nodes[&fixture.branch]["version"], 2);
    let posts = x.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["operations"].as_array().unwrap().len(), 6);
}

#[tokio::test]
async fn subtree_activation_conflict_leaves_every_draft_unchanged() {
    let x = setup().await;
    let fixture = activation_fixture(&x);
    // Changing a nested non-draft parent's links bumps its version before
    // Graph accepts the batch. All the planned draft changes must roll back.
    x.graph
        .nodes
        .lock()
        .unwrap()
        .get_mut(&fixture.branch)
        .unwrap()["version"] = json!(3);
    let (_, body) = x.act(&fixture.request).await;
    assert_eq!(body["data"]["disposition"], "conflict", "{body}");
    assert_eq!(x.posts().len(), 1);
    for draft in fixture.drafts {
        let live = &x.graph.nodes.lock().unwrap()[&draft];
        assert_eq!(live["task_state"], "draft");
        assert_eq!(live["version"], 5);
    }
}

struct ActivationFixture {
    root: Uuid,
    branch: Uuid,
    drafts: [Uuid; 4],
    request: Value,
}

fn activation_fixture(x: &Fixture) -> ActivationFixture {
    let root = Uuid::new_v4();
    let branch = Uuid::new_v4();
    let drafts = std::array::from_fn(|_| Uuid::new_v4());
    let task = |id, state, version| {
        let mut value = node(id, x.workspace(), "task", version);
        value["task_state"] = json!(state);
        x.put(value.clone());
        value
    };
    let tree = |task: Value, children: Vec<Value>| json!({"node":task,"children":children,"truncated":false});
    let draft = |id| tree(task(id, "draft", 5), vec![]);
    *x.graph.tree.lock().unwrap() = Some(tree(
        task(root, "draft", 3),
        vec![
            tree(task(branch, "active", 2), vec![draft(drafts[0])]),
            draft(drafts[1]),
            draft(drafts[2]),
            draft(drafts[3]),
        ],
    ));
    ActivationFixture {
        root,
        branch,
        drafts,
        request: action(
            "task.activate.subtree",
            "task",
            root,
            "3",
            json!({"operation_id":Uuid::new_v4(),"project_id":x.project(),"expected_version":"3"}),
        ),
    }
}
