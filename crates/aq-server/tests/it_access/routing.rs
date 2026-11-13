//! Route storage on the task graph, route history and the routing switch
//! (spec 0011 §3, §4), against a minimal fake Graph service.
use aq_routing::{Class, CompactRoute, ProviderIntent, Route, RouteReason, Source};
use aq_server::{
    AppState,
    agent_access::{AgentAccessConfig, Runtime},
    config::Config,
    error::AppError,
    identity::verifier_from_config,
    routing::{self, StoredRoute, Written},
    test_support::TestDb,
};
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

const NOW_MS: i64 = 1_000_000;

#[derive(Default)]
struct Fake {
    nodes: BTreeMap<String, Value>,
    posts: usize,
}
type Shared = Arc<Mutex<Fake>>;

fn reply(code: StatusCode, body: Value) -> Response {
    (code, [("X-AQ-Graph-API-Version", "1")], axum::Json(body)).into_response()
}

/// GET a node; POST `update_node` with the version check and a whole-field patch.
async fn graph(State(fake): State<Shared>, request: Request) -> Response {
    let parts: Vec<String> = request.uri().path().split('/').map(String::from).collect();
    let mut fake = fake.lock().await;
    if request.method() == Method::GET && parts.len() == 6 && parts[4] == "nodes" {
        return match fake.nodes.get(&parts[5]) {
            Some(n) => reply(StatusCode::OK, n.clone()),
            None => reply(StatusCode::NOT_FOUND, json!({"error":{"code":"not_found"}})),
        };
    }
    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    fake.posts += 1;
    let op = &body["operations"][0];
    assert_eq!(op["op"], "update_node");
    let node = fake.nodes.get_mut(op["id"].as_str().unwrap()).unwrap();
    if node["retired"] == true {
        return reply(StatusCode::FORBIDDEN, json!({"error":{"code":"forbidden"}}));
    }
    if node["version"] != op["expected_version"] {
        return reply(StatusCode::CONFLICT, json!({"error":{"code":"conflict"}}));
    }
    for (k, v) in op["patch"].as_object().unwrap() {
        node[k] = v.clone();
    }
    node["version"] = json!(node["version"].as_u64().unwrap() + 1);
    node["content_revision"] = json!(node["content_revision"].as_u64().unwrap() + 1);
    let result = node.clone();
    reply(
        StatusCode::OK,
        json!({"change_set_id":Uuid::new_v4(),"results":[result]}),
    )
}

struct Env {
    _db: TestDb,
    pool: PgPool,
    state: AppState,
    fake: Shared,
    workspace: Uuid,
    task: Uuid,
    _graph: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn node(&self) -> Value {
        self.fake.lock().await.nodes[&self.task.to_string()].clone()
    }
    async fn posts(&self) -> usize {
        self.fake.lock().await.posts
    }
    async fn stored(&self) -> Option<CompactRoute> {
        CompactRoute::from_metadata(&self.node().await["metadata"]).unwrap()
    }
    async fn history(&self) -> Vec<StoredRoute> {
        routing::history(&self.pool, self.task).await.unwrap()
    }
    async fn write(&self, version: u64, route: &Route) -> Result<Written, AppError> {
        routing::write(&self.state, self.workspace, self.task, version, route).await
    }
}

async fn setup() -> Env {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
    ))
    .unwrap();
    let task = Uuid::from_u128(1);
    let mut node = fixture["nodes"][task.to_string()].clone();
    node["metadata"] = json!({"aq:priority": 5, "aq:project_id": Uuid::from_u128(7)});
    let workspace: Uuid = node["workspace_id"].as_str().unwrap().parse().unwrap();
    let fake: Shared = Arc::new(Mutex::new(Fake {
        nodes: BTreeMap::from([(task.to_string(), node)]),
        posts: 0,
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let service = Router::new().fallback(graph).with_state(fake.clone());
    let graph_task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("graph-token");
    std::fs::write(&token_file, "ordinary-service-credential-0000000000000000").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = Runtime::new(&AgentAccessConfig {
        mutation_actor_id: Some("api01-writer".into()),
        graph_url: origin,
        graph_token_file: token_file,
        credential_ttl_ms: 10000,
    })
    .await
    .unwrap();
    let config: Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )
    .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(|| Utc.timestamp_millis_opt(NOW_MS).unwrap()),
        agent_access: Some(Arc::new(runtime)),
        source_retention: None,
    };
    Env {
        _db: db,
        pool,
        state,
        fake,
        workspace,
        task,
        _graph: graph_task,
        _dir: dir,
    }
}

