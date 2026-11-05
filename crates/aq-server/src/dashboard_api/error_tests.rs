use super::*;

#[derive(Clone)]
struct LogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn redacted_refusals_log_the_returned_request_id_and_safe_source_class() {
    use axum::{Router, body::Body, http::Request, middleware::from_fn, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let log = LogWriter(Default::default());
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let app = Router::new()
        .route(
            "/api/v1/invalid",
            get(|| async { crate::error::AppError::BadRequest("PRIVATE_BODY_SENTINEL".into()) }),
        )
        .route(
            "/api/v1/internal",
            get(|| async { crate::error::AppError::Internal }),
        )
        .layer(from_fn(normalize_errors));
    for (path, expected, source) in [
        ("invalid", 422, "bad_request"),
        ("internal", 503, "internal"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/{path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let logs = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        let line = logs
            .lines()
            .find(|line| line.contains(body["request_id"].as_str().unwrap()))
            .expect("response ID has a matching refusal log");
        assert!(line.contains(source), "{line}");
        assert!(!logs.contains("PRIVATE_BODY_SENTINEL"));
        assert!(!body.to_string().contains("PRIVATE_BODY_SENTINEL"));
    }
}
#[tokio::test]
async fn common_error_envelope_bounds_strings_and_covers_required_statuses() {
    use http_body_util::BodyExt;
    for status in [401, 403, 404, 409, 410, 413, 422, 426, 429, 503] {
        let response = ApiError::for_status(StatusCode::from_u16(status).unwrap()).into_response();
        assert_eq!(response.status().as_u16(), status);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert!(body["request_id"].is_string());
        assert!(body["message"].as_str().unwrap().len() <= 256);
        assert!(body["code"].as_str().unwrap().len() <= 64);
        assert!(body["retryable"].is_boolean());
    }
}

#[tokio::test]
async fn common_error_normalization_preserves_protocol_clock_and_retry_headers() {
    use axum::{Router, body::Body, http::Request, middleware::from_fn, routing::get};
    use tower::ServiceExt;
    let app = Router::new()
        .route(
            "/api/v1/example",
            get(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("retry-after", "1"), ("aq-terminal-server-time-ms", "123")],
                    "private diagnostic",
                )
            }),
        )
        .layer(from_fn(normalize_errors));
    let response = app
        .oneshot(Request::get("/api/v1/example").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    assert_eq!(
        response
            .headers()
            .get("aq-terminal-server-time-ms")
            .unwrap(),
        "123"
    );
}
