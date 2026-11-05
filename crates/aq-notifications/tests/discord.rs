use aq_notifications::{FailureReason, Payload, TransportOutcome, discord::*};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Peer {
    mode: u8,
    calls: AtomicUsize,
}
async fn handler(State(peer): State<Arc<Peer>>, request: Request<Body>) -> Response {
    peer.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        request.uri().path(),
        "/api/v10/webhooks/42/fixture_token_1234567890"
    );
    assert_eq!(request.uri().query(), Some("wait=true"));
    assert!(request.headers().get("authorization").is_none());
    let bytes = axum::body::to_bytes(request.into_body(), 8192)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["allowed_mentions"]["parse"], json!([]));
    assert_eq!(value["flags"], 4);
    assert!(value.get("attachments").is_none());
    match peer.mode {
        0 => (
            StatusCode::OK,
            axum::Json(
                json!({"id":"12345","webhook_id":"42","content":"private provider echo ignored"}),
            ),
        )
            .into_response(),
        1 => (
            StatusCode::BAD_GATEWAY,
            "private token must never appear in result",
        )
            .into_response(),
        2 => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.1:1/stolen")],
            "",
        )
            .into_response(),
        3 => (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "2.1")],
            axum::Json(json!({"retry_after":1.25,"global":false})),
        )
            .into_response(),
        4 => (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({"retry_after":0.0011,"global":true})),
        )
            .into_response(),
        5 => (
            StatusCode::OK,
            axum::Json(json!({"id":"12345","webhook_id":"99"})),
        )
            .into_response(),
        6 => (StatusCode::OK, "x".repeat(65537)).into_response(),
        7 => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"message":"provider private details"})),
        )
            .into_response(),
        8 => (StatusCode::NO_CONTENT, "").into_response(),
        9 => (
            StatusCode::OK,
            [
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset-after", "1.001"),
            ],
            axum::Json(json!({"id":"12345","webhook_id":"42"})),
        )
            .into_response(),
        _ => (
            StatusCode::OK,
            r#"{"id":"12345","id":"5678","webhook_id":"42"}"#,
        )
            .into_response(),
    }
}
async fn exchange(mode: u8) -> Exchange {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/v10/webhooks/42/fixture_token_1234567890",
        listener.local_addr().unwrap()
    );
    let peer = Arc::new(Peer {
        mode,
        calls: AtomicUsize::new(0),
    });
    let app = Router::new().fallback(handler).with_state(peer.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let endpoint = Webhook::loopback_fixture(&url).unwrap();
    let result = Discord::new()
        .unwrap()
        .send(
            &endpoint,
            &Payload {
                content: "@everyone safe mentions disabled".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    task.abort();
    result
}
#[tokio::test]
async fn acceptance_requires_real_receipt_association_not_status_alone() {
    assert_eq!(
        exchange(0).await.outcome,
        TransportOutcome::Accepted {
            message_id: "12345".into()
        }
    );
    for mode in [1, 2, 5, 6, 8, 10] {
        let result = exchange(mode).await;
        assert_eq!(result.outcome, TransportOutcome::Unknown);
        assert!(!format!("{result:?}").contains("private"));
    }
    assert_eq!(
        exchange(7).await.outcome,
        TransportOutcome::Permanent(FailureReason::DestinationUnavailable)
    );
}
#[tokio::test]
async fn provider_delays_round_up_and_success_can_throttle_next_delivery() {
    let result = exchange(3).await;
    assert_eq!(
        result.outcome,
        TransportOutcome::RateLimited {
            retry_after_ms: 2100,
            global: false
        }
    );
    assert_eq!(
        result.throttle,
        Some(Throttle {
            after_ms: 2100,
            scope: ThrottleScope::Destination
        })
    );
    let result = exchange(4).await;
    assert_eq!(
        result.throttle,
        Some(Throttle {
            after_ms: 2,
            scope: ThrottleScope::Provider
        })
    );
    let result = exchange(9).await;
    assert!(matches!(result.outcome, TransportOutcome::Accepted { .. }));
    assert_eq!(
        result.throttle,
        Some(Throttle {
            after_ms: 1001,
            scope: ThrottleScope::Destination
        })
    );
}
#[test]
fn protected_endpoint_is_fixed_origin_path_and_has_no_arbitrary_query_or_redirect() {
    assert!(
        Webhook::parse("https://discord.com/api/v10/webhooks/42/fixture_token_1234567890").is_ok()
    );
    // The form Discord issues is accepted and pinned to v10.
    assert!(Webhook::parse("https://discord.com/api/webhooks/42/fixture_token_1234567890").is_ok());
    for value in [
        "https://discord.com/api/v9/webhooks/42/fixture_token_1234567890",
        "https://discord.com/api/webhooks/42/fixture_token_1234567890/other",
        "http://discord.com/api/v10/webhooks/42/fixture_token_1234567890",
        "https://evil.example/api/v10/webhooks/42/fixture_token_1234567890",
        "https://discord.com/api/v10/webhooks/42/fixture_token_1234567890?wait=false",
        "https://user:secret@discord.com/api/v10/webhooks/42/fixture_token_1234567890",
        "https://discord.com/api/v10/webhooks/42/fixture_token_1234567890/other",
        "http://127.0.0.1:1234/api/v10/webhooks/42/fixture_token_1234567890",
    ] {
        assert!(Webhook::parse(value).is_err());
    }
}
