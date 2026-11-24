use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
#[tokio::test]
async fn media_upload_disabled_without_store_is_explicit() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/human/results/{}/revisions/{}/media-uploads",
        Uuid::new_v4(),
        "a".repeat(64)
    );
    let (status, body) = send(&app, "POST", &path, json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "media_store_unconfigured");
    let (_, bootstrap) = send(&app, "GET", "/api/v1/bootstrap", Value::Null).await;
    let cap = bootstrap["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "results.media.upload")
        .unwrap();
    assert_eq!(cap["enabled"], false);
    assert_eq!(cap["reason"], "Result media store is not configured");
}
#[path = "results_feedback/evidence.rs"]
mod evidence;
#[path = "results_feedback/following.rs"]
mod following;
#[path = "results_feedback/graph_tasks.rs"]
mod graph_tasks;
#[path = "results_feedback/threads.rs"]
mod threads;
