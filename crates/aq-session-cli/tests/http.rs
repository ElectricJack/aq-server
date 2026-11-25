#![cfg(feature = "loopback-development")]
use aq_session_protocol::{
    ArtifactRef, ControllerLease, Digest, SessionIdentity, Snapshot, TerminalIdentity, Text,
};
use aq_terminal_relay::{
    api::{Mutation, MutationReply, OutputReply, TerminalInfo},
    draft::{ControlStatus, InputMode},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
#[derive(Clone)]
struct Fake {
    terminal: TerminalIdentity,
    control: Arc<Mutex<ControlStatus>>,
    calls: Arc<Mutex<Vec<String>>>,
    origin: String,
    mode: InputMode,
    lease_ms: u64,
    permit: Arc<Mutex<Option<aq_terminal_relay::epoch::Permit>>>,
}
fn clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
async fn inspect(State(f): State<Fake>) -> Json<TerminalInfo> {
    f.calls.lock().unwrap().push("inspect".into());
    Json(TerminalInfo {
        terminal: f.terminal,
        session_revision: 1,
        input_mode: f.mode,
        terminal_policy: aq_session_protocol::TerminalPolicy {
            host: aq_session_protocol::TerminalHost::Tmux,
            input_enabled: true,
            max_input_bytes: 4096,
            max_pending_input_bytes: 32768,
            max_frame_bytes: 65536,
            replay_bytes: 1048576,
        },
        control: f.control.lock().unwrap().clone(),
        last_peer_observed_at_ms: clock(),
        last_output_observed_at_ms: clock(),
        disconnected: false,
    })
}
async fn mutate(
    State(f): State<Fake>,
    headers: HeaderMap,
    Json(request): Json<Mutation>,
) -> Result<Json<MutationReply>, StatusCode> {
    assert_eq!(headers["origin"], f.origin);
    assert_eq!(headers["x-aq-request"], "1");
    assert_eq!(headers["x-aq-dev-email"], "operator@localhost");
    match request {
        Mutation::Watch(r) => {
            f.calls.lock().unwrap().push("watch".into());
            Ok(Json(MutationReply::Watch {
                terminal: f.terminal,
                operation_id: r.operation_id,
                subscription_id: r.operation_id,
                session_revision: 1,
                expires_at_ms: r.expires_at_ms,
            }))
        }
        Mutation::Control(r) => control(f, r),
        Mutation::OpenEpoch(r) => open_epoch(f, r),
        Mutation::EpochInput(r) => {
            r.validate().unwrap();
            let permit = f.permit.lock().unwrap().clone().unwrap();
            assert_eq!(
                r.input.operation_id,
                aq_terminal_relay::epoch::operation_id(&permit, r.sequence).unwrap()
            );
            assert!(r.input.expires_at_ms <= permit.expires_at_ms);
            f.calls.lock().unwrap().push("epoch_input".into());
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Mutation::Input(r) => {
            assert!(
                r.expires_at_ms
                    <= f.control
                        .lock()
                        .unwrap()
                        .lease
                        .as_ref()
                        .unwrap()
                        .expires_at_ms
            );
            f.calls.lock().unwrap().push("input".into());
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}
fn control(
    f: Fake,
    r: aq_session_api::AcquireControlRequest,
) -> Result<Json<MutationReply>, StatusCode> {
    f.calls.lock().unwrap().push("control".into());
    let mut status = f.control.lock().unwrap();
    status.generation = 1;
    status.fence_acknowledged = true;
    status.lease = Some(ControllerLease {
        terminal: f.terminal,
        principal_id: Text::new("operator").unwrap(),
        lease_id: Uuid::new_v4(),
        generation: 1,
        issued_at_ms: clock(),
        expires_at_ms: r.expires_at_ms.min(clock() + f.lease_ms),
        execution_expires_at_ms: r.expires_at_ms + 10000,
    });
    Ok(Json(MutationReply::Control {
        operation_id: r.operation_id,
        status: status.clone(),
    }))
}

fn open_epoch(
    f: Fake,
    r: aq_terminal_relay::epoch::Open,
) -> Result<Json<MutationReply>, StatusCode> {
    assert!(r.expires_at_ms <= clock() + 2000);
    f.calls.lock().unwrap().push("open_epoch".into());
    let expires = f
        .control
        .lock()
        .unwrap()
        .lease
        .as_ref()
        .unwrap()
        .expires_at_ms;
    let permit = aq_terminal_relay::epoch::Permit {
        terminal: f.terminal,
        epoch_id: Uuid::new_v4(),
        lease_id: r.lease_id,
        controller_generation: r.expected_controller_generation,
        expires_at_ms: expires,
        derivation_version: 1,
    };
    *f.permit.lock().unwrap() = Some(permit.clone());
    Ok(Json(MutationReply::Epoch {
        operation_id: r.operation_id,
        permit,
    }))
}

async fn output(
    State(f): State<Fake>,
    axum::extract::Query(q): axum::extract::Query<aq_terminal_relay::api::ViewQuery>,
) -> Json<OutputReply> {
    f.calls.lock().unwrap().push("output".into());
    let bytes = b"synthetic output\x1b[2J".to_vec();
    let snapshot = Snapshot {
        terminal: f.terminal.clone(),
        snapshot_id: Uuid::new_v4(),
        capture_request_id: Uuid::new_v4(),
        through_sequence: 0,
        columns: 80,
        rows: 24,
        renderer: ArtifactRef {
            id: Text::new("aq.synthetic.plaintext").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"renderer"),
        },
        digest: Digest::of(&bytes),
        state: bytes,
    };
    Json(OutputReply::Ready {
        view: Box::new(aq_session_api::WatchView {
            terminal: f.terminal,
            subscription_id: q.subscription_id,
            session_revision: 1,
            expires_at_ms: clock() + 10000,
            last_observed_at_ms: clock(),
            disconnected: false,
            coverage: aq_session_api::Coverage::Complete,
            truncated: false,
            snapshot_id: snapshot.snapshot_id,
            snapshot_reference: aq_session_api::Source {
                identity: snapshot.snapshot_id.to_string(),
                digest: snapshot.digest.as_str().into(),
                revision: "1".into(),
            },
            renderer_reference: aq_session_api::Source {
                identity: "aq.synthetic.plaintext".into(),
                digest: snapshot.renderer.digest.as_str().into(),
                revision: "1".into(),
            },
            through_sequence: 0,
            columns: 80,
            rows: 24,
            gaps: vec![],
        }),
        snapshot,
        frames: vec![],
        next_sequence: 0,
        more: false,
    })
}
async fn fake() -> (Fake, tokio::task::JoinHandle<()>) {
    fake_with(InputMode::FiniteV1, 15000).await
}
async fn fake_with(mode: InputMode, lease_ms: u64) -> (Fake, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let terminal = TerminalIdentity {
        session: SessionIdentity::Remote {
            node_id: Uuid::new_v4(),
            boot_id: Uuid::new_v4(),
            attempt_id: Uuid::new_v4(),
            assignment_generation: 1,
            session_id: Uuid::new_v4(),
        },
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let f = Fake {
        control: Arc::new(Mutex::new(ControlStatus {
            terminal: terminal.clone(),
            session_revision: 1,
            generation: 0,
            lease: None,
            fence_acknowledged: false,
            input_paused: false,
        })),
        terminal,
        calls: Default::default(),
        origin,
        mode,
        lease_ms,
        permit: Default::default(),
    };
    let app = Router::new()
        .route("/api/v1/terminals/{terminal}", get(inspect))
        .route("/api/v1/terminals/{terminal}/mutations", post(mutate))
        .route("/api/v1/terminals/{terminal}/output", get(output))
        .with_state(f.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (f, task)
}
fn command(f: &Fake) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-session"));
    c.args([
        "--origin",
        &f.origin,
        "--dev-email",
        "operator@localhost",
        "attach",
        &f.terminal.terminal_id.to_string(),
        "--seconds",
        "1",
    ]);
    c.env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    c
}
async fn attach_with_input(f: &Fake, bytes: &[u8]) -> std::process::Output {
    use tokio::io::AsyncWriteExt;
    let mut child = command(f)
        .arg("--control")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(8), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn actual_cli_attach_is_watch_only_by_default_and_json_never_executes_escape_bytes() {
    let (f, task) = fake().await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(8), command(&f).output())
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let calls = f.calls.lock().unwrap();
    assert!(calls.iter().any(|c| c == "watch"));
    assert!(calls.iter().any(|c| c == "output"));
    assert!(!calls.iter().any(|c| c == "control" || c == "input"));
    assert!(!result.stdout.contains(&27));
}
#[tokio::test]
async fn actual_cli_control_lost_input_reply_stops_without_resending_literal_bytes() {
    let (f, task) = fake().await;
    let result = attach_with_input(&f, b"private input sentinel\n").await;
    task.abort();
    assert!(!result.status.success());
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "input")
            .count(),
        1
    );
    let errors = String::from_utf8_lossy(&result.stderr);
    assert!(errors.contains("outcome unknown"));
    assert!(!errors.contains("sentinel"));
}

#[tokio::test]
async fn actual_cli_logs_and_usage_preserve_typed_unknowns_and_category_subtotals() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../aq-observation-api/artifacts/examples.json"
    ))
    .unwrap();
    let expected = fixture.clone();
    let events = fixture["events"].clone();
    let usage = fixture["usage"].clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/api/observations/sessions/{attempt}/events",
            get(move || {
                let events = events.clone();
                async move { Json(events) }
            }),
        )
        .route(
            "/api/observations/sessions/{attempt}/usage",
            get(move || {
                let usage = usage.clone();
                async move { Json(usage) }
            }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (command, key) in [("logs", "events"), ("usage", "usage")] {
        let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_aq-session"))
            .args([
                "--origin",
                &origin,
                "--dev-email",
                "operator@localhost",
                command,
                "00000000-0000-0000-0000-000000000003",
            ])
            .output()
            .await
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(value, expected[key]);
    }
    task.abort();
}

#[tokio::test]
async fn actual_cli_epoch_open_has_short_intent_deadline_and_never_falls_back_to_finite_input() {
    let (f, task) = fake_with(InputMode::EpochV1, 15000).await;
    let result = attach_with_input(&f, b"private epoch input\n").await;
    task.abort();
    assert!(!result.status.success());
    let calls = f.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|c| *c == "open_epoch").count(), 1);
    assert_eq!(calls.iter().filter(|c| *c == "epoch_input").count(), 1);
    assert!(!calls.iter().any(|c| c == "input"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("outcome unknown"));
}
#[tokio::test]
async fn actual_cli_accepts_short_control_lease_and_clamps_literal_deadline() {
    let (f, task) = fake_with(InputMode::FiniteV1, 1500).await;
    let result = attach_with_input(&f, b"private short lease input\n").await;
    task.abort();
    assert!(!result.status.success());
    assert_eq!(
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "input")
            .count(),
        1
    );
    assert!(String::from_utf8_lossy(&result.stderr).contains("outcome unknown"));
}
