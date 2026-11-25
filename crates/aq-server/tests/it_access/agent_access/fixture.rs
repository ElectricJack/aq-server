use super::*;
impl Case {
    pub(super) async fn new() -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        let (node, node_token) = enroll(&store).await;
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let profile = fixture["context"]["profile"].clone();
        let context = fixture["context"]["context"].clone();
        let mut binding = fixture["context"]["binding"].clone();
        binding["node_id"] = json!(node);
        let launch = Launch::from_captured(node, binding, profile, context);
        let attempt_id: Uuid = launch.binding["attempt_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        launch.install(&pool, node).await;
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("graph-token");
        std::fs::write(&token_file, "ordinary-service-credential-0000000000000000").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let fake = Arc::new(FakeGraph {
            data: serde_json::from_str(include_str!(
                "../../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
            ))
            .unwrap(),
            hold: std::sync::atomic::AtomicBool::new(false),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_url = format!("http://{}", listener.local_addr().unwrap());
        let fake_app = Router::new().fallback(fake_graph).with_state(fake.clone());
        let fake_server =
            tokio::spawn(async move { axum::serve(listener, fake_app).await.unwrap() });
        let runtime = Runtime::new(&AgentAccessConfig {
            mutation_actor_id: None,
            graph_url: std::env::var("AQ_AGENT_GRAPH_TEST_URL").unwrap_or(fake_url),
            graph_token_file: std::env::var_os("AQ_AGENT_GRAPH_TEST_TOKEN_FILE")
                .map(std::path::PathBuf::from)
                .unwrap_or(token_file),
            credential_ttl_ms: 10000,
        })
        .await
        .unwrap();
        let (state, time) = state(store, runtime);
        let app = aq_server::agent_access::admin_routes(state.clone())
            .merge(aq_server::agent_access::machine_routes(state.clone()))
            .with_state(state.clone());
        Self {
            _db: db,
            _directory: directory,
            node,
            node_token,
            launch,
            attempt_id,
            app,
            state,
            time,
            pool,
            fake,
            graph: fake_server,
        }
    }
    pub(super) fn finish(self) {
        self.graph.abort();
    }
    pub(super) fn issue(&self, version: u64) -> Value {
        json!({"identity":self.launch.identity,"attempt":self.launch.attempt,"expected_credential_version":version})
    }
    pub(super) fn graph_request(&self, query: Value) -> Value {
        json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":self.launch.binding,"query":query})
    }
}