fn class(s: &str) -> Class {
    s.parse().unwrap()
}

fn route(source: Source) -> Route {
    Route {
        class: class("standard-high"),
        harnesses: vec!["codex".into()],
        providers: vec![],
        prefer: vec![],
        lane: None,
        provider_intent: ProviderIntent::Any,
        reasons: vec![RouteReason {
            code: "lane_matched".into(),
            detail: "implementation".into(),
        }],
        classification: None,
        policy_digest: Some("ab".repeat(32)),
        structure_revision: Some(7),
        source,
    }
}

#[tokio::test]
async fn write_sets_aq_route_keeps_other_metadata_and_records_history() {
    let e = setup().await;
    let first = route(Source::Router);
    assert_eq!(
        e.write(3, &first).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    let node = e.node().await;
    assert_eq!(node["version"], 4);
    assert_eq!(node["metadata"]["aq:priority"], 5);
    assert_eq!(node["metadata"]["aq:project_id"], json!(Uuid::from_u128(7)));
    assert_eq!(e.stored().await, Some(first.compact(1).unwrap()));
    let mut second = route(Source::Router);
    second.class = class("complex-high");
    assert_eq!(
        e.write(4, &second).await.unwrap(),
        Written::Routed { sequence: 2 }
    );
    assert_eq!(e.stored().await.unwrap().c, class("complex-high"));
    assert_eq!(
        e.history().await,
        vec![
            StoredRoute {
                sequence: 2,
                route: Some(second),
                at_ms: NOW_MS
            },
            StoredRoute {
                sequence: 1,
                route: Some(first),
                at_ms: NOW_MS
            },
        ]
    );
}

#[tokio::test]
async fn write_against_an_older_task_version_is_stale_and_leaves_no_history_row() {
    let e = setup().await;
    let before = e.node().await;
    // The node is at version 3; the graph refuses expected version 2 with a 409.
    assert_eq!(
        e.write(2, &route(Source::Router)).await.unwrap(),
        Written::Stale
    );
    assert_eq!(e.posts().await, 1, "the stale write reached the graph");
    assert_eq!(e.node().await, before);
    assert!(e.history().await.is_empty());
    // The freed sequence is used again under a fresh idempotency key.
    assert_eq!(
        e.write(3, &route(Source::Router)).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    assert_eq!(e.history().await.len(), 1);
    // A write the graph refuses outright leaves no history row either.
    e.fake
        .lock()
        .await
        .nodes
        .get_mut(&e.task.to_string())
        .unwrap()["retired"] = json!(true);
    assert!(e.write(4, &route(Source::Router)).await.is_err());
    assert_eq!(e.history().await.len(), 1);
}

#[tokio::test]
async fn router_never_overwrites_an_owner_route_but_the_owner_can_replace_a_router_route() {
    let e = setup().await;
    assert_eq!(
        e.write(3, &route(Source::Router)).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    let owner = route(Source::Owner);
    assert_eq!(
        e.write(4, &owner).await.unwrap(),
        Written::Routed { sequence: 2 }
    );
    assert_eq!(e.stored().await, Some(owner.compact(2).unwrap()));
    let posts = e.posts().await;
    assert_eq!(
        e.write(5, &route(Source::Router)).await.unwrap(),
        Written::Stale
    );
    assert_eq!(
        routing::clear(&e.state, e.workspace, e.task, 5, Source::Router)
            .await
            .unwrap(),
        Written::Stale
    );
    assert_eq!(e.posts().await, posts, "a refused router write never posts");
    assert_eq!(e.stored().await.unwrap().s, Source::Owner);
    assert_eq!(e.history().await.len(), 2);
    let mut replaced = route(Source::Owner);
    replaced.class = class("deep-max");
    assert_eq!(
        e.write(5, &replaced).await.unwrap(),
        Written::Routed { sequence: 3 }
    );
}

#[tokio::test]
async fn clear_removes_the_key_and_records_a_cleared_entry() {
    let e = setup().await;
    let first = route(Source::Router);
    assert_eq!(
        e.write(3, &first).await.unwrap(),
        Written::Routed { sequence: 1 }
    );
    assert_eq!(
        routing::clear(&e.state, e.workspace, e.task, 4, Source::Owner)
            .await
            .unwrap(),
        Written::Routed { sequence: 2 }
    );
    let node = e.node().await;
    assert_eq!(
        node["metadata"],
        json!({"aq:priority": 5, "aq:project_id": Uuid::from_u128(7)})
    );
    let history = e.history().await;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0],
        StoredRoute {
            sequence: 2,
            route: None,
            at_ms: NOW_MS
        }
    );
    assert_eq!(history[1].route, Some(first));
}

fn catalog() -> aq_profiles::ValidatedCatalog {
    let base = aq_profiles::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog"),
    )
    .unwrap();
    let instructions = base.artifact().instructions.clone();
    let mut catalog = base.artifact().catalog.clone();
    let mut assigned = catalog.intelligence[0].clone();
    assigned.id = "careful".into();
    assigned.version = 3;
    let mut router = catalog.intelligence[0].clone();
    router.id = "router".into();
    router.automatic_routing = Some(true);
    catalog.intelligence.extend([assigned, router]);
    aq_profiles::validate(catalog, instructions).unwrap()
}

