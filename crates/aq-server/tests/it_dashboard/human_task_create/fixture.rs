use super::*;
pub(super) struct Scenario {
    pub(super) f: common::profiled::Fixture,
    pub(super) pool: sqlx::PgPool,
    pub(super) app: Router,
    pub(super) path: String,
    pub(super) project: Uuid,
    pub(super) workspace: Uuid,
    pub(super) origin: String,
    pub(super) config: AgentAccessConfig,
    pub(super) trace: Observation,
    child: tokio::process::Child,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
    _graph_db: TestDb,
}
pub(super) const TOKEN: &str = "human-create-isolated-fixture-token";
impl Scenario {
    pub(super) async fn new() -> Self {
        let binary = std::env::var("AQ_TEST_GRAPH_BINARY").unwrap();
        assert_eq!(
            hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
            std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
        );
        let mut f = common::profiled::fixture().await;
        common::profiled::insert(&f).await;
        let pool = f.db.pool().await;
        let graph_db = TestDb::create().await;
        let dir = tempfile::tempdir().unwrap();
        let token = TOKEN;
        let token_path = dir.path().join("token");
        std::fs::write(&token_path, token).unwrap();
        std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let workspace = f.selection.lineage.workspace_id;
        let (child, origin) = start_graph(&binary, &graph_db, &token_path, workspace).await;
        let proxy = Forwarder::new(pool.clone(), origin.clone());
        let (proxy_origin, server) = proxy.serve().await;
        let config = AgentAccessConfig {
            graph_url: proxy_origin,
            graph_token_file: token_path,
            mutation_actor_id: Some("local-admin".into()),
            credential_ttl_ms: 120000,
        };
        f.state.agent_access = Some(Arc::new(Runtime::new(&config).await.unwrap()));
        assert!(human::task_create_available(&f.state));
        let app = aq_server::build_admin_router(f.state.clone());
        let project = f.selection.lineage.project_id;
        let path = format!("/api/human/projects/{project}/tasks");
        Self {
            f,
            pool,
            app,
            path,
            project,
            workspace,
            origin,
            config,
            trace: proxy.trace,
            child,
            server,
            _dir: dir,
            _graph_db: graph_db,
        }
    }
    pub(super) async fn shutdown(mut self) {
        self.server.abort();
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
    pub(super) async fn owner(&self, path: &str, body: Option<&str>) -> (StatusCode, Value) {
        send(&self.app, path, body, "owner@localhost").await
    }
    pub(super) fn action(&self, id: Uuid, revision: &str) -> String {
        let project = self.project;
        let payload: Value = serde_json::from_str(&request(id, revision)).unwrap();
        json!({"operation_id":id,"kind":"task.create","subject":{"type":"project","id":project,"revision":revision},"expected_versions":{"project":revision},"payload":payload}).to_string()
    }
}
async fn start_graph(
    binary: &str,
    graph_db: &TestDb,
    token_path: &std::path::Path,
    workspace: Uuid,
) -> (tokio::process::Child, String) {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    drop(socket);
    let mut child = tokio::process::Command::new(binary)
        .args([
            "serve",
            "--bind",
            &address.to_string(),
            "--bootstrap-workspace",
            &workspace.to_string(),
            "--bootstrap-token-file",
            token_path.to_str().unwrap(),
            "--pool-size",
            "2",
        ])
        .env("DATABASE_URL", graph_db.options().to_url_lossy().as_str())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let origin = format!("http://{address}");
    let client = reqwest::Client::new();
    let mut ready = false;
    for _ in 0..200 {
        if client
            .get(format!("{origin}/health/ready"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ready);
    (child, origin)
}
#[derive(Clone, Default)]
pub(super) struct Observation {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) lose: Arc<AtomicBool>,
    pub(super) bodies: Arc<tokio::sync::Mutex<Vec<Vec<u8>>>>,
    pub(super) hold: Arc<AtomicBool>,
    pub(super) arrived: Arc<tokio::sync::Notify>,
    pub(super) release: Arc<tokio::sync::Notify>,
}
#[derive(Clone)]
struct Forwarder {
    trace: Observation,
    pool: sqlx::PgPool,
    upstream: String,
}
impl Observation {
    pub(super) fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
impl Forwarder {
    fn new(pool: sqlx::PgPool, upstream: String) -> Self {
        Self {
            trace: Observation::default(),
            pool,
            upstream,
        }
    }
    async fn serve(&self) -> (String, tokio::task::JoinHandle<()>) {
        let proxy = Router::new()
            .route("/{*path}", axum::routing::post(Self::forward))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
        (proxy_origin, server)
    }
    async fn forward(
        axum::extract::State(proxy): axum::extract::State<Self>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let upstream = &proxy.upstream;
        let uri = request.uri().to_string();
        let body = axum::body::to_bytes(request.into_body(), 131072)
            .await
            .unwrap();
        let mapped: Value = serde_json::from_slice(&body).unwrap();
        let id: Uuid = mapped["operations"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let retained: Vec<u8> = sqlx::query_scalar("SELECT payload_bytes FROM human_graph_operations WHERE operation_id=$1 AND dispatch_count>0 AND status->>'state'='unknown'").bind(id).fetch_one(&proxy.pool).await.unwrap();
        assert_eq!(
            retained,
            body.as_ref(),
            "exact durable intent must precede Graph IO"
        );
        proxy.trace.bodies.lock().await.push(body.to_vec());
        proxy.trace.calls.fetch_add(1, Ordering::SeqCst);
        let res = reqwest::Client::new()
            .post(format!("{upstream}{uri}"))
            .bearer_auth(TOKEN)
            .header("X-AQ-Graph-API-Version", "1")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let code = res.status();
        let bytes = res.bytes().await.unwrap();
        if proxy.trace.hold.swap(false, Ordering::SeqCst) {
            proxy.trace.arrived.notify_one();
            proxy.trace.release.notified().await;
        }
        if proxy.trace.lose.swap(false, Ordering::SeqCst) {
            assert!(
                code.is_success(),
                "{code} {}",
                String::from_utf8_lossy(&bytes)
            );
            return axum::response::Response::builder()
                .status(503)
                .body(Body::empty())
                .unwrap();
        }
        axum::response::Response::builder()
            .status(code)
            .header("X-AQ-Graph-API-Version", "1")
            .body(Body::from(bytes))
            .unwrap()
    }
}
