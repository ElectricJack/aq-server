//! Synthetic SQL/relay qualification; no tmux, provider or input injection claim.
use crate::common;
use aq_server::{
    execution::profiled::{Status, Stop},
    terminal::{
        config::Loaded,
        protection::{KeyFile, Keyring, ProtectionConfig},
        service,
    },
};
use aq_session_protocol::{Capability, ProfiledGrant, TerminalIdentity, Text};
use aq_terminal_relay::draft::{CAPABILITY, InputMode, Peer, Register};
use common::profiled::{Fixture, NOW, fixture, insert};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use uuid::Uuid;

mod authorization;
mod capacity;
mod dispatch;
mod epochs;
mod persistence;

async fn running() -> (Fixture, tempfile::TempDir, Register) {
    running_with_policy(None).await
}

async fn running_with_policy(
    policy: Option<aq_session_protocol::TerminalPolicy>,
) -> (Fixture, tempfile::TempDir, Register) {
    let mut f = fixture().await;
    if let Some(policy) = policy {
        f.profiled.offer.manifest.terminal = policy;
        f.profiled.offer.binding.launch_manifest_digest =
            aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    }
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5_000,
    });
    f.profiled.stop = Stop::Running;
    insert(&f).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    std::fs::write(&path, [31u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut cfg = (*f.state.config).clone();
    cfg.terminals = Some(relay_config(path, false));
    f.state.config = Arc::new(cfg);
    let r = Register {
        version: 1,
        capability: Text::new(CAPABILITY).unwrap(),
        operation_id: Uuid::new_v4(),
        peer: Peer::Remote {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        terminal: TerminalIdentity {
            session: f.profiled.offer.binding.session.clone(),
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        },
        expected_authority_revision: 1,
        input_mode: InputMode::FiniteV1,
        advertised_capabilities: vec![Capability {
            id: Text::new(CAPABILITY).unwrap(),
            version: 1,
        }],
    };
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
    (f, dir, r)
}

async fn authenticated(f: &Fixture) -> aq_server::terminal::peer::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    aq_server::terminal::peer::authenticate(&f.state, &headers)
        .await
        .unwrap()
}

async fn register(f: &Fixture, r: &Register) {
    let node = authenticated(f).await;
    service::register(&f.state, &node, r).await.unwrap();
}

fn polling(r: &Register, cursor: u64) -> aq_terminal_relay::draft::RelayPoll {
    aq_terminal_relay::draft::RelayPoll {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        cursor,
        hold_ms: 0,
        active_subscriptions: 0,
        active_controllers: 1,
        advertised_capabilities: r.advertised_capabilities.clone(),
    }
}

async fn acquire_control(
    f: &Fixture,
    r: &Register,
    actor: Uuid,
) -> aq_terminal_relay::draft::ControlStatus {
    let reply = service::mutate(
        &f.state,
        actor,
        &service::Mutation::Control(control_request(r, 4_000)),
    )
    .await
    .unwrap();
    let aq_terminal_relay::api::MutationReply::Control { status, .. } = reply else {
        panic!("expected controller status")
    };
    status
}

async fn pending_fence(
    f: &Fixture,
    r: &Register,
    node: &aq_server::terminal::peer::AuthenticatedNode,
) -> (u64, aq_terminal_relay::draft::Fence) {
    let poll = aq_server::terminal::peer::poll(&f.state, node, &polling(r, 0))
        .await
        .unwrap();
    let fence = poll
        .effects
        .into_iter()
        .find_map(|effect| match effect {
            aq_terminal_relay::draft::Effect::Fence(fence) => Some(fence),
            _ => None,
        })
        .expect("controller acquired without a worker fence");
    (poll.cursor, fence)
}

async fn controller_as(
    f: &Fixture,
    r: &Register,
    actor: Uuid,
) -> aq_session_protocol::ControllerLease {
    let node = authenticated(f).await;
    let lease = acquire_control(f, r, actor).await.lease.unwrap();
    let (_, fence) = pending_fence(f, r, &node).await;
    aq_server::terminal::peer::upload(&f.state, &node, &fence_upload(r, 0, &fence))
        .await
        .unwrap();
    lease
}

fn literal_request(
    r: &Register,
    lease: &aq_session_protocol::ControllerLease,
) -> aq_session_api::InputRequest {
    aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2000,
        bytes: b"private literal input".to_vec(),
        escape_key: false,
    }
}

