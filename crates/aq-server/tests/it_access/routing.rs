//! Route storage on the task graph, route history and the routing switch
//! (spec 0011 §3, §4), against a minimal fake Graph service.
use aq_profiles::roles::Kind;
use aq_routing::{
    Class, CompactRoute, ProviderIntent, Route, RouteReason, Source, classify::Offer,
};
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
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use tower::ServiceExt;
use uuid::Uuid;

const NOW_MS: i64 = 1_000_000;

mod graph;
use graph::{Fake, Shared, graph};

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
        ..Fake::default()
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
        ended_attempts: 0,
        judgment: None,
    }
}

mod history;

mod owner;
