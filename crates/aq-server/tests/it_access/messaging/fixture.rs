use super::*;
use crate::agent_launch::{Launch, enroll, state};
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
    /// The state `app` and `admin` serve, for a test that rebuilds them.
    #[cfg_attr(
        not(all(feature = "project_environments", feature = "delivery_playbook")),
        allow(dead_code)
    )]
    pub(crate) state: aq_server::AppState,
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
    let launch = Launch::new(node, attempt, |profile, context| {
        for cap in ["message.send", "message.read", "message.ack"] {
            profile["capabilities"]
                .as_array_mut()
                .unwrap()
                .push(json!(cap));
        }
        edit(profile, context);
    });
    launch.install(&pool, node).await;
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime(endpoint.url, endpoint.mutation_actor_id, &directory).await;
    let (state, time) = state(store, runtime);
    let app = aq_server::build_worker_router(state.clone());
    let admin = aq_server::build_admin_router(state.clone());
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
        state,
    }
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
