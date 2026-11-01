//! Actual output-only terminal registration, human Watch and retained literal tail.
use super::*;
use aq_session_protocol::Validate;
use aq_terminal_relay::api::{MutationReply, TerminalInfo};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

pub(super) async fn configure(case: &mut Case) {
    use aq_server::terminal::{
        config::Loaded,
        protection::{KeyFile, Keyring, ProtectionConfig},
    };
    let path = case.env.env._dir.path().join("actual-terminal-output-key");
    std::fs::write(&path, [41u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut config = (*case.env.env.state.config).clone();
    config.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "actual-output-test".into(),
            keys: vec![KeyFile {
                id: "actual-output-test".into(),
                file: path,
            }],
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    case.env.env.state.config = Arc::new(config);
    aq_server::terminal::persistence::install(&case.env.env.state)
        .await
        .unwrap();
}
async fn human(case: &Case, path: &str, body: Option<Value>) -> (axum::http::HeaderMap, Value) {
    let state = case.env.env.state.clone();
    let mut request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("x-aq-dev-email", "owner@localhost");
    if body.is_some() {
        request = request
            .header("content-type", "application/json")
            .header("x-aq-request", "1")
            .header("origin", &state.config.server.public_origin);
    }
    let response = aq_server::build_admin_router(state)
        .oneshot(
            request
                .body(Body::from(
                    body.map(|v| serde_json::to_vec(&v).unwrap())
                        .unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 524288).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "human route {path}: {}",
        String::from_utf8_lossy(&bytes)
    );
    (headers, serde_json::from_slice(&bytes).unwrap())
}
pub(super) struct Watch {
    info: TerminalInfo,
    subscription: Uuid,
}
pub(super) async fn watch(case: &Case, ready: &Value) -> Watch {
    assert_eq!(ready["phase"], "terminal_registered");
    let info: TerminalInfo = serde_json::from_value(ready["terminal_info"].clone()).unwrap();
    assert!(!info.terminal_policy.input_enabled);
    assert!(info.control.lease.is_none());
    let path = format!("/api/v1/terminals/{}", info.terminal.terminal_id);
    let (headers, actual) = human(case, &path, None).await;
    assert_eq!(
        actual["terminal"],
        serde_json::to_value(&info.terminal).unwrap()
    );
    assert_eq!(actual["session_revision"], json!(info.session_revision));
    let now: u64 = headers["aq-terminal-server-time-ms"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let subscription = Uuid::new_v4();
    let request = aq_server::terminal::service::Mutation::Watch(aq_session_api::WatchRequest {
        terminal: info.terminal.clone(),
        operation_id: subscription,
        expected_session_revision: info.session_revision,
        expires_at_ms: now + 15000,
        after_sequence: None,
    });
    let (_, response) = human(case, &format!("{path}/mutations"), Some(json!(request))).await;
    let MutationReply::Watch {
        subscription_id, ..
    } = serde_json::from_value(response).unwrap()
    else {
        panic!("actual Watch receipt required");
    };
    assert_eq!(subscription_id, subscription);
    Watch { info, subscription }
}
pub(super) async fn verify(case: &Case, watched: &Watch, result: &Value) {
    let path = format!(
        "/api/v1/terminals/{}/tail?subscription_id={}&after_sequence=0",
        watched.info.terminal.terminal_id, watched.subscription
    );
    let (_, response) = human(case, &path, None).await;
    let tail: aq_server::terminal::tail::TailReply = serde_json::from_value(response).unwrap();
    assert_eq!(tail.terminal, watched.info.terminal);
    assert_eq!(tail.subscription_id, watched.subscription);
    assert!(tail.initial_incomplete && tail.disconnected);
    assert_eq!(
        tail.availability,
        aq_server::terminal::tail::Availability::ResidentTail
    );
    assert!(tail.last_observed_at_ms.is_some());
    assert!(!tail.frames.is_empty());
    let mut bytes = Vec::new();
    for frame in &tail.frames {
        frame.validate().unwrap();
        assert_eq!(frame.terminal, watched.info.terminal);
        bytes.extend_from_slice(&frame.bytes);
    }
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("synthetic native fixture output tick")
    );
    assert_eq!(
        result["phase_output"]["terminal_info"]["terminal"],
        json!(tail.terminal)
    );
    assert!(result["phase_output"]["upload_ack"].is_object());
    assert!(tail.next_sequence > 0);
    eprintln!(
        "actual worker output: authenticated registration, human Watch, literal frames and disconnected tail after ECHILD; no Snapshot/input"
    );
    browser(case, watched).await;
}

/// Optional actual browser against the same daemon runtime that received bytes.
/// A database restored into another process cannot reproduce this volatile tail.
async fn browser(case: &Case, watched: &Watch) {
    let Some(script) = std::env::var_os("AQ_TEST_TAIL_BROWSER_SCRIPT") else {
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_TAIL_DASHBOARD_DIR")
        .expect("browser probe requires an explicit production dashboard bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = case.env.env.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let app = aq_server::build_admin_router_with_dashboard(state, Arc::new(assets));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let session = match watched.info.terminal.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => panic!("actual worker session required"),
    };
    let mut child = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", &origin)
        .env("AQ_LIVE_SESSION_ID", session.to_string())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(12), child.wait()).await;
    server.abort();
    assert!(result.expect("bounded browser probe").unwrap().success());
}
