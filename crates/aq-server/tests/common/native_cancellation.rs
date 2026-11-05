//! Owner HTTP cancellation helper; no fabricated cancellation authority.
use aq_server::AppState;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

pub async fn cancel(
    state: &AppState,
    attempt: Uuid,
    operation: Uuid,
    reason: &str,
) -> (StatusCode, Value) {
    cancel_as(state, attempt, operation, reason, "owner@localhost").await
}
pub async fn cancel_as(
    state: &AppState,
    attempt: Uuid,
    operation: Uuid,
    reason: &str,
    email: &str,
) -> (StatusCode, Value) {
    let response = aq_server::build_admin_router(state.clone())
        .oneshot(
            Request::post(format!("/api/execution/attempts/{attempt}/cancel"))
                .header("content-type", "application/json")
                .header("x-aq-dev-email", email)
                .header("X-AQ-Request", "1")
                .body(Body::from(
                    json!({"operation_id":operation,"reason":reason}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
