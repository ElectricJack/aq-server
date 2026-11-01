//! Actual phase Grant→Started→terminal HTTP registration and inspection owners.
//! Graph/runtime/output claims are synthetic here; physical worker tests separate.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod phase;
use aq_server::{
    AppState,
    terminal::{
        self,
        config::Loaded,
        protection::{KeyFile, Keyring, ProtectionConfig},
        service,
        tail::{self, Availability},
    },
};
use aq_session_protocol::{Capability, Digest, OutputFrame, OutputGap, TerminalIdentity, Text};
use aq_terminal_relay::{
    api::{MutationReply, ViewQuery},
    draft::{CAPABILITY, InputMode, Peer, PeerUpload, Register},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};
use tower::ServiceExt;
use uuid::Uuid;
struct Case {
    f: phase::Prepared,
    started: aq_phase_terminal_protocol::StartedRequest,
    registration: Register,
    actor: Uuid,
    at: Arc<AtomicI64>,
    dir: tempfile::TempDir,
}
async fn http(
    state: &AppState,
    credential: Option<&str>,
    path: &str,
    body: Option<Value>,
    email: &str,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let app = if credential.is_some() {
        aq_server::build_worker_router(state.clone())
    } else {
        aq_server::build_admin_router(state.clone())
    };
    let mut req = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("content-type", "application/json")
        .header("X-AQ-Request", "1")
        .header("x-aq-dev-email", email);
    if let Some(value) = credential {
        req = req.header("authorization", format!("Bearer {value}"));
    }
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let response = app
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"invalid_body":true})),
    )
}
fn config(dir: &tempfile::TempDir) -> Arc<Loaded> {
    Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "tail-test".into(),
            keys: vec![KeyFile {
                id: "tail-test".into(),
                file: dir.path().join("key"),
            }],
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    })
}
async fn setup() -> Case {
    let mut f = Box::pin(phase::prepared_terminal()).await;
    let execution = phase::issue(&f).await;
    let started = phase::started_request(&f, &execution);
    let ack = aq_server::profiled_execution::phase_terminal::started::record(
        &f.c.env.env.state,
        &phase::auth(&f).await,
        &started,
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("key"), [31u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let at = Arc::new(AtomicI64::new(
        chrono::Utc::now()
            .timestamp_millis()
            .max(ack.original.recorded_at_ms as i64 + 1),
    ));
    let clock = at.clone();
    f.c.env.env.state.clock = Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(clock.load(Ordering::SeqCst)).unwrap()
    });
    let mut cfg = (*f.c.env.env.state.config).clone();
    cfg.terminals = Some(config(&dir));
    f.c.env.env.state.config = Arc::new(cfg);
    terminal::persistence::install(&f.c.env.env.state)
        .await
        .unwrap();
    let registration = Register {
        version: 1,
        capability: Text::new(CAPABILITY).unwrap(),
        operation_id: Uuid::new_v4(),
        peer: Peer::Remote {
            node_id: f.prepared.original.identity.node_id,
            boot_id: f.prepared.original.identity.boot_id,
        },
        terminal: TerminalIdentity {
            session: execution.grant.binding.session.clone(),
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        },
        expected_authority_revision: ack.original.authority_revision_at_record,
        input_mode: InputMode::FiniteV1,
        advertised_capabilities: vec![Capability {
            id: Text::new(CAPABILITY).unwrap(),
            version: 1,
        }],
    };
    let body = serde_json::to_value(&registration).unwrap();
    let (status, _, reply) = http(
        &f.c.env.env.state,
        Some(&f.c.env.env.credential),
        "/v1/terminal-relay/register",
        Some(body),
        "",
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let MutationReply::Registered { operation_id, info } = serde_json::from_value(reply).unwrap()
    else {
        panic!("frozen registered reply required")
    };
    assert_eq!(operation_id, registration.operation_id);
    assert_eq!(info.terminal, registration.terminal);
    assert_eq!(
        info.session_revision,
        registration.expected_authority_revision
    );
    assert!(!info.terminal_policy.input_enabled);
    let actor = f.c.env.actor;
    Case {
        f,
        started,
        registration,
        actor,
        at,
        dir,
    }
}
async fn watch(c: &Case, actor: Uuid, ttl: i64) -> (Uuid, u64) {
    let id = Uuid::new_v4();
    let expiry = (c.at.load(Ordering::SeqCst) + ttl) as u64;
    let r = service::Mutation::Watch(aq_session_api::WatchRequest {
        terminal: c.registration.terminal.clone(),
        operation_id: id,
        expected_session_revision: c.registration.expected_authority_revision,
        expires_at_ms: expiry,
        after_sequence: Some(0),
    });
    service::mutate(&c.f.c.env.env.state, actor, &r)
        .await
        .unwrap();
    (id, expiry)
}
async fn read(
    c: &Case,
    actor: Uuid,
    id: Uuid,
    after: u64,
) -> Result<tail::TailReply, aq_server::error::AppError> {
    tail::read(
        &c.f.c.env.env.state,
        actor,
        c.registration.terminal.terminal_id,
        &ViewQuery {
            subscription_id: id,
            after_sequence: after,
        },
    )
    .await
}
fn frame(c: &Case, sequence: u64, bytes: &[u8]) -> OutputFrame {
    OutputFrame {
        terminal: c.registration.terminal.clone(),
        sequence,
        bytes: bytes.to_vec(),
        byte_length: bytes.len() as u32,
        digest: Digest::of(bytes),
    }
}
async fn upload(c: &Case, frames: Vec<OutputFrame>, gaps: Vec<OutputGap>) {
    terminal::peer::upload(
        &c.f.c.env.env.state,
        &phase::auth(&c.f).await,
        &PeerUpload {
            version: 1,
            capability: c.registration.capability.clone(),
            peer: c.registration.peer.clone(),
            exchange_id: Uuid::new_v4(),
            cursor: 0,
            receipts: vec![],
            epoch_receipts: vec![],
            resize_receipts: vec![],
            fence_acks: vec![],
            frames,
            snapshots: vec![],
            gaps,
        },
    )
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_phase_http_registration_and_dashboard_link_expose_read_only_tail() {
    let c = Box::pin(setup()).await;
    let (id, _) = watch(&c, c.actor, 15000).await;
    let empty = read(&c, c.actor, id, 0).await.unwrap();
    assert_eq!(empty.availability, Availability::NotRetained);
    assert!(empty.last_observed_at_ms.is_none());
    upload(
        &c,
        vec![frame(
            &c,
            1,
            b"real phase scope\n<script>literal</script>\x1b[31m",
        )],
        vec![],
    )
    .await;
    let path = format!(
        "/api/v1/terminals/{}/tail?subscription_id={id}&after_sequence=0",
        c.registration.terminal.terminal_id
    );
    let (status, headers, body) =
        http(&c.f.c.env.env.state, None, &path, None, "owner@localhost").await;
    assert_eq!(status, 200, "{body}");
    let reply: tail::TailReply = serde_json::from_value(body).unwrap();
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        headers["aq-terminal-server-time-ms"],
        reply.server_time_ms.to_string()
    );
    assert_eq!(
        reply.frames[0].bytes,
        b"real phase scope\n<script>literal</script>\x1b[31m"
    );
    assert!(reply.initial_incomplete);
    let (_, headers, _) = http(
        &c.f.c.env.env.state,
        None,
        &format!("/api/v1/terminals/{}", c.registration.terminal.terminal_id),
        None,
        "owner@localhost",
    )
    .await;
    assert!(headers.contains_key("aq-terminal-server-time-ms"));
    let (_, _, bootstrap) = http(
        &c.f.c.env.env.state,
        None,
        "/api/v1/bootstrap",
        None,
        "owner@localhost",
    )
    .await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["name"] == "terminal.watch" && a["enabled"] == true),
        "{bootstrap}"
    );
    let session = match c.registration.terminal.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    let (status, _, detail) = http(
        &c.f.c.env.env.state,
        None,
        &format!("/api/v1/sessions/{session}"),
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{detail}");
    assert_eq!(
        detail["data"]["terminal"]["id"],
        c.registration.terminal.terminal_id.to_string()
    );
    assert!(
        service::mutate(
            &c.f.c.env.env.state,
            c.actor,
            &service::Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: c.registration.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: c.registration.expected_authority_revision,
                expected_controller_generation: 0,
                expires_at_ms: reply.server_time_ms + 1000
            })
        )
        .await
        .is_err()
    );
}
#[tokio::test]
async fn maximum_frame_and_later_gap_do_not_stall_or_skip_budgeted_frames() {
    let c = Box::pin(setup()).await;
    let (id, _) = watch(&c, c.actor, 15000).await;
    upload(
        &c,
        vec![
            frame(&c, 1, &vec![255; 65536]),
            frame(&c, 2, b"two"),
            frame(&c, 3, b"three"),
        ],
        vec![],
    )
    .await;
    upload(
        &c,
        vec![],
        vec![OutputGap {
            terminal: c.registration.terminal.clone(),
            first_missing: 4,
            last_missing: 6,
        }],
    )
    .await;
    let first = read(&c, c.actor, id, 0).await.unwrap();
    assert_eq!(first.frames.len(), 1);
    assert_eq!(first.frames[0].bytes.len(), 65536);
    assert_eq!(first.next_sequence, 1);
    assert!(first.more);
    assert!(first.gaps.is_empty());
    assert!(serde_json::to_vec(&first).unwrap().len() <= tail::MAX_WIRE_BYTES);
    let next = read(&c, c.actor, id, first.next_sequence).await.unwrap();
    assert_eq!(
        next.frames.iter().map(|f| f.sequence).collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(next.next_sequence, 6);
    assert!(!next.more);
    assert_eq!(next.gaps[0].first_missing, 4);
}
#[tokio::test]
async fn stopped_and_expired_execution_can_inspect_resident_bytes_but_expired_watch_cannot() {
    let c = Box::pin(setup()).await;
    let (id, expiry) = watch(&c, c.actor, 15000).await;
    upload(&c, vec![frame(&c, 1, b"retained until watch ends")], vec![]).await;
    let stop = phase::stop_request(&c.started);
    aq_server::profiled_execution::phase_terminal::stopped::record(
        &c.f.c.env.env.state,
        &phase::auth(&c.f).await,
        &stop,
    )
    .await
    .unwrap();
    let v = read(&c, c.actor, id, 0).await.unwrap();
    assert_eq!(v.availability, Availability::ResidentTail);
    assert!(v.disconnected);
    assert_eq!(v.frames.len(), 1);
    c.at.store(expiry as i64, Ordering::SeqCst);
    assert!(read(&c, c.actor, id, 0).await.is_err());
    let (new, _) = watch(&c, c.actor, 1000).await;
    let v = read(&c, c.actor, new, 0).await.unwrap();
    assert_eq!(v.availability, Availability::NotRetained);
    assert!(v.frames.is_empty());
}
#[tokio::test]
async fn restart_forgets_output_and_never_uses_sql_activity_as_bytes() {
    let mut c = Box::pin(setup()).await;
    let (id, _) = watch(&c, c.actor, 15000).await;
    upload(&c, vec![frame(&c, 1, b"volatile")], vec![]).await;
    let mut cfg = (*c.f.c.env.env.state.config).clone();
    cfg.terminals = Some(config(&c.dir));
    c.f.c.env.env.state.config = Arc::new(cfg);
    terminal::persistence::install(&c.f.c.env.env.state)
        .await
        .unwrap();
    let v = read(&c, c.actor, id, 0).await.unwrap();
    assert_eq!(v.availability, Availability::NotRetained);
    assert!(v.disconnected);
    assert!(v.last_observed_at_ms.is_none());
    assert!(v.frames.is_empty());
}
#[tokio::test]
async fn inspection_revocation_and_foreign_subscription_never_return_tail() {
    let c = Box::pin(setup()).await;
    let (owner_sub, _) = watch(&c, c.actor, 15000).await;
    upload(&c, vec![frame(&c, 1, b"scoped")], vec![]).await;
    let viewer = Uuid::new_v4();
    let pool = &c.f.c.env.env.pool;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'tail-viewer@localhost','viewer','dev','tail-viewer@localhost',now())").bind(viewer).execute(pool).await.unwrap();
    assert!(read(&c, viewer, owner_sub, 0).await.is_err());
    let key = aq_server::session_access::session_key(&c.registration.terminal.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(c.registration.terminal.session)).bind(viewer).bind(c.at.load(Ordering::SeqCst)+5000).execute(pool).await.unwrap();
    assert!(read(&c, viewer, owner_sub, 0).await.is_err());
    let (sub, _) = watch(&c, viewer, 4000).await;
    assert_eq!(read(&c, viewer, sub, 0).await.unwrap().frames.len(), 1);
    sqlx::query("UPDATE session_inspection_acl SET can_inspect=false,revision=revision+1 WHERE principal_id=$1").bind(viewer).execute(pool).await.unwrap();
    assert!(read(&c, viewer, sub, 0).await.is_err());
    assert_eq!(
        read(&c, c.actor, owner_sub, 0).await.unwrap().frames.len(),
        1
    );
}
