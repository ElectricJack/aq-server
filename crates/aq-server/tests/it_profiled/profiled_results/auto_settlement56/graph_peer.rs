use super::*;
pub(super) struct GraphPeer {
    pub(super) writes: Arc<AtomicUsize>,
    pub(super) bodies: Arc<Mutex<Vec<Value>>>,
    pub(super) recovery: Arc<GraphRecovery>,
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
        let recovery = Arc::new(GraphRecovery {
            write_status: AtomicU16::new(graph_status.as_u16()),
            lookup_status: AtomicU16::new(200),
            has_receipt: AtomicBool::new(false),
            lookups: Mutex::default(),
        });
        let mut node = graph["node"].clone();
        node["task_state"] = json!("completed");
        node["version"] = json!(node["version"].as_u64().unwrap() + 1);
        node["content_revision"] = json!(node["content_revision"].as_u64().unwrap() + 1);
        let receipt =
            json!({"change_set_id":"00000000-0000-0000-0000-000000000052","results":[node]});
        let (wc, bc, writer, committed) = (
            writes.clone(),
            bodies.clone(),
            recovery.clone(),
            receipt.clone(),
        );
        let reader = recovery.clone();
        let app = Router::new()
            .route(
                "/v1/workspaces/{workspace}/nodes/{id}/context",
                axum::routing::get(move || {
                    std::future::ready(([("X-AQ-Graph-API-Version", "1")], Json(graph.clone())))
                }),
            )
            .route(
                "/v1/workspaces/{workspace}/mutations",
                post(move |Json(body): Json<Value>| {
                    wc.fetch_add(1, Ordering::SeqCst);
                    bc.lock().unwrap().push(body);
                    let code = axum::http::StatusCode::from_u16(
                        writer.write_status.load(Ordering::SeqCst),
                    )
                    .unwrap();
                    let reply = if code.is_success() {
                        writer.has_receipt.store(true, Ordering::SeqCst);
                        committed.clone()
                    } else {
                        json!({"error":"write unavailable"})
                    };
                    std::future::ready((code, [("X-AQ-Graph-API-Version", "1")], Json(reply)))
                }),
            )
            .route(
                "/v1/workspaces/{workspace}/mutations/receipt",
                post(move |Json(body): Json<Value>| {
                    reader.lookups.lock().unwrap().push(body);
                    let code = axum::http::StatusCode::from_u16(
                        reader.lookup_status.load(Ordering::SeqCst),
                    )
                    .unwrap();
                    let reply = if code.is_success() && reader.has_receipt.load(Ordering::SeqCst) {
                        receipt.clone()
                    } else {
                        Value::Null
                    };
                    std::future::ready((code, [("X-AQ-Graph-API-Version", "1")], Json(reply)))
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
            recovery,
            directory,
        }
    }
}
