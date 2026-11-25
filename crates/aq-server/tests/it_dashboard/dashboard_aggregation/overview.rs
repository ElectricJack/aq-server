use super::*;

#[tokio::test]
async fn overview_empty_and_bounded_project_sources_are_truthful() {
    use serde_json::json;
    use uuid::Uuid;
    let f = common::profiled::fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, empty) = read(&app, "/api/v1/overview").await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["data"]["counts"]["running"]["value"], 0);
    assert_eq!(empty["data"]["counts"]["running"]["accuracy"], "exact");
    assert!(empty["data"]["counts"]["ready"]["value"].is_null());
    assert!(empty["data"]["counts"]["needs_you"]["value"].is_null());
    // The fixture's enrolled worker has never sent a heartbeat (SO-06).
    assert!(
        empty["data"]["attention"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["kind"] == "worker_offline" && v["project"].is_null()),
        "{empty}"
    );
    common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    for _ in 0..9 {
        let project = Uuid::new_v4();
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $1,revision,jsonb_set(binding,'{project_id}',$2),validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$3").bind(project).bind(json!(project)).bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    }
    let (_, bounded) = read(&app, "/api/v1/overview").await;
    assert_eq!(bounded["data"]["in_motion"].as_array().unwrap().len(), 8);
    assert_eq!(
        bounded["data"]["counts"]["running"]["accuracy"],
        "estimated"
    );
    assert!(
        bounded["data"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["source"] == "projects"
                && s["coverage"]["reason"] == "project_sample_truncated")
    );
}

#[tokio::test]
async fn overview_ready_owner_preserves_samples_failures_and_binding_fence() {
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let mode = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let [node, claimed] = ready_nodes(&f);
    let server_mode = mode.clone();
    let reached = entered.clone();
    let proceed = release.clone();
    let graph = Router::new().route(
        "/v1/workspaces/{workspace}/ready",
        axum::routing::get(move || {
            let nodes = [node.clone(), claimed.clone()];
            let mode = server_mode.clone();
            let reached = reached.clone();
            let proceed = proceed.clone();
            async move {
                let mode = mode.load(Ordering::SeqCst);
                if mode == 3 {
                    reached.notify_one();
                    proceed.notified().await;
                }
                (
                    if mode == 2 {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::OK
                    },
                    [("X-AQ-Graph-API-Version", "1")],
                    axum::Json(
                        json!({"items":nodes,"next_cursor":if mode==1 {Some("next")} else {None}}),
                    ),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, graph).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    attach_graph(&mut f, origin, dir.path()).await;
    let app = aq_server::build_admin_router(f.state.clone());
    assert_ready_samples(&app, &mode).await;
    mode.store(2, Ordering::SeqCst);
    let (_, body) = read(&app, "/api/v1/overview").await;
    assert!(body["data"]["counts"]["ready"]["value"].is_null());
    assert_eq!(body["data"]["counts"]["ready"]["accuracy"], "unknown");
    mode.store(3, Ordering::SeqCst);
    let pending_app = app.clone();
    let pending = tokio::spawn(async move { read(&pending_app, "/api/v1/overview").await });
    entered.notified().await;
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(f.selection.lineage.project_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    release.notify_one();
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
    server.abort();
}

fn ready_nodes(f: &common::profiled::Fixture) -> [Value; 2] {
    use serde_json::json;
    let workspace = f.profiled.graph.workspace_id;
    let mut node: Value = serde_json::from_str::<Value>(include_str!(
        "../../../../../protocol/human-graph/v1/examples/task.json"
    ))
    .unwrap()["data"]
        .clone();
    // `role` is AQ's projection (spec 0014 §5.5), not a Graph field.
    node.as_object_mut().unwrap().remove("role");
    node["workspace_id"] = json!(workspace);
    node["metadata"] = json!({});
    node["version"] = json!(1);
    node["content_revision"] = json!(1);
    let mut claimed = node.clone();
    claimed["id"] = json!(f.selection.lineage.task_id);
    node["id"] = json!(uuid::Uuid::new_v4());
    [node, claimed]
}

async fn assert_ready_samples(app: &Router, mode: &std::sync::atomic::AtomicUsize) {
    use std::sync::atomic::Ordering;
    for (value, accuracy) in [(0, "exact"), (1, "estimated")] {
        mode.store(value, Ordering::SeqCst);
        let (status, body) = read(app, "/api/v1/overview").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["counts"]["ready"]["value"], 1, "{body}");
        assert_eq!(body["data"]["counts"]["ready"]["accuracy"], accuracy);
        // The active Graph task is ready; it does not become a running attempt.
        assert_eq!(body["data"]["counts"]["running"]["value"], 0);
        assert!(
            !body["data"]["attention"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["kind"] == "blocked")
        );
        assert!(
            body["data"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["source"] == "graph_ready"
                    && v["total"]["value"] == 1
                    && v["observation"]["observed_at"] == "1970-01-01T00:16:40.000Z")
        );
    }
}

async fn attach_graph(f: &mut common::profiled::Fixture, origin: String, dir: &std::path::Path) {
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    let token = dir.join("token");
    std::fs::write(&token, "s".repeat(32)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: origin,
            graph_token_file: token,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
}
