//! Actual protected relay owners over synthetic running execution metadata.
//! No input injection, native process or physical terminal qualification.
mod common;
use aq_server::{
    execution::profiled::{Status, Stop},
    terminal::{
        self,
        config::Loaded,
        protection::{KeyFile, Keyring, ProtectionConfig},
        service,
    },
};
use aq_session_protocol::{Capability, ProfiledGrant, TerminalIdentity, Text};
use aq_terminal_relay::{
    api::MutationReply,
    draft::{
        CAPABILITY, Effect, Envelope, FenceAck, InputMode, Peer, PeerUpload, Register, RelayPoll,
    },
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::{Fixture, NOW, fixture, insert};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{Arc, atomic::Ordering},
};
use tower::ServiceExt;
use uuid::Uuid;

fn loaded(dir: &tempfile::TempDir) -> Arc<Loaded> {
    Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "output-poll".into(),
            keys: vec![KeyFile {
                id: "output-poll".into(),
                file: dir.path().join("key"),
            }],
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    })
}
async fn setup() -> (Fixture, tempfile::TempDir, Register) {
    let mut f = fixture().await;
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(NOW + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: NOW + 5000,
    });
    f.profiled.stop = Stop::Running;
    insert(&f).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("key"), [31u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut config = (*f.state.config).clone();
    config.terminals = Some(loaded(&dir));
    f.state.config = Arc::new(config);
    terminal::persistence::install(&f.state).await.unwrap();
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
    service::register(&f.state, &auth(&f).await, &r)
        .await
        .unwrap();
    (f, dir, r)
}
async fn auth(f: &Fixture) -> terminal::peer::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    terminal::peer::authenticate(&f.state, &headers)
        .await
        .unwrap()
}
fn request(r: &Register) -> RelayPoll {
    RelayPoll {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        cursor: 0,
        hold_ms: 0,
        active_subscriptions: 0,
        active_controllers: 0,
        advertised_capabilities: r.advertised_capabilities.clone(),
    }
}
async fn http(f: &Fixture, q: &RelayPoll) -> (StatusCode, Value) {
    let response = aq_server::build_worker_router(f.state.clone())
        .oneshot(
            Request::post("/v1/terminal-relay/output-poll")
                .header("authorization", format!("Bearer {}", f.credential))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(q).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn watch(f: &Fixture, r: &Register, ttl: u64) -> Uuid {
    let operation = Uuid::new_v4();
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: operation,
            expected_session_revision: 1,
            expires_at_ms: NOW + ttl,
            after_sequence: None,
        }),
    )
    .await
    .unwrap();
    operation
}
async fn queue_snapshot(f: &Fixture) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('inputs',(SELECT jsonb_agg(to_jsonb(i) ORDER BY operation_id) FROM terminal_input_receipts i),'outbox',(SELECT jsonb_agg(to_jsonb(o) ORDER BY outbox_id) FROM terminal_outbox o),'sessions',(SELECT jsonb_agg(to_jsonb(s) ORDER BY terminal_id) FROM terminal_sessions s))").fetch_one(&f.db.pool().await).await.unwrap()
}