async fn queued_input_as(f: &Fixture, r: &Register, actor: Uuid) -> aq_session_api::InputRequest {
    let lease = controller_as(f, r, actor).await;
    let request = literal_request(r, &lease);
    service::mutate(&f.state, actor, &service::Mutation::Input(request.clone()))
        .await
        .unwrap();
    request
}

async fn claimed_input_as(f: &Fixture, r: &Register, actor: Uuid) -> aq_session_api::InputRequest {
    let request = queued_input_as(f, r, actor).await;
    let node = authenticated(f).await;
    let claimed = aq_server::terminal::peer::poll(&f.state, &node, &polling(r, 0))
        .await
        .unwrap();
    assert!(
        claimed
            .effects
            .iter()
            .any(|e| matches!(e, aq_terminal_relay::draft::Effect::Input(_)))
    );
    request
}

fn control_request(r: &Register, ttl_ms: u64) -> aq_session_api::AcquireControlRequest {
    aq_session_api::AcquireControlRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expected_controller_generation: 0,
        expires_at_ms: NOW + ttl_ms,
    }
}

fn empty_upload(r: &Register, cursor: u64) -> aq_terminal_relay::draft::PeerUpload {
    aq_terminal_relay::draft::PeerUpload {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor,
        receipts: vec![],
        epoch_receipts: vec![],
        resize_receipts: vec![],
        fence_acks: vec![],
        frames: vec![],
        snapshots: vec![],
        gaps: vec![],
    }
}

fn fence_upload(
    r: &Register,
    cursor: u64,
    fence: &aq_terminal_relay::draft::Fence,
) -> aq_terminal_relay::draft::PeerUpload {
    use aq_terminal_relay::draft::{FenceAck, PeerUpload};
    PeerUpload {
        fence_acks: vec![FenceAck {
            terminal: r.terminal.clone(),
            fence_id: fence.fence_id,
            generation: fence.generation,
        }],
        ..empty_upload(r, cursor)
    }
}

async fn new_viewer(pool: &sqlx::PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'viewer',now())")
        .bind(id)
        .bind(format!("{id}@fixture.test"))
        .execute(pool)
        .await
        .unwrap();
    id
}

fn relay_config(key_file: std::path::PathBuf, epoch_input: bool) -> Arc<Loaded> {
    Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: key_file,
            }],
        })
        .unwrap(),
        epoch_input,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    })
}

fn watch_request(r: &Register, ttl_ms: u64) -> aq_session_api::WatchRequest {
    aq_session_api::WatchRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expires_at_ms: NOW + ttl_ms,
        after_sequence: None,
    }
}
async fn watch(f: &Fixture, r: &Register, actor: Uuid, ttl_ms: u64) -> Uuid {
    let request = watch_request(r, ttl_ms);
    let id = request.operation_id;
    service::mutate(&f.state, actor, &service::Mutation::Watch(request))
        .await
        .unwrap();
    id
}

enum Grant {
    Inspect,
    Control,
}
impl Grant {
    fn for_actor(self, r: &Register, actor: Uuid) -> aq_terminal_relay::api::AccessUpdate {
        aq_terminal_relay::api::AccessUpdate {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            principal_id: actor,
            expected_inspection_revision: 0,
            expected_control_revision: 0,
            can_inspect: matches!(self, Self::Inspect),
            can_control: matches!(self, Self::Control),
            expires_at_ms: NOW + 20_000,
        }
    }
}
