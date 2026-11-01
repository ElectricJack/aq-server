//! Actual output-only terminal registration, human Watch and retained literal tail.
use super::*;
use aq_session_protocol::Validate;
use aq_terminal_relay::{
    api::{MutationReply, TerminalInfo},
    draft::{Effect, Envelope, PeerUpload, UploadAck},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

/// Observation only: requests still pass unchanged through the actual router.
/// The worker validates the real response; this test independently binds it to
/// the exact published frames and authorized human tail.
#[derive(Clone)]
pub(super) struct UploadExchange {
    request: PeerUpload,
    status: StatusCode,
}
pub(super) async fn capture_upload_request(
    request: axum::extract::Request,
) -> (axum::extract::Request, Option<PeerUpload>) {
    if request.uri().path() != "/v1/terminal-relay/upload" {
        return (request, None);
    }
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 256 * 1024)
        .await
        .expect("bounded actual output upload");
    let upload: PeerUpload = serde_json::from_slice(&bytes).unwrap();
    upload.validate().unwrap();
    (Request::from_parts(parts, Body::from(bytes)), Some(upload))
}
pub(super) fn record_upload(
    observed: &Arc<std::sync::Mutex<Vec<UploadExchange>>>,
    request: Option<PeerUpload>,
    status: StatusCode,
) {
    if let Some(request) = request {
        let mut observed = observed.lock().unwrap();
        assert!(observed.len() < 8, "bounded output exchange observation");
        observed.push(UploadExchange { request, status });
    }
}