async fn assign(pool: &PgPool, revision: i64, assignments: Value) {
    let parents: Vec<Value> = ["simple", "standard", "complex", "deep"]
        .iter()
        .map(|id| json!({"id": id, "description": ""}))
        .collect();
    sqlx::query("UPDATE intelligence_structure SET revision=$1, document=$2 WHERE singleton")
        .bind(revision)
        .bind(json!({"routing_markdown":"","parents":parents,"assignments":assignments}))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn class_profile_follows_structure_assignments_and_is_none_for_an_unassigned_slot() {
    let e = setup().await;
    let catalog = catalog();
    let profile = |c: &'static str| {
        let (pool, catalog) = (&e.pool, &catalog);
        async move {
            routing::class_profile(pool, catalog, class(c))
                .await
                .unwrap()
        }
    };
    assign(
        &e.pool,
        1,
        json!([
            {"profile_id":"careful","parent_id":"standard","effort":"high"},
            {"profile_id":"router","parent_id":"deep","effort":"max"},
        ]),
    )
    .await;
    assert_eq!(profile("standard-high").await.as_deref(), Some("careful@3"));
    assert_eq!(profile("simple-low").await, None);
    assert_eq!(
        profile("deep-max").await,
        None,
        "the router is never a class profile"
    );
    assign(
        &e.pool,
        2,
        json!([{"profile_id":"careful","parent_id":"complex","effort":"high"}]),
    )
    .await;
    assert_eq!(profile("standard-high").await, None);
    assert_eq!(profile("complex-high").await.as_deref(), Some("careful@3"));
}

#[tokio::test]
async fn routing_cannot_be_enabled_without_an_active_policy() {
    let e = setup().await;
    let pool = &e.pool;
    assert!(!routing::enabled(pool).await.unwrap());
    assert!(matches!(
        routing::set_enabled(pool, true).await,
        Err(AppError::Conflict(_))
    ));
    sqlx::query(
        "INSERT INTO routing_policies(digest,status,created_at_ms) VALUES('d1','failed',1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE routing_state SET active_policy_digest='d1'")
        .execute(pool)
        .await
        .unwrap();
    assert!(routing::set_enabled(pool, true).await.is_err());
    assert!(!routing::enabled(pool).await.unwrap());
    sqlx::query("UPDATE routing_policies SET status='active' WHERE digest='d1'")
        .execute(pool)
        .await
        .unwrap();
    routing::set_enabled(pool, true).await.unwrap();
    assert!(routing::enabled(pool).await.unwrap());
    routing::set_enabled(pool, false).await.unwrap();
    assert!(!routing::enabled(pool).await.unwrap());
}
