//! Local-development notification owner qualification. Disposable persisted
//! messages have no endpoint or route, and cannot deliver outside this process.
use crate::common;

use aq_lifecycle_api::Scope;
use aq_server::notifications::persistence::{SendRequest, Service as NotificationService};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
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

fn notification_capability(bootstrap: &Value) -> &Value {
    bootstrap["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|cap| cap["name"] == "notifications.read")
        .unwrap()
}

const OWNER: &str = "owner@localhost";
const VIEWER: &str = "notification-viewer@localhost";

/// A project bound to a fresh Graph workspace.
async fn bound_scope(pool: &PgPool) -> Scope {
    let scope = Scope {
        project_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
    };
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
        .bind(scope.project_id).bind(json!({"enabled":true,"graph_workspace_id":scope.workspace_id}))
        .execute(pool).await.unwrap();
    scope
}

/// Sends one notification as `sender` and checks that it has no external
/// route. Returns its message id.
async fn notify(
    service: &NotificationService,
    sender: Uuid,
    scope: &Scope,
    (recipient, summary): (Uuid, &str),
) -> Uuid {
    let request = SendRequest {
        operation_id: Uuid::new_v4(),
        message_id: Uuid::new_v4(),
        revision: 1,
        scope: scope.clone(),
        recipient_user_id: recipient,
        summary: summary.into(),
        ttl_ms: 60_000,
    };
    let receipt = service.enqueue(sender, request.clone()).await.unwrap();
    assert_eq!(receipt["external_state"], "no_external_route");
    assert_eq!(receipt["delivery_ids"], json!([]));
    request.message_id
}

/// `email`'s mounted inbox holds `expected` alone, its detail has no
/// delivery, and only the mounted router advertises the read.
async fn inbox_shows_only_own_message(
    app: &Router,
    standalone: &Router,
    email: &str,
    expected: Uuid,
) {
    let (status, bootstrap) = read(app, "/api/v1/bootstrap", email).await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    let cap = notification_capability(&bootstrap);
    assert_eq!(cap["enabled"], true);
    assert_eq!(cap["version"], "1");
    let (_, bootstrap) = read(standalone, "/api/v1/bootstrap", email).await;
    assert_eq!(notification_capability(&bootstrap)["enabled"], false);
    let (status, inbox) = read(app, "/api/notifications", email).await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        inbox["items"][0]["message"]["message_id"],
        expected.to_string()
    );
    let (status, detail) = read(app, &format!("/api/notifications/{expected}"), email).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["external_state"], "no_external_route");
    assert_eq!(detail["deliveries"], json!([]));
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn mounted_notifications_read_current_principal_self_without_external_delivery() {
    let f = common::profiled::fixture().await;
    assert!(f.state.config.notifications.is_none());
    let viewer = Uuid::new_v4();
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'notification-viewer@localhost','viewer','dev','notification-viewer@localhost',now())")
        .bind(viewer).execute(&pool).await.unwrap();
    let scope = bound_scope(&pool).await;
    let service = aq_server::notifications::api::service(&f.state);
    service.install(Uuid::new_v4()).await.unwrap();
    let owner_message = (f.actor, "Local fixture: review the deployment plan");
    let owner_message = notify(&service, f.actor, &scope, owner_message).await;
    let viewer_message = (viewer, "Local fixture: viewer private notification");
    let viewer_message = notify(&service, f.actor, &scope, viewer_message).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    assert_eq!(
        read(&standalone, "/api/notifications", OWNER).await.0,
        StatusCode::NOT_FOUND
    );
    for (email, expected) in [(OWNER, owner_message), (VIEWER, viewer_message)] {
        inbox_shows_only_own_message(&app, &standalone, email, expected).await;
    }
    // The viewer reads neither the owner's inbox nor the owner's message.
    for (path, refused) in [
        (
            format!("/api/notifications?recipient={}", f.actor),
            StatusCode::FORBIDDEN,
        ),
        (
            format!("/api/notifications/{owner_message}"),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let (status, body) = read(&app, &path, VIEWER).await;
        assert_eq!(status, refused, "{path}: {body}");
    }
    optional_browser(&f, owner_message).await;
    let mut counts = Vec::new();
    for table in [
        "notification_destinations",
        "notification_deliveries",
        "notification_routes",
    ] {
        counts.push(count(&pool, table).await);
    }
    assert_eq!(counts, [0, 0, 0]);
}

/// Opt-in only: serve this actual persisted notification owner and an explicitly
/// supplied verified bundle. Ordinary tests need no sibling frontend or Node.
async fn optional_browser(f: &common::profiled::Fixture, message: Uuid) {
    let Some(script) = std::env::var_os("AQ_TEST_NOTIFICATION_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_NOTIFICATION_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_NOTIFICATION_DASHBOARD_DIR")
        .expect("explicit notification probe requires a compiled dashboard bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let router = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        None,
        None,
        Arc::new(assets),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut probe = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_NOTIFICATION_ID", message.to_string())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), probe.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded actual notification browser probe")
            .unwrap()
            .success()
    );
}
