//! HTTP boundary checks require only real node credentials, not fabricated authority.
mod common;
use aq_server::{profiled_execution::phase_terminal::routes, store::Store};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;
const VERSION: &str = "aq-phase-terminal-api-version";
fn request(path: &str, credential: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut r = Request::builder()
        .method("POST")
        .uri(path)
        .header(VERSION, "1")
        .header("content-type", "application/json");
    if let Some(c) = credential {
        r = r.header("authorization", format!("Bearer {c}"))
    }
    r.body(Body::from(body)).unwrap()
}
async fn expect(app: &Router, request: Request<Body>, status: StatusCode) {
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[VERSION], "1");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(to_bytes(response.into_body(), 65536).await.unwrap().len() < 65536);
}
#[tokio::test]
async fn exact_single_version_and_json_type_required_before_owner_dispatch() {
    let f = common::profiled::fixture().await;
    let app = routes::routes(f.state.clone()).with_state(f.state.clone());
    let path = "/v1/worker/profiled-v2/history";
    for (name, value) in [(VERSION, "2"), ("content-type", "text/plain")] {
        let mut r = request(path, Some(&f.credential), b"{}".to_vec());
        r.headers_mut().insert(name, value.parse().unwrap());
        expect(&app, r, StatusCode::BAD_REQUEST).await;
    }
    for name in [VERSION, "content-type"] {
        let mut r = request(path, Some(&f.credential), b"{}".to_vec());
        let v = r.headers()[name].clone();
        r.headers_mut().append(name, v);
        expect(&app, r, StatusCode::BAD_REQUEST).await;
    }
    let mut missing = request(path, Some(&f.credential), b"{}".to_vec());
    missing.headers_mut().remove(VERSION);
    expect(&app, missing, StatusCode::BAD_REQUEST).await;
}
#[tokio::test]
async fn current_bearer_and_inclusive_body_limit_are_enforced_on_all_reports() {
    let f = common::profiled::fixture().await;
    let app = routes::routes(f.state.clone()).with_state(f.state.clone());
    for path in ["started", "stopped", "launch-closure", "history"] {
        let path = format!("/v1/worker/profiled-v2/{path}");
        expect(
            &app,
            request(&path, None, b"{}".to_vec()),
            StatusCode::UNAUTHORIZED,
        )
        .await;
        expect(
            &app,
            request(&path, Some("aqn_invalid"), b"{}".to_vec()),
            StatusCode::UNAUTHORIZED,
        )
        .await;
        expect(
            &app,
            request(&path, Some(&f.credential), vec![b' '; 65537]),
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
        expect(
            &app,
            request(&path, Some(&f.credential), vec![b' '; 65536]),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
}
#[tokio::test]
async fn cleanup_authentication_uses_control_connections_with_general_saturated() {
    let f = common::profiled::fixture().await;
    let general = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(f.db.options())
        .await
        .unwrap();
    let mut state = f.state.clone();
    state.store = Store::from_pool(general.clone()).await.unwrap();
    let app = routes::routes(state.clone()).with_state(state);
    let held = general.acquire().await.unwrap();
    let blocked = app.clone().oneshot(request(
        "/v1/worker/profiled-v2/started",
        Some(&f.credential),
        b"{}".to_vec(),
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), blocked)
            .await
            .is_err()
    );
    for path in ["stopped", "launch-closure"] {
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            expect(
                &app,
                request(
                    &format!("/v1/worker/profiled-v2/{path}"),
                    Some(&f.credential),
                    b"{}".to_vec(),
                ),
                StatusCode::BAD_REQUEST,
            ),
        )
        .await
        .expect("cleanup credential lookup was blocked by General pool");
    }
    drop(held);
}
