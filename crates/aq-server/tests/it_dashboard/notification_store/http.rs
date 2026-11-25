use super::*;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;
pub(super) async fn writes(app: &axum::Router, request: &SendRequest) {
    let raw = serde_json::to_string(request).unwrap();
    let post = |email: &str, csrf: bool| {
        let mut b = Request::post("/api/notifications")
            .header("content-type", "application/json")
            .header("x-aq-dev-email", email);
        if csrf {
            b = b
                .header("x-aq-request", "1")
                .header("origin", "https://aq.example");
        }
        b.body(Body::from(raw.clone())).unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(post("notify-owner@localhost", false))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.clone()
            .oneshot(post("recipient@localhost", true))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .clone()
        .oneshot(post("notify-owner@localhost", true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = serde_json::from_slice::<serde_json::Value>(
        &response.into_body().collect().await.unwrap().to_bytes(),
    )
    .unwrap();
    assert_eq!(body["external_state"], "no_external_route");
}

pub(super) async fn reads(
    app: axum::Router,
    state: aq_server::AppState,
    owner: Uuid,
    message: Uuid,
) {
    let get = |path: String, email: &str| {
        Request::get(path)
            .header("x-aq-dev-email", email)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(get(
                format!("/api/notifications/{}", message),
                "recipient@localhost"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(get(
                format!("/api/notifications?recipient={}", owner),
                "recipient@localhost"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        aq_server::build_worker_router(state)
            .oneshot(get("/api/notifications".into(), "notify-owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}
