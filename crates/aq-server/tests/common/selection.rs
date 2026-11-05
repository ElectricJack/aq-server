//! AQ-19 public-owner routes, fake authoritative peers and explicit real-peer gate.
use aq_server::{
    AppState,
    agent_access::{AgentAccessConfig, Runtime as GraphRuntime},
    config::Config,
    source_retention::{Runtime as GitRuntime, SourceRetentionConfig},
    store::NewNode,
    test_support::TestDb,
    tokens,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use chrono::{Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;
use uuid::Uuid;
pub struct Env {
    pub app: Router,
    pub state: AppState,
    pub pool: sqlx::PgPool,
    pub node: Uuid,
    pub body: Value,
    pub generation: Arc<AtomicU64>,
    pub git_status: Arc<AtomicU8>,
    pub graph_status: Arc<AtomicU8>,
    pub time: Arc<AtomicI64>,
    pub tasks: Vec<tokio::task::JoinHandle<()>>,
    pub _dir: tempfile::TempDir,
    pub _db: Arc<TestDb>,
    pub credential: String,
    pub _gate: FixtureGate,
}
impl Drop for Env {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
async fn graph(
    State((status, workspace)): State<(Arc<AtomicU8>, Uuid)>,
    Path(id): Path<Uuid>,
) -> impl IntoResponse {
    let status = if id != workspace {
        StatusCode::NOT_FOUND
    } else {
        match status.load(Ordering::SeqCst) {
            1 => StatusCode::FORBIDDEN,
            2 => StatusCode::NOT_FOUND,
            3 => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::OK,
        }
    };
    (
        status,
        [("X-AQ-Graph-API-Version", "1")],
        Json(if status == StatusCode::OK {
            json!({"items":[],"next_cursor":null})
        } else {
            json!({"error":"unavailable"})
        }),
    )
}
pub async fn call(
    app: &Router,
    path: &str,
    body: Option<Value>,
    email: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("content-type", "application/json")
        .header("X-AQ-Request", "1")
        .header("x-aq-dev-email", email);
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}
#[path = "selection_actual.rs"]
mod actual;
pub use actual::{ActualSelectionFixture, ActualSetup, setup_actual};
pub type ActualProfileChoice = actual::ActualProfileChoice;

// Actual and historical fixtures share the existing test-only serialization
// gate. Neither constructor creates a second admission driver.
//
// The gate is not re-entrant: a test that builds a second fixture while its
// first is still alive (including one merely shadowed by `let c = ...` again)
// would wait on itself forever. That is refused at once instead of hanging.
static GATE_HOLDER: std::sync::Mutex<Option<std::thread::ThreadId>> = std::sync::Mutex::new(None);
pub struct FixtureGate(#[allow(dead_code)] tokio::sync::OwnedMutexGuard<()>);
impl Drop for FixtureGate {
    fn drop(&mut self) {
        // Cleared before the guard field releases the mutex.
        *GATE_HOLDER.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}
async fn fixture_gate() -> FixtureGate {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();
    let me = std::thread::current().id();
    assert_ne!(
        *GATE_HOLDER.lock().unwrap_or_else(|p| p.into_inner()),
        Some(me),
        "a selection fixture is still alive on this test thread; drop it before \
         building another (the fixture gate is not re-entrant and would deadlock)"
    );
    let guard = GATE
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    *GATE_HOLDER.lock().unwrap_or_else(|p| p.into_inner()) = Some(me);
    FixtureGate(guard)
}
pub async fn setup(real: bool) -> Env {
    let gate = fixture_gate().await;
    let db = Arc::new(TestDb::create().await);
    let store = db.store().await;
    let pool = db.pool().await;
    let dir = tempfile::tempdir().unwrap();
    let generation = Arc::new(AtomicU64::new(1));
    let git_status = Arc::new(AtomicU8::new(0));
    let graph_status = Arc::new(AtomicU8::new(0));
    let mut tasks = Vec::new();
    let workspace = if real {
        std::env::var("AQ_PROJECT_WORKSPACE_ID")
            .unwrap()
            .parse()
            .unwrap()
    } else {
        Uuid::from_u128(3)
    };
    let repository = if real {
        std::env::var("AQ_PROJECT_REPOSITORY_ID")
            .unwrap()
            .parse()
            .unwrap()
    } else {
        Uuid::from_u128(2)
    };
    let socket = if real {
        PathBuf::from(std::env::var("AQ_PROJECT_GIT_SOCKET").unwrap())
    } else {
        dir.path().join("git.sock")
    };
    if !real {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let g = generation.clone();
        let status = git_status.clone();
        tasks.push(tokio::spawn(async move {loop {let(stream,_)=listener.accept().await.unwrap();let g=g.clone();let status=status.clone();tokio::spawn(async move {let mut reader=BufReader::new(stream);let mut line=Vec::new();reader.read_until(b'\n',&mut line).await.unwrap();let r:Value=serde_json::from_slice(&line).unwrap();assert_eq!(r["operation"],"repo.show");assert_eq!(r["protocol_version"],1);let expected=r["expected_binding_generation"].as_u64().unwrap();let current=g.load(Ordering::SeqCst);let mut out=json!({"protocol_version":1,"request_id":r["request_id"]});
            if r["repository_id"]!=repository.to_string() || status.load(Ordering::SeqCst)==1 {out["ok"]=false.into();out["error"]=json!({"code":"repository_not_found","message":"absent","details":{},"retry":"refresh_inputs"});}
            else if expected!=current {out["ok"]=false.into();out["error"]=json!({"code":"binding_conflict","message":"stale","details":{"binding_generation":current},"retry":"refresh_inputs"});}
            else {out["ok"]=true.into();out["result"]=json!({"repository_id":repository,"binding_generation":current,"mode":if status.load(Ordering::SeqCst)==2{"worker"}else{"central"},"object_format":"sha1"});}
            let mut bytes=serde_json::to_vec(&out).unwrap();bytes.push(b'\n');reader.into_inner().write_all(&bytes).await.unwrap();});}}));
    }
    let graph_url = if real {
        std::env::var("AQ_AGENT_GRAPH_TEST_URL").unwrap()
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/v1/workspaces/{id}/nodes", get(graph))
            .with_state((graph_status.clone(), workspace));
        tasks.push(tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap()
        }));
        format!("http://{addr}")
    };
    let token_file = if real {
        PathBuf::from(std::env::var("AQ_AGENT_GRAPH_TEST_TOKEN_FILE").unwrap())
    } else {
        let p = dir.path().join("graph-token");
        std::fs::write(&p, "synthetic-project-read-token-0000000000000000").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        p
    };
    let graph = GraphRuntime::new(&AgentAccessConfig {
        graph_url,
        graph_token_file: token_file,
        credential_ttl_ms: 10_000,
        mutation_actor_id: None,
    })
    .await
    .unwrap();
    let git = GitRuntime::new(SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: dir.path().join("staging"),
        git_root: dir.path().join("git"),
        git_socket: socket,
        max_staging_bytes: 1024 * 1024,
        max_bundle_bytes: 1024 * 1024,
        bulk_concurrency: 1,
        incomplete_ttl_secs: 60,
    })
    .unwrap();
    let now = Utc::now();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, now)
        .await
        .unwrap();
    store
        .create_enrollment("project-enroll", owner.id, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let token = tokens::new_node_credential();
    let node = store
        .redeem_enrollment(
            "project-enroll",
            NewNode {
                name: "project-worker".into(),
                credential_hash: tokens::verifier(&token),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE nodes SET last_seen_at=$2 WHERE id=$1")
        .bind(node.id)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
    let mut config: Config = toml::from_str(
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
    config
        .projects
        .worker_groups
        .insert("engineering".into(), vec![node.id]);
    let time = Arc::new(AtomicI64::new(now.timestamp_millis()));
    let clock = time.clone();
    let state = AppState {
        config: Arc::new(config),
        store,
        identity: Arc::new(aq_server::identity::DevIdentity::new(
            "owner@localhost".into(),
        )),
        clock: Arc::new(move || {
            Utc.timestamp_millis_opt(clock.load(Ordering::SeqCst))
                .unwrap()
        }),
        agent_access: Some(Arc::new(graph)),
        source_retention: Some(Arc::new(git)),
    };
    let app = aq_server::build_admin_router(state.clone());
    let catalog = aq_profiles::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog"),
    )
    .unwrap();
    let activated=call(&app,"/api/profiles/catalogs/activate",Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"artifact_json":String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap()})),"owner@localhost").await;
    assert_eq!(activated.0, StatusCode::OK, "{}", activated.1);
    let body = json!({"schema_version":1,"project_id":Uuid::from_u128(1),"name":"Example project","enabled":true,"repository_id":repository,"repository_binding_generation":1,"repository_adapter_reference":"aq.git.configured","graph_adapter_reference":"aq.graph.configured","graph_workspace_id":workspace,"catalog_digest":catalog.digest(),"allowed_agent_profiles":["implementer@1"],"allowed_intelligence_profiles":["engineering@1"],"allowed_execution_profiles":["confined-development@1"],"allowed_node_ids":[],"allowed_worker_groups":["engineering"],"allowed_environments":["confined-worktree"],"policy":{"identity":"policy","revision":"1","digest":"b".repeat(64)},"context":[],"ceilings":{"permissions":[{"operation":"task.read","scope_reference":"project:example"}],"context_bytes":8192,"wall_time_ms":60000,"memory_bytes":268435456,"cpu_millis":60000,"process_limit":8,"cost_limit_microunits":null,"currency":null,"require_hard_cost_enforcement":false}});
    Env {
        app,
        state,
        pool,
        node: node.id,
        body,
        generation,
        git_status,
        graph_status,
        time,
        tasks,
        _dir: dir,
        _db: db,
        credential: token,
        _gate: gate,
    }
}
pub fn path() -> String {
    format!("/api/projects/{}", Uuid::from_u128(1))
}
pub fn admission(env: &Env) -> Value {
    json!({"expected_project_revision":1,"expected_catalog_revision":1,"node_id":env.node,"environment":"confined-worktree","agent_profile":"implementer@1","intelligence_profile":"engineering@1","execution_profile":"confined-development@1"})
}
pub async fn bind(env: &Env) -> Value {
    let (status, v) = call(
        &env.app,
        &path(),
        Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"binding":env.body})),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    v
}
pub struct SelectionFixture {
    pub env: Env,
    pub input: aq_selection::SelectionInputs,
    pub frozen: aq_server::project_bindings::domain::FrozenProjectBinding,
    pub actor: Uuid,
    pub lineage: aq_selection::TaskLineage,
    pub manual: Value,
    pub unrelated: aq_selection::AuthorizedCredential,
    pub time: u64,
    pub operation: Uuid,
}
tokio::task_local! {
    /// Within this scope the selection fixture configures its credentials as
    /// subscription logins with an owner-only material store (C5b).
    pub static SUBSCRIPTION_LOGIN: bool;
}
pub async fn selection_fixture() -> SelectionFixture {
    use aq_selection::*;
    use aq_server::intelligence::config::{CredentialConfig, IntelligenceConfig};
    let mut env = setup(false).await;
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/selection-v2.json")).unwrap();
    let mut input: SelectionInputs = serde_json::from_value(fixture["inputs"].clone()).unwrap();
    let activation=call(&env.app,"/api/profiles/catalogs/activate",Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"artifact_json":fixture["catalog_artifact_json"]})),"owner@localhost").await;
    assert_eq!(activation.0, StatusCode::OK, "{}", activation.1);
    env.body["catalog_digest"] = json!(input.project.catalog_digest);
    env.body["allowed_agent_profiles"] = json!(["implementer@2"]);
    env.body["allowed_intelligence_profiles"] = json!(["engineering@2"]);
    env.body["allowed_execution_profiles"] = json!(["confined-development@2"]);
    env.body["ceilings"]["permissions"] = json!(
        input
            .project
            .permission_ceiling
            .iter()
            .map(|p| json!({"operation":p.operation,"scope_reference":p.resource}))
            .collect::<Vec<_>>()
    );
    bind(&env).await;
    let request=serde_json::from_value(json!({"expected_project_revision":1,"expected_catalog_revision":2,"node_id":env.node,"environment":"confined-worktree","agent_profile":"implementer@2","intelligence_profile":"engineering@2","execution_profile":"confined-development@2"})).unwrap();
    let frozen =
        aq_server::project_bindings::check_eligibility(&env.state, Uuid::from_u128(1), &request)
            .await
            .unwrap()
            .frozen_binding
            .unwrap();
    let boot = Uuid::new_v4();
    env.state
        .store
        .execution_register(&aq_worker_protocol::RegisterRequest {
            identity: aq_worker_protocol::NodeIdentity {
                node_id: env.node,
                boot_id: boot,
            },
            operation_id: Uuid::new_v4(),
            protocol_version: 2,
            worker_version: "0.1.0".into(),
            capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
        })
        .await
        .unwrap();
    let key = env._dir.path().join("synthetic-provider-key");
    std::fs::write(&key, b"synthetic-not-a-provider-key").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let login = SUBSCRIPTION_LOGIN.try_with(|v| *v).unwrap_or(false);
    let mut configured: Vec<_> = input
        .credentials
        .iter()
        .cloned()
        .map(|mut c| {
            c.provisionable = false;
            CredentialConfig {
                kind: if login {
                    aq_selection::CredentialKind::SubscriptionLogin
                } else {
                    Default::default()
                },
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: c,
                allowed_node_ids: vec![env.node],
                secret_file: Some(key.clone()),
                delivery_mode: None,
            }
        })
        .collect();
    let mut unrelated = configured[0].credential.clone();
    unrelated.profile = ArtifactRef {
        id: Text::new("unrelated-account").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"unrelated"),
    };
    unrelated.provider = "unrelated-provider".into();
    unrelated.account_reference = "unrelated-account".into();
    configured.push(CredentialConfig {
        kind: Default::default(),
        allowed_backends: aq_selection::default_allowed_backends(),
        credential: unrelated.clone(),
        allowed_node_ids: vec![],
        secret_file: Some(key.clone()),
        delivery_mode: None,
    });
    let config = IntelligenceConfig {
        accounting: Default::default(),
        material_store: login.then(|| env._dir.path().join("material-store")),
        revision: 1,
        credentials: configured,
        qualified_harnesses: input.node.harnesses.clone(),
    };
    let mut cfg = (*env.state.config).clone();
    cfg.intelligence = Some(Arc::new(config.load().unwrap()));
    env.state.config = Arc::new(cfg);
    env.app = aq_server::build_admin_router(env.state.clone());
    assert_eq!(
        call(
            &env.app,
            "/api/intelligence/register",
            Some(json!({})),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::OK
    );
    let time = env.time.load(Ordering::SeqCst) as u64;
    input.node.target = ExecutionTarget::Worker {
        node_id: env.node,
        boot_id: boot,
    };
    input.node.observed_at_ms = time;
    input.node.expires_at_ms = time + 30_000;
    for c in &mut input.node.credential_bindings {
        c.observed_at_ms = time;
        c.expires_at_ms = time + 30_000;
    }
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)").bind(env.node).bind(boot).bind(json!(input.node)).execute(&env.pool).await.unwrap();
    // Fresh operator observation for the preferred account. No provider probe.
    let scope = input.observations[0].scope.clone();
    let manual = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"sequence":1,"scope":scope,"state":"available","ttl_ms":30_000,"cooldown_ms":0});
    assert_eq!(
        call(
            &env.app,
            "/api/intelligence/observations",
            Some(manual.clone()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::OK
    );
    let actor: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE role='owner'")
        .fetch_one(&env.pool)
        .await
        .unwrap();
    let operation = Uuid::new_v4();
    let lineage = TaskLineage {
        project_id: Uuid::from_u128(1),
        workspace_id: env.body["graph_workspace_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
        task_id: Uuid::new_v4(),
    };
    SelectionFixture {
        env,
        input,
        frozen,
        actor,
        lineage,
        manual,
        unrelated,
        time,
        operation,
    }
}
