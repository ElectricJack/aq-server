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
struct Graph {
    nodes: Arc<Mutex<HashMap<Uuid, Value>>>,
    posts: Arc<Mutex<Vec<Value>>>,
    reply: Arc<Mutex<Option<(u16, Value)>>>,
    bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    receipts: Receipts,
    timeout_after_commit: Arc<AtomicBool>,
    committed: Arc<tokio::sync::Notify>,
    history: Arc<Mutex<Option<Value>>>,
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
    let (status, mut reply) = g.reply.lock().unwrap().clone().expect("configured reply");
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
    let graph = Graph::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/workspaces/{w}/nodes/{id}", get(read_node))
        .route("/v1/workspaces/{w}/nodes/{id}/history", get(read_history))
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
    f.state.agent_access = Some(Arc::new(runtime));
    Fixture {
        f,
        graph,
        server,
        _dir: dir,
    }
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
fn node(id: Uuid, workspace: Uuid, kind: &str, version: u64) -> Value {
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
fn action(kind: &str, entity: &str, id: Uuid, revision: &str, payload: Value) -> Value {
    json!({"operation_id":payload["operation_id"],"kind":kind,
        "subject":{"type":entity,"id":id,"revision":revision},
        "expected_versions":{entity:revision},"payload":payload})
}
fn change_set(results: Vec<Value>) -> Value {
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

    let (status, body) = x.act(&retry(Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
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
                {"id":first,"title":"First","body":"Implementation","priority":300,"profile_hint":"coding","intelligence_hint":"standard-high","task_type":"feature"},
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
    assert_eq!(
        posts[0]["operations"][0]["metadata"]["aq:profile_hint"],
        "coding"
    );
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
            "priority":1,"profile_hint":"coding","intelligence_hint":"standard-high","task_type":"feature"
        }),
    );
    let (status, receipt) = x.act(&request).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["data"]["disposition"], "confirmed", "{receipt}");
    assert_eq!(x.posts()[0]["operations"][0]["metadata"]["aq:priority"], 1);
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