#[tokio::test]
async fn output_poll_returns_watch_only_without_claiming_input_or_control_and_full_poll_is_unchanged()
 {
    let (f, _dir, r) = Box::pin(setup()).await;
    let node = auth(&f).await;
    let MutationReply::Control { status, .. } = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(aq_session_api::AcquireControlRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expected_controller_generation: 0,
            expires_at_ms: NOW + 4000,
        }),
    )
    .await
    .unwrap() else {
        panic!()
    };
    let lease = status.lease.unwrap();
    let first = terminal::peer::poll(&f.state, &node, &request(&r))
        .await
        .unwrap();
    let fence = first
        .effects
        .iter()
        .find_map(|e| {
            if let Effect::Fence(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    terminal::peer::upload(
        &f.state,
        &node,
        &PeerUpload {
            version: 1,
            capability: r.capability.clone(),
            peer: r.peer.clone(),
            exchange_id: Uuid::new_v4(),
            cursor: first.cursor,
            receipts: vec![],
            epoch_receipts: vec![],
            resize_receipts: vec![],
            fence_acks: vec![FenceAck {
                terminal: r.terminal.clone(),
                fence_id: fence.fence_id,
                generation: fence.generation,
            }],
            frames: vec![],
            snapshots: vec![],
            gaps: vec![],
        },
    )
    .await
    .unwrap();
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(aq_session_api::InputRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            lease_id: lease.lease_id,
            expected_controller_generation: lease.generation,
            expected_session_revision: 1,
            expires_at_ms: NOW + 2000,
            bytes: b"unexecuted fixture input".to_vec(),
        }),
    )
    .await
    .unwrap();
    let subscription = watch(&f, &r, 15000).await;
    let watch_cursor: i64 = sqlx::query_scalar(
        "SELECT outbox_id FROM terminal_outbox WHERE operation_id=$1 AND kind='watch'",
    )
    .bind(subscription)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Resize(aq_session_api::ResizeRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            lease_id: lease.lease_id,
            expected_controller_generation: lease.generation,
            expected_session_revision: 1,
            expires_at_ms: NOW + 2000,
            sequence: 1,
            columns: 100,
            rows: 30,
        }),
    )
    .await
    .unwrap();
    let before = queue_snapshot(&f).await;
    let (code, body) = http(&f, &request(&r)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let envelope: Envelope = serde_json::from_value(body).unwrap();
    assert_eq!(envelope.cursor, watch_cursor as u64);
    assert_eq!(envelope.effects.len(), 1);
    assert!(matches!(&envelope.effects[0],Effect::Watch(w) if w.subscription_id==subscription));
    assert_eq!(
        before,
        queue_snapshot(&f).await,
        "output delivery cannot mutate input,control or acknowledgement state"
    );
    let full = terminal::peer::poll(&f.state, &node, &request(&r))
        .await
        .unwrap();
    assert_eq!(
        full.effects
            .iter()
            .filter(|e| matches!(e, Effect::Input(_)))
            .count(),
        1,
        "full poll still dispatches with unchanged zero-controller activity hint"
    );
    assert!(full.effects.iter().any(|e| matches!(e, Effect::Resize(_))));
}

#[tokio::test]
async fn output_poll_filters_revoked_and_expired_watches_without_repair() {
    let (f, _dir, r) = Box::pin(setup()).await;
    watch(&f, &r, 1000).await;
    assert_eq!(
        terminal::peer::output_poll(&f.state, &auth(&f).await, &request(&r))
            .await
            .unwrap()
            .effects
            .len(),
        1
    );
    let pool = f.db.pool().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    let before = queue_snapshot(&f).await;
    assert!(
        terminal::peer::output_poll(&f.state, &auth(&f).await, &request(&r))
            .await
            .unwrap()
            .effects
            .is_empty()
    );
    assert_eq!(before, queue_snapshot(&f).await);
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    f.time.store((NOW + 1001) as i64, Ordering::SeqCst);
    let before = queue_snapshot(&f).await;
    assert!(
        terminal::peer::output_poll(&f.state, &auth(&f).await, &request(&r))
            .await
            .unwrap()
            .effects
            .is_empty()
    );
    assert_eq!(before, queue_snapshot(&f).await);
}

#[tokio::test]
async fn output_poll_refuses_non_output_modes_stale_credentials_boot_and_runtime_without_control_recovery()
 {
    let (mut f, dir, r) = Box::pin(setup()).await;
    watch(&f, &r, 15000).await;
    let mut q = request(&r);
    q.active_subscriptions = 1;
    q.hold_ms = 1;
    assert_eq!(http(&f, &q).await.0, StatusCode::BAD_REQUEST);
    q = request(&r);
    q.active_controllers = 1;
    assert_eq!(http(&f, &q).await.0, StatusCode::BAD_REQUEST);
    q = request(&r);
    let Peer::Remote { boot_id, .. } = &mut q.peer else {
        panic!()
    };
    *boot_id = Uuid::new_v4();
    assert_eq!(http(&f, &q).await.0, StatusCode::CONFLICT);
    let mut config = (*f.state.config).clone();
    config.terminals = Some(loaded(&dir));
    f.state.config = Arc::new(config);
    // Publish the new owner so current_peer succeeds. The terminal itself still
    // belongs to the old runtime; this reaches the output-only recovery refusal.
    terminal::persistence::install(&f.state).await.unwrap();
    let before = queue_snapshot(&f).await;
    assert!(
        terminal::peer::output_poll(&f.state, &auth(&f).await, &request(&r))
            .await
            .is_err()
    );
    assert_eq!(
        before,
        queue_snapshot(&f).await,
        "output poll must not run control recovery after runtime replacement"
    );
    let node = auth(&f).await;
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier("aqn_rotated-fixture-key"))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert!(matches!(
        terminal::peer::output_poll(&f.state, &node, &request(&r)).await,
        Err(aq_server::error::AppError::CredentialRevoked)
    ));
}
