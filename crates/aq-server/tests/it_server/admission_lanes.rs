//! Each admission lane is sized from the server's launch limits (observation lanes included).
use aq_machine_control::limits::LaunchLimits;
use aq_server::admission::{Admission, gate};
use axum::http::StatusCode;
use axum::{Router, body::Body, http::Request as HttpRequest, middleware, routing::post};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

async fn refused_after(limits: LaunchLimits, path: &'static str, allowed: usize) {
    let admission = Arc::new(Admission::new(&limits));
    let release = Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    let app = Router::new()
        .route(
            path,
            post(move || {
                let held = held.clone();
                async move { held.notified().await }
            }),
        )
        .layer(middleware::from_fn_with_state(admission, gate));
    let call = || {
        let app = app.clone();
        tokio::spawn(async move {
            let request = HttpRequest::post(path).body(Body::empty()).unwrap();
            app.oneshot(request).await.unwrap().status()
        })
    };
    let held: Vec<_> = (0..allowed).map(|_| call()).collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(call().await.unwrap(), StatusCode::SERVICE_UNAVAILABLE);
    release.notify_waiters();
    for task in held {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}
#[tokio::test]
async fn observation_lanes_are_sized_from_the_limits() {
    let limits = LaunchLimits {
        observation_requests: 3,
        observation_read_requests: 1,
        ..LaunchLimits::DEFAULT
    };
    refused_after(limits, "/v1/worker/observations/batch", 3).await;
    refused_after(limits, "/api/observations/x", 1).await;
    assert_eq!(LaunchLimits::DEFAULT.observation_requests, 2);
    assert_eq!(LaunchLimits::DEFAULT.observation_read_requests, 2);
}
