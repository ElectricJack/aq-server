use super::launch::Launch;
use super::*;
use aq_server::store::Store;
/// Also the agent-credential fixture for `agent_task_comments`.
pub(crate) struct Env {
    pub(crate) db: TestDb,
    pub(crate) pool: sqlx::PgPool,
    pub(crate) app: Router,
    pub(crate) admin: Router,
    pub(crate) token: String,
    pub(crate) binding: Value,
    pub(crate) time: Arc<std::sync::atomic::AtomicI64>,
    pub(super) node_token: String,
}
pub(crate) async fn setup() -> Env {
    setup_with(None).await
}
/// `grants` is the spec 0006 grant set provisioned with the launch.
pub(crate) async fn setup_with(grants: Option<Value>) -> Env {
    setup_graph(grants, "http://127.0.0.1:9").await
}
/// As `setup_with`, with the agent graph routes reading from `graph_url`.
pub(crate) async fn setup_graph(grants: Option<Value>, graph_url: &str) -> Env {
    setup_graph_with(grants, graph_url, None, |_, _| {}).await
}
/// `setup_graph` with the launch profile and startup context edited first,
/// for example to the shape a native launch resolves, and optionally a
/// configured Graph mutation actor (the service identity creates use).
pub(crate) async fn setup_graph_with(
    grants: Option<Value>,
    graph_url: &str,
    mutation_actor_id: Option<&str>,
    edit: impl FnOnce(&mut Value, &mut Value),
) -> Env {
    setup_graph_with_attempt(
        grants,
        GraphEndpoint {
            url: graph_url,
            mutation_actor_id,
        },
        None,
        edit,
    )
    .await
}
pub(crate) struct GraphEndpoint<'a> {
    pub(crate) url: &'a str,
    pub(crate) mutation_actor_id: Option<&'a str>,
}
pub(crate) async fn setup_graph_with_attempt(
    grants: Option<Value>,
    endpoint: GraphEndpoint<'_>,
    attempt: Option<Uuid>,
    edit: impl FnOnce(&mut Value, &mut Value),
) -> Env {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let (node, node_token) = enroll(&store).await;
    let launch = Launch::new(node, attempt, edit);
    launch.install(&pool, node).await;
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime(endpoint.url, endpoint.mutation_actor_id, &directory).await;
    let (state, time) = state(store, runtime);
    let app = aq_server::build_worker_router(state.clone());
    let admin = aq_server::build_admin_router(state);
    launch.provision(&admin, grants).await;
    let token = launch.issue(&app, &node_token).await;
    Env {
        db,
        pool,
        app,
        admin,
        token,
        binding: launch.binding,
        time,
        node_token,
    }
}
async fn enroll(store: &Store) -> (Uuid, String) {
    let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    let node_token = tokens::new_node_credential();
    store
        .create_enrollment("agent-test", owner.id, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let node = store
        .redeem_enrollment(
            "agent-test",
            NewNode {
                name: "agent".into(),
                credential_hash: tokens::verifier(&node_token),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    (node.id, node_token)
}
async fn runtime(
    graph_url: &str,
    mutation_actor_id: Option<&str>,
    directory: &tempfile::TempDir,
) -> Runtime {
    let token_file = directory.path().join("graph-token");
    std::fs::write(&token_file, "ordinary-service-credential-0000000000000000").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    Runtime::new(&AgentAccessConfig {
        mutation_actor_id: mutation_actor_id.map(String::from),
        graph_url: graph_url.into(),
        graph_token_file: token_file,
        credential_ttl_ms: 10000,
    })
    .await
    .unwrap()
}
fn state(store: Store, runtime: Runtime) -> (AppState, Arc<std::sync::atomic::AtomicI64>) {
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
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock_time = time.clone();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(move || {
            Utc.timestamp_millis_opt(clock_time.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap()
        }),
        agent_access: Some(Arc::new(runtime)),
        source_retention: None,
    };
    (state, time)
}