/// Bounded metadata only: never dump a refusal error chain, stdin config,
/// credential, material body or arbitrary journal field.
pub(super) fn refusal_diagnostics(
    root: &std::path::Path,
    frame: &Value,
    statuses: &Arc<std::sync::Mutex<Vec<(String, u16, u128)>>>,
) {
    let snapshot = std::fs::read(root.join("journal/journal.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let attempt = snapshot
        .as_ref()
        .and_then(|v| v["state"]["attempts"].as_object())
        .and_then(|a| a.values().next());
    let host = attempt.map(|a| &a["native_host"]);
    eprintln!(
        "bounded output fixture refusal: {}",
        json!({
            "phase":frame.get("phase").and_then(Value::as_str),
            "stage":frame.get("stage").and_then(Value::as_str),
            "http_statuses":*statuses.lock().unwrap(),
            "journal_revision":snapshot.as_ref().and_then(|v|v["state"]["revision"].as_u64()),
            "execution_deadline_ns":attempt.and_then(|a|a["phase_execution"]["deadline_ns"].as_u64()),
        "reserved_at_ns":attempt.and_then(|a|a["reservation"]["reserved_at_ns"].as_u64()),
        "reservation_deadline_ns":attempt.and_then(|a|a["reservation"]["deadline_ns"].as_u64()),
        "phase_accept_created_ns":attempt.and_then(|a|a["phase_acceptance"]["created_ns"].as_u64()),
        "phase_accept_original_deadline_ns":attempt.and_then(|a|a["phase_acceptance"]["original_deadline_ns"].as_u64()),
        "phase_accept_reservation_remaining_ms":attempt.and_then(|a|a["phase_acceptance"]["request"]["original"]["reservation_remaining_ms"].as_u64()),
            "baseline":host.is_some_and(|h|h["baseline"].is_object()),
            "running":host.is_some_and(|h|h["running"].is_object()),
            "stopped":host.is_some_and(|h|h["stopped"].is_object()),
            "kernel_wait_echild":host.and_then(|h|h["stopped"]["observation"]["kernel_wait_echild"].as_bool()),
            "started_ack":host.is_some_and(|h|h["terminal"]["started"]["receipt"].is_object()),
            "stopped_ack":host.is_some_and(|h|h["terminal"]["stopped"]["receipt"].is_object())
        })
    );
}

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
    assert_eq!(
        ready["phase"],
        "terminal_registered",
        "worker refusal stage={:?}",
        ready.get("stage")
    );
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
pub(super) async fn verify(
    case: &Case,
    watched: &Watch,
    result: &Value,
    observed: &Arc<std::sync::Mutex<Vec<UploadExchange>>>,
) {
    let exchanges = observed.lock().unwrap().clone();
    assert_eq!(exchanges.len(), 1, "one actual accepted output exchange");
    let exchange = &exchanges[0];
    assert_eq!(exchange.status, StatusCode::OK);
    let upload = &exchange.request;
    let ack: UploadAck =
        serde_json::from_value(result["phase_output"]["upload_ack"].clone()).unwrap();
    ack.validate().unwrap();
    assert_eq!(ack.version, upload.version);
    assert_eq!(ack.capability, upload.capability);
    assert_eq!(ack.peer, upload.peer);
    assert_eq!(ack.exchange_id, upload.exchange_id);
    assert_eq!(ack.cursor, upload.cursor);
    assert!(ack.retirements.is_empty() && ack.dropped_output.is_empty());
    assert!(
        upload.receipts.is_empty()
            && upload.epoch_receipts.is_empty()
            && upload.resize_receipts.is_empty()
            && upload.fence_acks.is_empty()
            && upload.snapshots.is_empty()
            && upload.gaps.is_empty()
    );
    assert!(!upload.frames.is_empty());
    let pull: Envelope =
        serde_json::from_value(result["phase_output"]["watch_pull"].clone()).unwrap();
    pull.validate().unwrap();
    assert_eq!(pull.peer, upload.peer);
    assert_eq!(pull.effects.len(), 1);
    let Effect::Watch(subscription) = &pull.effects[0] else {
        panic!("real node-pulled Watch required")
    };
    assert_eq!(subscription.terminal, watched.info.terminal);
    assert_eq!(subscription.subscription_id, watched.subscription);

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
    // No injected frame or invented accepted receipt: exact bytes, digests and
    // sequence numbers match what the real worker sent through the HTTP owner.
    assert_eq!(tail.frames, upload.frames);
    assert!(tail.gaps.is_empty() && !tail.more);
    assert_eq!(tail.next_sequence, upload.frames.last().unwrap().sequence);
    let expected_bytes: Vec<u8> = upload.frames.iter().flat_map(|f| f.bytes.clone()).collect();
    assert_eq!(bytes, expected_bytes);
    // The admitted periodic C fixture writes these fixed CRLF lines. The
    // pinned tmux PTY's ONLCR translates LF once more; no substring qualifies.
    let tick = b"synthetic native fixture output tick\r\r\n";
    let running = b"synthetic native fixture running\r\r\n";
    let ticks = bytes.strip_prefix(running).unwrap_or(&bytes);
    assert!(!ticks.is_empty() && ticks.chunks(tick.len()).all(|line| line == tick));
    assert_eq!(upload.frames[0].sequence, 1);
    assert!(
        upload
            .frames
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1)
    );
    assert_eq!(
        result["phase_output"]["terminal_info"]["terminal"],
        json!(tail.terminal)
    );
    assert!(tail.next_sequence > 0);
    eprintln!(
        "bounded accepted output proof: {}",
        json!({
            "upload_ack":ack,
            "subscription_id":watched.subscription,
            "terminal":tail.terminal,
            "tail_next_sequence":tail.next_sequence,
            "tail_disconnected":tail.disconnected,
            "tail_initial_incomplete":tail.initial_incomplete,
            "byte_count":bytes.len(),
            "frames":upload.frames.iter().map(|frame|json!({
                "sequence":frame.sequence,"byte_length":frame.byte_length,"digest":frame.digest
            })).collect::<Vec<_>>()
        })
    );
    eprintln!(
        "actual worker output: real admin Watch, node pull, HTTP200 exact UploadAck, byte/digest/sequence-identical authorized tail after ECHILD; no Snapshot/input"
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
