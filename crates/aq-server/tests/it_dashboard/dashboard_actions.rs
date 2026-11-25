mod pending;
mod sessions;
mod supervisor;

use crate::common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::fixture;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn action_union_rejects_unknown_kind_with_envelope() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .oneshot(
            Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"operation_id":Uuid::new_v4(),"kind":"shell.run",
            "subject":{"type":"project","id":Uuid::new_v4().to_string()},
            "expected_versions":{},"payload":{}})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = json_body(response).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "unknown_action_kind");
    assert!(body["request_id"].is_string());
    assert_eq!(body["retryable"], false);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn v1_outer_rejections_and_validation_use_common_safe_errors() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    for (path, method, body, csrf, wanted) in [
        ("/api/v1/actions", "POST", "{}", false, 403),
        ("/api/v1/actions", "POST", "{", true, 422),
        ("/api/v1/actions", "POST", "{}", true, 422),
        ("/api/v1/operations/not-a-uuid", "GET", "", true, 422),
        ("/api/v1/missing", "GET", "", true, 404),
        ("/api/v1/sessions?cursor=bad", "GET", "", true, 410),
    ] {
        let mut request = Request::builder()
            .uri(path)
            .method(method)
            .header("x-aq-dev-email", "owner@localhost")
            .header("content-type", "application/json");
        if csrf {
            request = request.header("x-aq-request", "1");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), wanted, "{path}");
        let body = json_body(response).await;
        assert!(body["request_id"].is_string(), "{path}: {body}");
        assert!(body["code"].is_string(), "{path}: {body}");
        assert!(body["retryable"].is_boolean());
        assert!(body["message"].as_str().unwrap().len() <= 256);
    }
}

#[tokio::test]
async fn production_registry_is_enabled_and_lookup_is_neutral() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/bootstrap")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = json_body(response).await;
    let capability = body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "dashboard.actions")
        .unwrap();
    assert_eq!(capability["enabled"], true);
    assert_eq!(capability["operation_retention"]["minimum_days"], 90);
    assert_eq!(
        capability["operation_retention"]["unresolved_indefinite"],
        true
    );
    let response = app
        .oneshot(
            Request::get(format!("/api/v1/operations/{}", Uuid::new_v4()))
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = json_body(response).await;
    assert_eq!(body["code"], "not_found");
    assert!(body.get("operation_id").is_none());
}

async fn call(
    app: &axum::Router,
    path: &str,
    body: Option<&Value>,
    email: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("x-aq-dev-email", email)
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.map(Value::to_string).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = json_body(response).await;
    (status, body)
}
#[tokio::test]
async fn unknown_fields_duplicate_keys_and_oversized_bodies_are_rejected_before_admission() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let base = json!({"operation_id":Uuid::new_v4(),"kind":"test.effect","subject":{"type":"test","id":"one"},"expected_versions":{},"payload":{}});
    let mut extra = base.clone();
    extra["command"] = json!("never");
    for (raw, status) in [
        (extra.to_string(), 422),
        (
            format!("{{\"kind\":\"test.effect\",{}", &base.to_string()[1..]),
            422,
        ),
        (" ".repeat(65537), 413),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/api/v1/actions")
                    .header("x-aq-dev-email", "owner@localhost")
                    .header("x-aq-request", "1")
                    .header("content-type", "application/json")
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), status);
        let body = json_body(response).await;
        assert!(body["request_id"].is_string());
        assert!(body["message"].as_str().unwrap().len() <= 256);
        assert!(body.get("operation_id").is_none());
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

async fn submit(app: &axum::Router, action: &Value) -> (StatusCode, Value) {
    call(app, "/api/v1/actions", Some(action), "owner@localhost").await
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    call(app, path, None, "owner@localhost").await
}
