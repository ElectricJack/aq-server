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

async fn read_revision(
    State(g): State<Graph>,
    Path((_, _, n)): Path<(Uuid, Uuid, u64)>,
) -> Response {
    found(g.revisions.lock().unwrap().get(&n))
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
        .route(
            "/v1/workspaces/{w}/nodes/{id}/revisions/{n}",
            get(read_revision),
        )
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

/// Spec 0014 §5.5: every task read shows the effective role and where it
/// comes from, and the work read holds a task whose role is disabled or gone.
#[tokio::test]
async fn task_reads_show_the_effective_role_and_hold_a_disabled_or_missing_one() {
    let x = setup().await;
    let (w, pool) = (x.w(), x.f.db.pool().await);
    let role = |id: &str, source: &str| json!({"id":id,"source":source});
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
    let nodes = [
        (plain, task(plain, w, "Plain", "active")),
        (review, named(review, "Review", "reviewer")),
        (gone, named(gone, "Gone", "tester")),
        (draft, drafted),
    ];
    x.graph.nodes.lock().unwrap().extend(nodes.clone());
    for (id, node) in &nodes {
        let others = nodes.iter().filter(|(other, _)| other != id);
        let others: Vec<Value> = others.map(|(_, n)| n.clone()).collect();
        let c = context(node, others, vec![], false);
        x.graph.contexts.lock().unwrap().insert(*id, c);
    }
    let expected = [
        (plain, role("developer", "project_default")),
        (review, role("reviewer", "task")),
        (gone, role("tester", "task")),
        (draft, role("reviewer", "task")),
    ];
    // Task detail, the task page, the graph and the work read agree.
    let page = x.get("/tasks?limit=10").await;
    let graph = x.get(&format!("/tasks/{plain}/graph")).await;
    for (id, role) in &expected {
        assert_eq!(&x.get(&format!("/tasks/{id}")).await["role"], role);
        for listed in [&page["items"], &graph["nodes"]] {
            let found = listed.as_array().unwrap().iter();
            let found = found.clone().find(|n| n["id"] == json!(id)).unwrap();
            assert_eq!(&found["role"], role, "{found}");
        }
        let work = x.get(&format!("/tasks/{id}/work")).await;
        assert_eq!(&work["role"], role);
        assert_eq!(work["readiness"]["reasons"], json!([]), "{work}");
    }
    assert_eq!(
        x.get(&format!("/tasks/{review}/work")).await["readiness"]["state"],
        "ready"
    );

    // A disabled role and a role that no longer exists hold their tasks.
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", false).await;
    common::delete_role(&pool, x.f.actor, "tester").await;
    let held = x.get(&format!("/tasks/{review}/work")).await;
    assert_eq!(held["readiness"]["state"], "held", "{held}");
    let reason = &held["readiness"]["reasons"][0];
    assert_eq!(reason["code"], "role_disabled");
    assert_eq!(reason["role"], "reviewer");
    assert_eq!(
        reason["message"],
        "AQ does not offer this task: its role `reviewer` is disabled. Enable the role in Settings or change the task's role."
    );
    let missing = x.get(&format!("/tasks/{gone}/work")).await;
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
    // A task on the default role is not affected, and a draft is not held.
    let free = x.get(&format!("/tasks/{plain}/work")).await;
    assert_eq!(free["readiness"]["state"], "ready");
    assert_eq!(free["readiness"]["reasons"], json!([]));
    let idle = x.get(&format!("/tasks/{draft}/work")).await;
    assert_eq!(idle["readiness"]["state"], "draft");
    assert_eq!(idle["readiness"]["reasons"], json!([]));
    // Enabling the role again releases the task.
    common::save_role(&pool, x.f.actor, "reviewer", "reviewer", true).await;
    let released = x.get(&format!("/tasks/{review}/work")).await;
    assert_eq!(released["readiness"]["state"], "ready");
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

#[tokio::test]
async fn work_reports_a_stopped_holding_attempt_as_held_with_its_reason() {
    // QA P1-01: the MS1 cancel drill's attempt was cancelled and stopped, but
    // keeps the task claim; readiness said `running`.
    use aq_server::execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::{Status, Stop},
    };
    let x = setup().await;
    let w = x.w();
    let held = x.held();
    let attempt = x.f.profiled.offer.binding.attempt_id;
    x.graph.contexts.lock().unwrap().insert(
        held,
        context(&task(held, w, "Held", "active"), vec![], vec![], false),
    );
    let running = x.get(&format!("/tasks/{held}/work")).await;
    assert_eq!(running["readiness"]["state"], "running");
    assert_eq!(running["readiness"]["reasons"], json!([]));

    let pool = x.f.db.pool().await;
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

    let held_work = x.get(&format!("/tasks/{held}/work")).await;
    let readiness = &held_work["readiness"];
    assert_eq!(readiness["state"], "held", "{held_work}");
    assert_eq!(
        readiness["reasons"][0]["code"],
        "cancelled_attempt_holds_task"
    );
    assert_eq!(readiness["reasons"][0]["attempt_id"], json!(attempt));
    assert_eq!(held_work["execution"]["status"], "finished");
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
    let entry = |n: u64, category: &str, operation: &str, data: Value| {
        json!({"sequence":n,"entry_id":Uuid::new_v4(),"category":category,"actor_id":"fixture",
            "operation":operation,"reason":null,"change_set_id":Uuid::new_v4(),
            "data":data,"created_at":"2026-01-01T00:00:00Z"})
    };
    *x.graph.history.lock().unwrap() = Some(json!({"items":[
        entry(5, "revision", "updated", snapshot(2, 5, "Held")),
        entry(6, "lifecycle", "transitioned", json!({"from":"active","to":"completed","version":3})),
        entry(7, "edge", "added", edge(w, blocker, held, "blocks")),
        entry(8, "lifecycle", "set_task_state", json!({"secret":"must-not-escape"})),
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

/// Stores the fixture's catalog and binds its project to it, with its intelligence profile an
/// automatic router or not.
async fn bind_catalog(x: &Fixture, automatic: bool) {
    let pool = x.f.db.pool().await;
    let binding: Value =
        sqlx::query_scalar("SELECT binding FROM project_execution_bindings WHERE project_id=$1")
            .bind(x.f.selection.lineage.project_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    // The fixture keeps its catalog in a file, not in the artifact table.
    let raw: Value = serde_json::from_str(include_str!("../fixtures/selection-v2.json")).unwrap();
    let original = raw["catalog_artifact_json"]
        .as_str()
        .unwrap()
        .as_bytes()
        .to_vec();
    let mut artifact = aq_profiles::ValidatedCatalog::from_artifact(&original)
        .unwrap()
        .artifact()
        .clone();
    for profile in &mut artifact.catalog.intelligence {
        profile.automatic_routing = automatic.then_some(true);
    }
    let updated = aq_profiles::validate(artifact.catalog, artifact.instructions).unwrap();
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) SELECT $1,$2,id,0 FROM users LIMIT 1")
        .bind(updated.digest())
        .bind(updated.canonical_bytes().unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let mut binding = binding;
    binding["catalog_digest"] = json!(updated.digest());
    sqlx::query("UPDATE project_execution_bindings SET binding=$2 WHERE project_id=$1")
        .bind(x.f.selection.lineage.project_id)
        .bind(binding)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn work_read_reports_waiting_then_routed() {
    let x = setup().await;
    let (w, id) = (x.w(), Uuid::new_v4());
    let node = task(id, w, "Unrouted", "active");
    x.graph
        .contexts
        .lock()
        .unwrap()
        .insert(id, context(&node, vec![], vec![], false));
    let path = format!("/tasks/{id}/work");
    let work = || x.get(&path);
    let pool = x.f.db.pool().await;
    // Not on an automatic router: no routing object, even with routing on.
    sqlx::query("UPDATE routing_state SET enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    bind_catalog(&x, false).await;
    assert!(work().await.get("routing").is_none());

    bind_catalog(&x, true).await;
    let waiting = work().await;
    assert_eq!(waiting["readiness"]["state"], "ready");
    // The read has no ready time, so the node's updated_at (2026-01-01T00:00:00Z) is used.
    assert_eq!(
        waiting["routing"],
        json!({"state":"waiting","since_ms":1767225600000_i64})
    );
    // Routing on, not ready (an open blocker), no route: nothing is held back for routing.
    let (blocked, blocker) = (Uuid::new_v4(), Uuid::new_v4());
    x.graph.contexts.lock().unwrap().insert(
        blocked,
        context(
            &task(blocked, w, "Blocked", "active"),
            vec![task(blocker, w, "First", "active")],
            vec![edge(w, blocker, blocked, "blocks")],
            false,
        ),
    );
    let held = x.get(&format!("/tasks/{blocked}/work")).await;
    assert_eq!(held["readiness"]["state"], "waiting");
    assert_eq!(held["routing"], json!({"state":"unrouted"}));
    // Routing off and no route: the owner can still route by hand.
    sqlx::query("UPDATE routing_state SET enabled=false WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(work().await["routing"], json!({"state":"unrouted"}));
    sqlx::query("UPDATE routing_state SET enabled=true WHERE singleton")
        .execute(&pool)
        .await
        .unwrap();

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
    .execute(&pool)
    .await
    .unwrap();
    let mut routed = node.clone();
    routed["metadata"] =
        aq_routing::with_route(&node["metadata"], Some(&route.compact(1).unwrap())).unwrap();
    x.graph
        .contexts
        .lock()
        .unwrap()
        .insert(id, context(&routed, vec![], vec![], false));
    assert_eq!(
        work().await["routing"],
        json!({"state":"routed","since_ms":1790000000000_i64,"route":{"class":"standard-high",
            "harnesses":["codex"],"providers":["openai"],"prefer":[],"lane":"backend",
            "provider_intent":"any","source":"owner","policy_digest":null}})
    );
}

#[tokio::test]
async fn routes_history_lists_reasons_newest_first() {
    let x = setup().await;
    let id = Uuid::new_v4();
    let pool = x.f.db.pool().await;
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

#[tokio::test]
async fn work_read_survives_a_routing_view_that_cannot_be_computed() {
    let x = setup().await;
    let (w, id) = (x.w(), Uuid::new_v4());
    x.graph.contexts.lock().unwrap().insert(
        id,
        context(&task(id, w, "Any", "active"), vec![], vec![], false),
    );
    // The binding's catalog digest names no stored artifact (the fixture stores none).
    let work = x.get(&format!("/tasks/{id}/work")).await;
    assert_eq!(work["readiness"]["state"], "ready");
    assert!(work.get("routing").is_none());
}
