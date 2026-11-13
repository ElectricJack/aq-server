//! A local stand-in for a provider's responses API, for internal work tests:
//! no real provider is contacted.
use axum::{Router, extract::State, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Seen {
    pub authorization: Vec<String>,
    pub bodies: Vec<Value>,
}

/// Answers every request with `status` and `reply`; returns its endpoint.
pub async fn serve(status: u16, reply: Value) -> (String, Arc<Mutex<Seen>>) {
    serve_each(status, vec![reply]).await
}

/// Answers the n-th request with `replies[n]`, and every later one with the
/// last; returns its endpoint.
pub async fn serve_each(status: u16, replies: Vec<Value>) -> (String, Arc<Mutex<Seen>>) {
    assert!(!replies.is_empty());
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new()
        .route(
            "/v1/responses",
            post(
                move |State(seen): State<Arc<Mutex<Seen>>>,
                      headers: HeaderMap,
                      axum::Json(body): axum::Json<Value>| {
                    let replies = replies.clone();
                    async move {
                        let mut seen = seen.lock().unwrap();
                        seen.authorization.push(
                            headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned(),
                        );
                        seen.bodies.push(body);
                        let reply = &replies[(seen.bodies.len() - 1).min(replies.len() - 1)];
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            axum::Json(reply.clone()),
                        )
                    }
                },
            ),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1/responses"), seen)
}

pub fn completed(text: &str) -> Value {
    json!({
        "status": "completed",
        "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}],
        "usage": {"input_tokens": 12, "output_tokens": 5}
    })
}
