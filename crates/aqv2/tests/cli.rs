//! Boundary tests: real CLI translation and HTTP transport, pinned owner fixtures.
use aq_human_http::{Authentication, Client, Limits};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

#[path = "cli/channel.rs"]
mod channel;
#[path = "cli/result.rs"]
mod result;

struct Seen {
    method: String,
    path: String,
    headers: axum::http::HeaderMap,
    body: Value,
}
struct Server {
    origin: String,
    client: Client,
    seen: mpsc::UnboundedReceiver<Seen>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn next(&mut self) -> Seen {
        tokio::time::timeout(Duration::from_secs(1), self.seen.recv())
            .await
            .expect("CLI did not send expected request")
            .unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
struct Responses {
    values: Arc<Mutex<VecDeque<(StatusCode, Value)>>>,
    seen: mpsc::UnboundedSender<Seen>,
}
async fn handle(State(state): State<Responses>, request: Request) -> impl IntoResponse {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 65537).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    let request_id = body["operation_id"].clone();
    state
        .seen
        .send(Seen {
            method: parts.method.to_string(),
            path: parts.uri.to_string(),
            headers: parts.headers,
            body,
        })
        .unwrap();
    let mut response = state.values.lock().unwrap().pop_front().unwrap_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"private":"must not leak"}),
    ));
    if response.1["data"]["operation_id"] == "echo-request-id" {
        response.1["data"]["operation_id"] = request_id;
    }
    (response.0, axum::Json(response.1))
}
async fn server(responses: Vec<(StatusCode, Value)>) -> Server {
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().fallback(any(handle)).with_state(Responses {
        values: Arc::new(Mutex::new(responses.into())),
        seen: tx,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = Client::new(
        &origin,
        Authentication::LoopbackDevelopmentEmail("owner@example.test".into()),
        Limits::default(),
    )
    .unwrap();
    Server {
        origin,
        client,
        seen: rx,
        task,
    }
}
fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-actions/v1/fixtures")
        .join(name);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn confirmed(id: &str) -> Value {
    let mut value = fixture("operation-confirmed.json");
    value["data"]["operation_id"] = json!(id);
    value
}

#[path = "cli/actions.rs"]
mod actions;
#[path = "cli/binary.rs"]
mod binary;
#[path = "cli/flags.rs"]
mod flags;
#[path = "cli/reads.rs"]
mod reads;
#[path = "cli/receipts.rs"]
mod receipts;
