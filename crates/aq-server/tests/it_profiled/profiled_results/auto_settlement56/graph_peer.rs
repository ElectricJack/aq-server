use super::*;
pub(super) struct GraphPeer {
    pub(super) writes: Arc<AtomicUsize>,
    pub(super) bodies: Arc<Mutex<Vec<Value>>>,
    pub(super) directory: tempfile::TempDir,
}
impl GraphPeer {
    pub(super) async fn install(
        f: &mut Fixture,
        expected_version: u64,
        graph_status: axum::http::StatusCode,
    ) -> Self {
        // Synthetic Graph peer: counts mutation writes and keeps each body.
        let graph = settlement_owner39::graph_context(f);
        assert_eq!(graph["node"]["version"], json!(expected_version));
        let writes = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (wc, bc) = (writes.clone(), bodies.clone());
        // The human work read's Context of the task (spec 0016 section 7's task read).
        let read = graph.clone();
        let app = Router::new().route(
            "/v1/workspaces/{workspace}/nodes/{id}/context",
            axum::routing::get(move || {
                std::future::ready(([("X-AQ-Graph-API-Version", "1")], Json(read.clone())))
            }),
        );
        let app = app.route(
        "/v1/workspaces/{workspace}/mutations",
        post(move |Json(body): Json<Value>| {
            let (g, wc, bc) = (graph.clone(), wc.clone(), bc.clone());
            async move {
                wc.fetch_add(1, Ordering::SeqCst);
                bc.lock().unwrap().push(body);
                let mut node = g["node"].clone();
                node["task_state"] = json!("completed");
                node["version"] = json!(node["version"].as_u64().unwrap() + 1);
                node["content_revision"] = json!(node["content_revision"].as_u64().unwrap() + 1);
                let reply = if graph_status.is_success() {
                    json!({"change_set_id":"00000000-0000-0000-0000-000000000052","results":[node]})
                } else {
                    json!({"error":"version_conflict"})
                };
                (graph_status, [("X-AQ-Graph-API-Version", "1")], Json(reply))
            }
        }),
    );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph-key");
        std::fs::write(&path, "x".repeat(32)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        f.state.agent_access = Some(Arc::new(
            aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
                graph_url: format!("http://{address}/"),
                graph_token_file: path,
                mutation_actor_id: Some("synthetic-settlement-service".into()),
                credential_ttl_ms: 120000,
            })
            .await
            .unwrap(),
        ));
        Self {
            writes,
            bodies,
            directory,
        }
    }
}
