//! Bootstrap capabilities track actual mounted owners and current human roles.
mod common;
use aq_server::{
    agent_access::{AgentAccessConfig, Runtime},
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
async fn read(app: &Router, path: &str, email: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
fn cap(body: &Value, name: &str) -> bool {
    body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == name)
        .unwrap()["enabled"]
        .as_bool()
        .unwrap()
}
#[tokio::test]
async fn mounted_routes_and_absent_owners_have_granular_truthful_capabilities() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, b) = read(&app, "/api/v1/bootstrap", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    for name in [
        "human.graph.read",
        "human.overview.tasks.read",
        "tasks.read",
        "knowledge.read",
        "decisions.read",
        "decisions.resolve",
        "playbooks.inspect",
    ] {
        assert!(!cap(&b, name), "{name}")
    }
    assert!(cap(&b, "search.read"));
    assert!(cap(&b, "results.read"));
    assert!(cap(&b, "results.comment"));
    assert!(cap(&b, "results.feedback"));
    assert!(cap(&b, "session.events.read"));
    let binding = aq_admission::transport::lifecycle_binding(&f.profiled.offer).unwrap();
    let events_path = format!("/api/observations/sessions/{}/events", binding.attempt_id);
    let (status, events) = read(&app, &events_path, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let events = aq_observation_api::decode_events(&serde_json::to_vec(&events).unwrap()).unwrap();
    assert!(events.events.is_empty());
    assert!(events.complete_for_snapshot);
    assert_eq!(events.binding, binding);
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    let (_, standalone_bootstrap) = read(&standalone, "/api/v1/bootstrap", "owner@localhost").await;
    assert!(!cap(&standalone_bootstrap, "session.events.read"));
    assert!(!cap(&standalone_bootstrap, "results.comment"));
    assert!(!cap(&standalone_bootstrap, "results.feedback"));
    assert_eq!(
        read(&app, "/api/human/results", "owner@localhost").await.0,
        StatusCode::OK
    );
    assert_eq!(
        read(
            &app,
            &format!(
                "/api/human/projects/{}/tasks",
                f.selection.lineage.project_id
            ),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE,
        "Graph router is mounted, runtime absent"
    );
    assert_eq!(
        read(&app, "/api/reviews/decisions", "owner@localhost")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        read(&app, "/api/playbooks/status", "owner@localhost")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn configured_graph_and_injected_review_owner_are_visible_only_to_owner() {
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("graph-token");
    std::fs::write(&token, "dashboard-mount-fixture-service-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: "http://127.0.0.1:9".into(),
            graph_token_file: token,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    let (_, standalone_bootstrap) = read(&standalone, "/api/v1/bootstrap", "owner@localhost").await;
    for name in [
        "human.graph.read",
        "human.overview.tasks.read",
        "tasks.read",
        "knowledge.read",
        "results.read",
    ] {
        assert!(
            !cap(&standalone_bootstrap, name),
            "standalone dashboard cannot advertise absent router: {name}"
        );
    }
    assert!(cap(&standalone_bootstrap, "search.read"));
    let key = dir.path().join("review-key");
    std::fs::write(&key, [23u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Arc::new(
        Keyring::load(&ProtectionConfig {
            active_key_id: "test".into(),
            keys: vec![KeyFile {
                id: "test".into(),
                file: key,
            }],
        })
        .unwrap(),
    );
    let reviews = Arc::new(aq_server::reviews::Service::new(f.state.clone(), keys));
    let app =
        aq_server::build_admin_router_with_policy_owners(f.state.clone(), None, Some(reviews));
    let (status, b) = read(&app, "/api/v1/bootstrap", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    for name in [
        "human.graph.read",
        "human.overview.tasks.read",
        "tasks.read",
        "knowledge.read",
        "decisions.read",
        "decisions.resolve",
        "results.read",
    ] {
        assert!(cap(&b, name), "{name}: {b}")
    }
    assert!(cap(&b, "search.read"));
    assert!(!cap(&b, "playbooks.inspect"));
    assert_eq!(
        read(&app, "/api/reviews/decisions", "owner@localhost")
            .await
            .0,
        StatusCode::OK
    );
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(viewer).execute(&f.db.pool().await).await.unwrap();
    let (_, b) = read(&app, "/api/v1/bootstrap", "viewer@localhost").await;
    assert!(cap(&b, "search.read"));
    for name in [
        "human.graph.read",
        "human.overview.tasks.read",
        "tasks.read",
        "knowledge.read",
        "decisions.read",
        "decisions.resolve",
        "results.read",
        "results.comment",
        "results.feedback",
        "playbooks.inspect",
    ] {
        assert!(!cap(&b, name), "{name}")
    }
    assert_eq!(
        read(&app, "/api/human/results", "viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(
            &app,
            &format!(
                "/api/human/projects/{}/tasks",
                f.selection.lineage.project_id
            ),
            "viewer@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn unsupported_saved_graph_adapter_is_refused_before_upstream_io() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let graph = axum::Router::new().fallback(move || {
        let observed = observed.clone();
        async move {
            observed.fetch_add(1, Ordering::SeqCst);
            StatusCode::SERVICE_UNAVAILABLE
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, graph).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "graph-adapter-scope-fixture-service-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: origin,
            graph_token_file: token,
            mutation_actor_id: None,
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let project = f.selection.lineage.project_id;
    let pool = f.db.pool().await;
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{graph_adapter_reference}','\"other.graph.adapter\"'::jsonb),revision=revision+1 WHERE project_id=$1").bind(project).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!("/api/human/projects/{project}/tasks");
    assert_eq!(
        read(&app, &path, "owner@localhost").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "unsupported adapter must not read configured Graph"
    );
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{graph_adapter_reference}','\"aq.graph.configured\"'::jsonb),revision=revision+1 WHERE project_id=$1").bind(project).execute(&pool).await.unwrap();
    assert_eq!(
        read(&app, &path, "owner@localhost").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "supported adapter reaches the configured upstream"
    );
    server.abort();
}
