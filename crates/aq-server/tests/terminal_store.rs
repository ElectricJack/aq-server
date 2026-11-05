//! Synthetic SQL/relay qualification; no tmux, provider or input injection claim.
mod common;
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
    cfg.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: path,
            }],
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
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
#[tokio::test]
async fn encrypted_registration_and_exact_controller_retry_survive_sql_reload() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    let mut changed_registration = r.clone();
    changed_registration
        .advertised_capabilities
        .push(Capability {
            id: Text::new("test.additional").unwrap(),
            version: 1,
        });
    assert!(
        service::register(&f.state, &node, &changed_registration)
            .await
            .is_err()
    );
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert_eq!(info.terminal, r.terminal);
    let request = aq_session_api::AcquireControlRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expected_controller_generation: 0,
        expires_at_ms: NOW + 4_000,
    };
    let first = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(request.clone()),
    )
    .await
    .unwrap();
    let retry = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(request.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(retry).unwrap()
    );
    let mut changed = request;
    changed.expires_at_ms += 1;
    assert!(
        service::mutate(&f.state, f.actor, &service::Mutation::Control(changed))
            .await
            .is_err()
    );
    let row: (Vec<u8>, i64) = sqlx::query_as(
        "SELECT ciphertext,ledger_revision FROM terminal_sessions WHERE terminal_id=$1",
    )
    .bind(r.terminal.terminal_id)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    assert!(!row.0.windows(10).any(|v| v == b"controller"));
    assert_eq!(row.1, 1);
}
#[tokio::test]
async fn altered_ciphertext_and_terminal_replacement_fail_closed() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let mut changed = r.clone();
    changed.terminal.epoch += 1;
    let node = authenticated(&f).await;
    assert!(service::register(&f.state, &node, &changed).await.is_err());
    sqlx::query("UPDATE terminal_sessions SET ciphertext=decode(repeat('00',32),'hex') WHERE terminal_id=$1").bind(r.terminal.terminal_id).execute(&f.db.pool().await).await.unwrap();
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
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
#[tokio::test]
async fn lost_poll_response_never_redispatches_input_and_expiry_is_durable_unknown() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay::{
        api::MutationReply,
        draft::{Effect, FenceAck, PeerUpload},
    };
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    let reply = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Control(aq_session_api::AcquireControlRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expected_controller_generation: 0,
            expires_at_ms: NOW + 4_000,
        }),
    )
    .await
    .unwrap();
    let MutationReply::Control { status, .. } = reply else {
        panic!()
    };
    let lease = status.lease.unwrap();
    assert!(!status.fence_acknowledged);
    let first = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
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
    let upload = PeerUpload {
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
    };
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    let request = aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2_000,
        bytes: b"private literal input".to_vec(),
    };
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(request.clone()),
    )
    .await
    .unwrap();
    let claimed = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, first.cursor))
        .await
        .unwrap();
    assert_eq!(
        claimed
            .effects
            .iter()
            .filter(|e| matches!(e, Effect::Input(_)))
            .count(),
        1
    );
    let retry = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, first.cursor))
        .await
        .unwrap();
    assert!(!retry.effects.iter().any(|e| matches!(e, Effect::Input(_))));
    f.time
        .store((NOW + 2_001) as i64, std::sync::atomic::Ordering::SeqCst);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    let observed = service::mutate(&f.state, f.actor, &service::Mutation::Input(request))
        .await
        .unwrap();
    let MutationReply::Input(receipt) = observed else {
        panic!()
    };
    assert_eq!(receipt.disposition, InputDisposition::DeliveryUnknown);
    assert!(receipt.input_paused);
    let pool = f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_outbox WHERE terminal_id=$1 AND kind='input'"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT disposition FROM terminal_input_receipts WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "delivery_unknown"
    );
}

#[tokio::test]
async fn viewer_inspection_is_exact_revocable_historical_and_never_control_authority() {
    use aq_session_protocol::{ArtifactRef, Digest, Snapshot};
    use aq_terminal_relay::{
        api::{AccessUpdate, OutputReply, ViewQuery},
        draft::PeerUpload,
    };
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'viewer',now())")
        .bind(viewer)
        .bind(format!("{viewer}@fixture.test"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        service::inspect(&f.state, viewer, r.terminal.terminal_id)
            .await
            .is_err()
    );
    let grant = AccessUpdate {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        principal_id: viewer,
        expected_inspection_revision: 0,
        expected_control_revision: 0,
        can_inspect: true,
        can_control: false,
        expires_at_ms: NOW + 20_000,
    };
    service::set_access(&f.state, f.actor, &grant)
        .await
        .unwrap();
    service::inspect(&f.state, viewer, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(
        service::mutate(
            &f.state,
            viewer,
            &service::Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: r.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: 1,
                expected_controller_generation: 0,
                expires_at_ms: NOW + 2_000
            })
        )
        .await
        .is_err()
    );
    let subscription = Uuid::new_v4();
    service::mutate(
        &f.state,
        viewer,
        &service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: subscription,
            expected_session_revision: 1,
            expires_at_ms: NOW + 15_000,
            after_sequence: None,
        }),
    )
    .await
    .unwrap();
    let query = ViewQuery {
        subscription_id: subscription,
        after_sequence: 0,
    };
    assert!(matches!(
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .unwrap(),
        OutputReply::Pending { .. }
    ));
    let raw = b"synthetic private output".to_vec();
    let snapshot = Snapshot {
        snapshot_id: Uuid::new_v4(),
        capture_request_id: Uuid::new_v4(),
        terminal: r.terminal.clone(),
        through_sequence: 0,
        columns: 80,
        rows: 24,
        renderer: ArtifactRef {
            id: Text::new("aq.synthetic.plaintext").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic renderer"),
        },
        digest: Digest::of(&raw),
        state: raw,
    };
    let node = authenticated(&f).await;
    aq_server::terminal::peer::upload(
        &f.state,
        &node,
        &PeerUpload {
            version: 1,
            capability: r.capability.clone(),
            peer: r.peer.clone(),
            exchange_id: Uuid::new_v4(),
            cursor: 0,
            receipts: vec![],
            epoch_receipts: vec![],
            resize_receipts: vec![],
            fence_acks: vec![],
            frames: vec![],
            snapshots: vec![snapshot],
            gaps: vec![],
        },
    )
    .await
    .unwrap();
    f.time
        .store((NOW + 6_000) as i64, std::sync::atomic::Ordering::SeqCst);
    let OutputReply::Ready { view, .. } =
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .unwrap()
    else {
        panic!()
    };
    assert!(view.disconnected); // execution grant expired, historical ACL remains current
    let mut revoke = grant;
    revoke.operation_id = Uuid::new_v4();
    revoke.expected_inspection_revision = 1;
    revoke.expected_control_revision = 1;
    revoke.can_inspect = false;
    service::set_access(&f.state, f.actor, &revoke)
        .await
        .unwrap();
    assert!(
        service::output(&f.state, viewer, r.terminal.terminal_id, &query)
            .await
            .is_err()
    );
    let polled = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    assert!(!polled.effects.iter().any(|e|matches!(e,aq_terminal_relay::draft::Effect::Watch(w) if w.subscription_id==subscription)));
}

#[tokio::test]
async fn actual_http_separates_listeners_csrf_identity_binding_and_sensitive_errors() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let admin = aq_server::build_admin_router(f.state.clone());
    let worker = aq_server::build_worker_router(f.state.clone());
    let path = format!("/api/v1/terminals/{}", r.terminal.terminal_id);
    assert_eq!(
        admin
            .clone()
            .oneshot(
                Request::get(&path)
                    .header("x-aq-dev-email", "owner@localhost")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        worker
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let request = service::Mutation::Control(aq_session_api::AcquireControlRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expected_controller_generation: 0,
        expires_at_ms: NOW + 4_000,
    });
    let path = format!("{path}/mutations");
    let raw = serde_json::to_vec(&request).unwrap();
    let post = |csrf: bool, origin: &str, body: Vec<u8>| {
        let mut b = Request::post(&path)
            .header("content-type", "application/json")
            .header("x-aq-dev-email", "owner@localhost")
            .header("origin", origin);
        if csrf {
            b = b.header("x-aq-request", "1");
        }
        b.body(Body::from(body)).unwrap()
    };
    assert_eq!(
        admin
            .clone()
            .oneshot(post(false, "http://127.0.0.1:8080", raw.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        admin
            .clone()
            .oneshot(post(true, "https://wrong.example", raw.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        admin
            .clone()
            .oneshot(post(true, "http://127.0.0.1:8080", raw))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let response = admin
        .clone()
        .oneshot(post(
            true,
            "http://127.0.0.1:8080",
            br#"{"kind":"input","body":{"bytes":"private raw input sentinel"}}"#.to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        response
            .headers()
            .contains_key("aq-terminal-server-time-ms")
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["code"], "invalid_request");
    assert!(error["request_id"].is_string());
    assert!(!String::from_utf8_lossy(&bytes).contains("sentinel"));
    let register = serde_json::to_vec(&r).unwrap();
    assert_eq!(
        worker
            .clone()
            .oneshot(
                Request::post("/v1/terminal-relay/register")
                    .header("content-type", "application/json")
                    .body(Body::from(register.clone()))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        worker
            .oneshot(
                Request::post("/v1/terminal-relay/register")
                    .header("authorization", format!("Bearer {}", f.credential))
                    .header("content-type", "application/json")
                    .body(Body::from(register))
                    .unwrap()
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

async fn reload_keys(f: &mut Fixture, dir: &std::path::Path, active: &str, ids: &[&str]) {
    let mut cfg = (*f.state.config).clone();
    cfg.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: active.into(),
            keys: ids
                .iter()
                .map(|id| KeyFile {
                    id: (*id).into(),
                    file: dir.join(if *id == "one" { "key" } else { "key-two" }),
                })
                .collect(),
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    f.state.config = Arc::new(cfg);
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
}
#[tokio::test]
async fn actual_stored_ciphertext_requires_old_key_and_rewrites_under_rotated_key() {
    let (mut f, dir, r) = running().await;
    register(&f, &r).await;
    let watch = || {
        service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expires_at_ms: NOW + 10_000,
            after_sequence: None,
        })
    };
    service::mutate(&f.state, f.actor, &watch()).await.unwrap();
    let pool = f.db.pool().await;
    let old:(i32,String,Vec<u8>,Vec<u8>)=sqlx::query_as("SELECT protection_version,key_id,nonce,ciphertext FROM terminal_sessions WHERE terminal_id=$1").bind(r.terminal.terminal_id).fetch_one(&pool).await.unwrap();
    std::fs::write(dir.path().join("key-two"), [42u8; 32]).unwrap();
    std::fs::set_permissions(
        dir.path().join("key-two"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    reload_keys(&mut f, dir.path(), "two", &["two"]).await;
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    reload_keys(&mut f, dir.path(), "two", &["one", "two"]).await;
    service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    service::mutate(&f.state, f.actor, &watch()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT key_id FROM terminal_sessions WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "two"
    );
    reload_keys(&mut f, dir.path(), "two", &["two"]).await;
    service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    // Old authentic ciphertext is still rejected when swapped into a newer revision.
    sqlx::query("UPDATE terminal_sessions SET protection_version=$2,key_id=$3,nonce=$4,ciphertext=$5 WHERE terminal_id=$1").bind(r.terminal.terminal_id).bind(old.0).bind(old.1).bind(old.2).bind(old.3).execute(&pool).await.unwrap();
    reload_keys(&mut f, dir.path(), "two", &["one", "two"]).await;
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn exhausted_general_pool_does_not_consume_terminal_authorization_connections() {
    let (mut f, _dir, r) = running().await;
    register(&f, &r).await;
    let general = f.db.pool().await; // explicit two-connection caller-owned general pool
    f.state.store = aq_server::store::Store::from_pool(general.clone())
        .await
        .unwrap();
    let first = general.acquire().await.unwrap();
    let second = general.acquire().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), general.acquire())
            .await
            .is_err()
    );
    let info = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        service::inspect(&f.state, f.actor, r.terminal.terminal_id),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(info.terminal, r.terminal);
    drop((first, second));
}

#[tokio::test]
async fn owner_install_waits_for_old_transaction_then_fences_old_handlers() {
    use sqlx::Row;
    let (f, dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT value FROM meta WHERE key='terminal_relay_owner' FOR SHARE")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let mut replacement = f.state.clone();
    let mut cfg = (*replacement.config).clone();
    cfg.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: dir.path().join("key"),
            }],
        })
        .unwrap(),
        epoch_input: false,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    replacement.config = Arc::new(cfg);
    let next = replacement.clone();
    let install =
        tokio::spawn(async move { aq_server::terminal::persistence::install(&next).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let waiting=sqlx::query("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND query LIKE 'INSERT INTO meta%' AND wait_event_type='Lock'").fetch_one(&pool).await.unwrap().get::<i64,_>(0);
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "install did not reach observed owner lock wait"
        );
        tokio::task::yield_now().await;
    }
    assert!(!install.is_finished());
    held.commit().await.unwrap();
    install.await.unwrap().unwrap();
    assert!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: r.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: 1,
                expected_controller_generation: 0,
                expires_at_ms: NOW + 2_000
            })
        )
        .await
        .is_err()
    );
    aq_server::terminal::runtime::maintenance(&replacement, Uuid::nil())
        .await
        .unwrap();
    service::inspect(&replacement, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_controller_retries_share_one_durable_lease_and_stale_generation_refuses() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let request = service::Mutation::Control(aq_session_api::AcquireControlRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expected_controller_generation: 0,
        expires_at_ms: NOW + 4_000,
    });
    let (a, b) = tokio::join!(
        service::mutate(&f.state, f.actor, &request),
        service::mutate(&f.state, f.actor, &request)
    );
    assert_eq!(
        serde_json::to_value(a.unwrap()).unwrap(),
        serde_json::to_value(b.unwrap()).unwrap()
    );
    let stale = service::Mutation::Control(aq_session_api::AcquireControlRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        expected_session_revision: 1,
        expected_controller_generation: 0,
        expires_at_ms: NOW + 4_000,
    });
    assert!(service::mutate(&f.state, f.actor, &stale).await.is_err());
    let pool = f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_outbox WHERE terminal_id=$1 AND kind='fence'"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
}
#[tokio::test]
async fn changed_boot_cannot_upload_even_an_empty_batch_or_obtain_old_terminal_effects() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    assert!(
        f.state
            .store
            .execution_register(&aq_worker_protocol::RegisterRequest {
                identity: aq_worker_protocol::NodeIdentity {
                    node_id: node.id(),
                    boot_id: Uuid::new_v4(),
                },
                operation_id: Uuid::new_v4(),
                protocol_version: 2,
                worker_version: "fixture".into(),
                capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
            })
            .await
            .is_err()
    );
    let mut upload = aq_terminal_relay::draft::PeerUpload {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: 0,
        receipts: vec![],
        epoch_receipts: vec![],
        resize_receipts: vec![],
        fence_acks: vec![],
        frames: vec![],
        snapshots: vec![],
        gaps: vec![],
    };
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.peer = Peer::Remote {
        node_id: node.id(),
        boot_id: Uuid::new_v4(),
    };
    assert!(
        aq_server::terminal::peer::upload(&f.state, &node, &upload)
            .await
            .is_err()
    );
    let mut unexpected_poll = polling(&r, 0);
    unexpected_poll.peer = upload.peer.clone();
    assert!(
        aq_server::terminal::peer::poll(&f.state, &node, &unexpected_poll)
            .await
            .is_err()
    );
}

async fn controller_as(
    f: &Fixture,
    r: &Register,
    actor: Uuid,
) -> aq_session_protocol::ControllerLease {
    use aq_terminal_relay::{
        api::MutationReply,
        draft::{Effect, FenceAck, PeerUpload},
    };
    let node = authenticated(f).await;
    let MutationReply::Control { status, .. } = service::mutate(
        &f.state,
        actor,
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
    let fence = aq_server::terminal::peer::poll(&f.state, &node, &polling(r, 0))
        .await
        .unwrap();
    let Some(Effect::Fence(fence)) = fence
        .effects
        .into_iter()
        .find(|e| matches!(e, Effect::Fence(_)))
    else {
        panic!()
    };
    aq_server::terminal::peer::upload(
        &f.state,
        &node,
        &PeerUpload {
            version: 1,
            capability: r.capability.clone(),
            peer: r.peer.clone(),
            exchange_id: Uuid::new_v4(),
            cursor: 0,
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
#[tokio::test]
async fn owner_replacement_recovers_lost_response_unknown_before_any_new_input() {
    use aq_terminal_relay::{api::MutationReply, draft::Effect};
    let (mut f, dir, r) = running().await;
    register(&f, &r).await;
    let request = claimed_input_as(&f, &r, f.actor).await;
    let old = f.state.clone();
    reload_keys(&mut f, dir.path(), "one", &["one"]).await;
    assert!(
        service::mutate(&old, f.actor, &service::Mutation::Input(request.clone()))
            .await
            .is_err()
    );
    assert!(
        service::receipt(
            &f.state,
            f.actor,
            r.terminal.terminal_id,
            request.operation_id
        )
        .await
        .is_err()
    );
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(info.disconnected);
    assert!(!info.control.fence_acknowledged);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    let receipt = service::receipt(
        &f.state,
        f.actor,
        r.terminal.terminal_id,
        request.operation_id,
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
    assert!(receipt.input_paused);
    let node = authenticated(&f).await;
    let poll = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    assert!(
        !poll
            .effects
            .iter()
            .any(|e| matches!(e, Effect::Input(_) | Effect::EpochInput(_)))
    );
    let MutationReply::Input(replayed) =
        service::mutate(&f.state, f.actor, &service::Mutation::Input(request))
            .await
            .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        replayed.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
}

#[tokio::test]
async fn control_only_principal_can_inspect_own_receipt_after_expiry_without_output_access() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'viewer',now())")
        .bind(actor)
        .bind(format!("{actor}@fixture.test"))
        .execute(&pool)
        .await
        .unwrap();
    service::set_access(
        &f.state,
        f.actor,
        &aq_terminal_relay::api::AccessUpdate {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            principal_id: actor,
            expected_inspection_revision: 0,
            expected_control_revision: 0,
            can_inspect: false,
            can_control: true,
            expires_at_ms: NOW + 20000,
        },
    )
    .await
    .unwrap();
    assert!(
        service::inspect(&f.state, actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
    let input = claimed_input_as(&f, &r, actor).await;
    f.time
        .store((NOW + 6000) as i64, std::sync::atomic::Ordering::SeqCst);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    let receipt = service::receipt(&f.state, actor, r.terminal.terminal_id, input.operation_id)
        .await
        .unwrap();
    assert_eq!(
        receipt.disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
    assert!(
        service::inspect(&f.state, actor, r.terminal.terminal_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lower_sealed_input_pending_frame_replay_policy_is_binding_in_sql_and_output() {
    use aq_session_protocol::{
        ArtifactRef, Digest, OutputFrame, Snapshot, TerminalHost, TerminalPolicy,
    };
    use aq_terminal_relay::{
        api::{MutationReply, OutputReply, ViewQuery},
        draft::{Effect, FenceAck, PeerUpload},
    };
    let policy = TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: true,
        max_input_bytes: 2,
        max_pending_input_bytes: 3,
        max_frame_bytes: 2,
        replay_bytes: 64,
    };
    let (f, _dir, r) = running_with_policy(Some(policy.clone())).await;
    register(&f, &r).await;
    assert_eq!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .unwrap()
            .terminal_policy,
        policy
    );
    let node = authenticated(&f).await;
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
    let fence = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap()
        .effects
        .into_iter()
        .find_map(|e| {
            if let Effect::Fence(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let mut upload = PeerUpload {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: 0,
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
    };
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.fence_acks.clear();
    let mut input = aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2000,
        bytes: vec![1; 3],
    };
    assert!(
        service::mutate(&f.state, f.actor, &service::Mutation::Input(input.clone()))
            .await
            .is_err()
    );
    input.bytes = vec![1; 2];
    service::mutate(&f.state, f.actor, &service::Mutation::Input(input.clone()))
        .await
        .unwrap();
    input.operation_id = Uuid::new_v4();
    assert!(
        service::mutate(&f.state, f.actor, &service::Mutation::Input(input))
            .await
            .is_err()
    );
    let subscription = Uuid::new_v4();
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: subscription,
            expected_session_revision: 1,
            expires_at_ms: NOW + 10000,
            after_sequence: None,
        }),
    )
    .await
    .unwrap();
    let snapshot = Snapshot {
        terminal: r.terminal.clone(),
        snapshot_id: Uuid::new_v4(),
        capture_request_id: Uuid::new_v4(),
        through_sequence: 0,
        columns: 80,
        rows: 24,
        renderer: ArtifactRef {
            id: Text::new("synthetic").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"renderer"),
        },
        state: b"ok".to_vec(),
        digest: Digest::of(b"ok"),
    };
    upload.snapshots = vec![snapshot.clone()];
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.snapshots.clear();
    upload.frames = vec![OutputFrame {
        terminal: r.terminal.clone(),
        sequence: 1,
        bytes: vec![1; 3],
        byte_length: 3,
        digest: Digest::of(&[1; 3]),
    }];
    assert!(
        aq_server::terminal::peer::upload(&f.state, &node, &upload)
            .await
            .is_err()
    );
    upload.frames = vec![OutputFrame {
        terminal: r.terminal.clone(),
        sequence: 1,
        bytes: vec![1; 2],
        byte_length: 2,
        digest: Digest::of(&[1; 2]),
    }];
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.frames.clear();
    let mut oversized = snapshot.clone();
    oversized.snapshot_id = Uuid::new_v4();
    oversized.state = vec![1; 65];
    oversized.digest = Digest::of(&oversized.state);
    upload.snapshots = vec![oversized];
    assert!(
        aq_server::terminal::peer::upload(&f.state, &node, &upload)
            .await
            .is_err()
    );
    let OutputReply::Ready {
        snapshot: retained,
        frames,
        ..
    } = service::output(
        &f.state,
        f.actor,
        r.terminal.terminal_id,
        &ViewQuery {
            subscription_id: subscription,
            after_sequence: 0,
        },
    )
    .await
    .unwrap()
    else {
        panic!()
    };
    assert_eq!(retained.snapshot_id, snapshot.snapshot_id);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].bytes.len(), 2);
}
#[tokio::test]
async fn sealed_output_only_terminal_registers_and_watches_but_cannot_acquire_control() {
    let mut policy = aq_session_protocol::TerminalPolicy {
        host: aq_session_protocol::TerminalHost::Tmux,
        input_enabled: false,
        max_input_bytes: 1,
        max_pending_input_bytes: 1,
        max_frame_bytes: 64,
        replay_bytes: 64,
    };
    let (f, _dir, r) = running_with_policy(Some(policy.clone())).await;
    register(&f, &r).await;
    let info = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(!info.terminal_policy.input_enabled);
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::Control(aq_session_api::AcquireControlRequest {
                terminal: r.terminal.clone(),
                operation_id: Uuid::new_v4(),
                expected_session_revision: 1,
                expected_controller_generation: 0,
                expires_at_ms: NOW + 2000
            })
        )
        .await
        .is_err()
    );
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expires_at_ms: NOW + 10000,
            after_sequence: None,
        }),
    )
    .await
    .unwrap();
    policy.input_enabled = true;
    assert_ne!(
        service::inspect(&f.state, f.actor, r.terminal.terminal_id)
            .await
            .unwrap()
            .terminal_policy,
        policy
    );
}

#[tokio::test]
async fn saturated_data_connections_do_not_block_metadata_control_ack_and_input_receipts() {
    use aq_session_protocol::{
        Digest, InputDisposition, InputReceipt, OutputFrame, canonical_digest,
    };
    use aq_terminal_relay::{
        api::MutationReply,
        draft::{Effect, FenceAck, PeerUpload},
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
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
    let poll = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    let fence = poll
        .effects
        .into_iter()
        .find_map(|e| {
            if let Effect::Fence(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let data = f.state.store.terminal_data_pool_for_testing();
    let first = data.acquire().await.unwrap();
    let second = data.acquire().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), data.acquire())
            .await
            .is_err()
    );
    let app = aq_server::build_worker_router(f.state.clone());
    let post = |upload: &PeerUpload| {
        Request::post("/v1/terminal-relay/control-upload")
            .header("authorization", format!("Bearer {}", f.credential))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(upload).unwrap()))
            .unwrap()
    };
    let mut upload = PeerUpload {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: 0,
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
    };
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.clone().oneshot(post(&upload))
        )
        .await
        .unwrap()
        .unwrap()
        .status(),
        StatusCode::OK
    );
    let request = aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2000,
        bytes: b"literal".to_vec(),
    };
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(request.clone()),
    )
    .await
    .unwrap();
    let poll = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    let command = poll
        .effects
        .into_iter()
        .find_map(|e| {
            if let Effect::Input(c) = e {
                Some(c)
            } else {
                None
            }
        })
        .unwrap();
    upload.fence_acks.clear();
    upload.exchange_id = Uuid::new_v4();
    upload.receipts = vec![InputReceipt {
        terminal: r.terminal.clone(),
        operation_id: request.operation_id,
        command_digest: canonical_digest(&command).unwrap(),
        disposition: InputDisposition::Applied,
    }];
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.clone().oneshot(post(&upload))
        )
        .await
        .unwrap()
        .unwrap()
        .status(),
        StatusCode::OK
    );
    let pool = f.db.pool().await;
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT disposition FROM terminal_input_receipts WHERE terminal_id=$1 AND operation_id=$2").bind(r.terminal.terminal_id).bind(request.operation_id).fetch_one(&pool).await.unwrap(),"applied");
    upload.frames = vec![OutputFrame {
        terminal: r.terminal.clone(),
        sequence: 1,
        bytes: vec![1],
        byte_length: 1,
        digest: Digest::of(&[1]),
    }];
    assert_eq!(
        app.oneshot(post(&upload)).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT last_output_at_ms FROM terminal_sessions WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    drop((first, second));
    assert_eq!(
        service::receipt(
            &f.state,
            f.actor,
            r.terminal.terminal_id,
            request.operation_id
        )
        .await
        .unwrap()
        .disposition,
        InputDisposition::Applied
    );
}

#[tokio::test]
async fn observed_held_poll_releases_all_sql_and_common_locks_then_rechecks_revocation_on_wake() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expires_at_ms: NOW + 10000,
            after_sequence: None,
        }),
    )
    .await
    .unwrap();
    let first = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    let mut request = polling(&r, first.cursor);
    request.hold_ms = 20000;
    request.active_subscriptions = 1;
    request.active_controllers = 0;
    let state = f.state.clone();
    let peer = node.clone();
    let held =
        tokio::spawn(async move { aq_server::terminal::peer::poll(&state, &peer, &request).await });
    let config = f.state.config.terminals.as_ref().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !config.runtime.poll_waiting_for_testing(node.id()) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "poll never entered actual wait"
        );
        tokio::task::yield_now().await;
    }
    let pool = f.state.store.terminal_control_pool_for_testing();
    let mut connections = vec![];
    for _ in 0..4 {
        connections.push(
            tokio::time::timeout(std::time::Duration::from_secs(1), pool.acquire())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(!held.is_finished());
    drop(connections);
    let raw = f.db.pool().await;
    let mut tx = raw.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout='1s'")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT value FROM meta WHERE key='terminal_relay_owner' FOR UPDATE")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.id())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let aq_session_protocol::SessionIdentity::Remote { attempt_id, .. } = r.terminal.session else {
        panic!()
    };
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!held.is_finished());
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(node.id())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    config.runtime.wake(node.id());
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), held)
            .await
            .unwrap()
            .unwrap(),
        Err(aq_server::error::AppError::CredentialRevoked)
    ));
}

#[tokio::test]
async fn epoch_receipts_roll_over_bounded_sql_window_and_retired_input_cannot_replay() {
    use aq_session_protocol::{InputDisposition, InputReceipt, canonical_digest};
    use aq_terminal_relay::{
        api::MutationReply,
        draft::{Effect, FenceAck, PeerUpload},
        epoch,
    };
    let (mut f, dir, mut r) = running().await;
    let mut cfg = (*f.state.config).clone();
    cfg.terminals = Some(Arc::new(Loaded {
        keyring: Keyring::load(&ProtectionConfig {
            active_key_id: "one".into(),
            keys: vec![KeyFile {
                id: "one".into(),
                file: dir.path().join("key"),
            }],
        })
        .unwrap(),
        epoch_input: true,
        runtime_id: Uuid::new_v4(),
        runtime: Default::default(),
    }));
    f.state.config = Arc::new(cfg);
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
    r.input_mode = InputMode::EpochV1;
    r.capability = Text::new(epoch::CAPABILITY).unwrap();
    r.advertised_capabilities.push(Capability {
        id: Text::new(epoch::CAPABILITY).unwrap(),
        version: 1,
    });
    register(&f, &r).await;
    let node = authenticated(&f).await;
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
    let fence = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap()
        .effects
        .into_iter()
        .find_map(|e| {
            if let Effect::Fence(f) = e {
                Some(f)
            } else {
                None
            }
        })
        .unwrap();
    let mut upload = PeerUpload {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: 0,
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
    };
    aq_server::terminal::peer::control_upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.fence_acks.clear();
    let MutationReply::Epoch { permit, .. } = service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::OpenEpoch(epoch::Open {
            terminal: r.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expected_controller_generation: lease.generation,
            lease_id: lease.lease_id,
            expires_at_ms: NOW + 2000,
        }),
    )
    .await
    .unwrap() else {
        panic!()
    };
    let mut first = None;
    let mut last_retired = 0;
    for sequence in 1..=80 {
        let request = epoch::Request {
            version: 1,
            capability: Text::new(epoch::CAPABILITY).unwrap(),
            epoch_id: permit.epoch_id,
            sequence,
            input: aq_session_api::InputRequest {
                terminal: r.terminal.clone(),
                operation_id: epoch::operation_id(&permit, sequence).unwrap(),
                lease_id: lease.lease_id,
                expected_controller_generation: lease.generation,
                expected_session_revision: 1,
                expires_at_ms: NOW + 2000,
                bytes: vec![sequence as u8],
            },
        };
        if sequence == 1 {
            first = Some(request.clone());
        }
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::EpochInput(request.clone()),
        )
        .await
        .unwrap();
        let command = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
            .await
            .unwrap()
            .effects
            .into_iter()
            .find_map(|e| {
                if let Effect::EpochInput(c) = e {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(command.sequence, sequence);
        upload.exchange_id = Uuid::new_v4();
        upload.epoch_receipts = vec![epoch::Receipt {
            epoch_id: permit.epoch_id,
            sequence,
            receipt: InputReceipt {
                terminal: r.terminal.clone(),
                operation_id: request.input.operation_id,
                command_digest: canonical_digest(&command.input).unwrap(),
                disposition: InputDisposition::Applied,
            },
        }];
        let ack = aq_server::terminal::peer::control_upload(&f.state, &node, &upload)
            .await
            .unwrap();
        last_retired = ack.retirements[0].retired_through;
    }
    assert_eq!(last_retired, 16);
    let pool = f.db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_input_receipts WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        64
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM terminal_operations WHERE terminal_id=$1"
        )
        .bind(r.terminal.terminal_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        3
    );
    assert!(
        service::mutate(
            &f.state,
            f.actor,
            &service::Mutation::EpochInput(first.unwrap())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn corrupted_plaintext_outbox_claim_and_kind_cannot_redispatch_protected_input() {
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let input = claimed_input_as(&f, &r, f.actor).await;
    let node = authenticated(&f).await;
    let pool = f.db.pool().await;
    sqlx::query("UPDATE terminal_outbox SET claimed=FALSE,kind='watch' WHERE terminal_id=$1 AND operation_id=$2").bind(r.terminal.terminal_id).bind(input.operation_id).execute(&pool).await.unwrap();
    assert!(
        aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
            .await
            .is_err()
    );
    assert!(sqlx::query_scalar::<_,bool>("SELECT dispatched FROM terminal_input_receipts WHERE terminal_id=$1 AND operation_id=$2").bind(r.terminal.terminal_id).bind(input.operation_id).fetch_one(&pool).await.unwrap());
    f.time
        .store((NOW + 2001) as i64, std::sync::atomic::Ordering::SeqCst);
    aq_server::terminal::runtime::maintenance(&f.state, Uuid::nil())
        .await
        .unwrap();
    assert_eq!(
        service::receipt(
            &f.state,
            f.actor,
            r.terminal.terminal_id,
            input.operation_id
        )
        .await
        .unwrap()
        .disposition,
        aq_session_protocol::InputDisposition::DeliveryUnknown
    );
}
#[tokio::test]
async fn execution_cancellation_refuses_positive_controller_fence_queued_before_cancel() {
    let (mut f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    service::mutate(
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
    .unwrap();
    f.profiled.status = Status::Cancelling;
    f.profiled.cancellation_id = Some(Uuid::new_v4());
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_attempts SET state=$2 WHERE attempt_id=$1")
        .bind(f.profiled.offer.binding.attempt_id)
        .bind(serde_json::json!(f.profiled))
        .execute(&pool)
        .await
        .unwrap();
    let poll = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, 0))
        .await
        .unwrap();
    assert!(
        !poll.effects.iter().any(
            |e| matches!(e,aq_terminal_relay::draft::Effect::Fence(f) if f.controller.is_some())
        )
    );
}

async fn sibling_terminal(f: &mut Fixture, first: &Register) -> Register {
    let old_state = f.profiled.clone();
    let old_selection = f.selection.clone();
    let old_decision = f.decision;
    let task = Uuid::new_v4();
    let attempt = Uuid::new_v4();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: f.node.node_id,
        boot_id: f.node.boot_id,
        attempt_id: attempt,
        assignment_generation: 1,
        session_id: Uuid::new_v4(),
    };
    f.selection.lineage.task_id = task;
    f.decision = Uuid::new_v4();
    f.profiled.offer.binding.attempt_id = attempt;
    f.profiled.offer.binding.offer_id = Uuid::new_v4();
    f.profiled.offer.binding.session = session.clone();
    f.profiled.offer.manifest.bindings.session = session.clone();
    f.profiled.offer.manifest.bindings.attempt_id = attempt;
    f.profiled.offer.manifest.bindings.task_id = task;
    f.profiled.offer.manifest.selection_explanation.digest =
        aq_selection::digest(&f.selection).unwrap();
    f.profiled.offer.binding.execution_spec_digest=aq_selection::digest(&serde_json::json!({"schema_version":1,"kind":"synthetic_profiled_authority_fixture","task_id":task})).unwrap();
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    f.profiled.grant.as_mut().unwrap().binding = f.profiled.offer.binding.clone();
    f.profiled.grant.as_mut().unwrap().grant_id = Uuid::new_v4();
    common::profiled::insert_sibling(f).await;
    f.profiled = old_state;
    f.selection = old_selection;
    f.decision = old_decision;
    let mut sibling = first.clone();
    sibling.operation_id = Uuid::new_v4();
    sibling.terminal = TerminalIdentity {
        session,
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    register(f, &sibling).await;
    sibling
}
#[tokio::test]
async fn revoked_earliest_principal_is_durably_rejected_and_other_terminal_dispatches_same_poll() {
    use aq_terminal_relay::draft::Effect;
    let (mut f, _dir, first) = running().await;
    register(&f, &first).await;
    let second = sibling_terminal(&mut f, &first).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'viewer',now())")
        .bind(actor)
        .bind(format!("{actor}@fixture.test"))
        .execute(&pool)
        .await
        .unwrap();
    service::set_access(
        &f.state,
        f.actor,
        &aq_terminal_relay::api::AccessUpdate {
            terminal: first.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            principal_id: actor,
            expected_inspection_revision: 0,
            expected_control_revision: 0,
            can_inspect: false,
            can_control: true,
            expires_at_ms: NOW + 20000,
        },
    )
    .await
    .unwrap();
    let first_lease = controller_as(&f, &first, actor).await;
    let second_lease = controller_as(&f, &second, f.actor).await;
    let denied = literal_request(&first, &first_lease);
    service::mutate(&f.state, actor, &service::Mutation::Input(denied.clone()))
        .await
        .unwrap();
    f.time
        .store((NOW + 1) as i64, std::sync::atomic::Ordering::SeqCst);
    let allowed = literal_request(&second, &second_lease);
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(allowed.clone()),
    )
    .await
    .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(actor)
        .execute(&pool)
        .await
        .unwrap();
    f.time
        .store((NOW + 1) as i64, std::sync::atomic::Ordering::SeqCst);
    let node = authenticated(&f).await;
    let response = aq_server::terminal::peer::poll(&f.state, &node, &polling(&first, 0))
        .await
        .unwrap();
    assert!(
        response
            .effects
            .iter()
            .any(|e| matches!(e,Effect::Input(c) if c.operation_id==allowed.operation_id))
    );
    assert!(
        !response
            .effects
            .iter()
            .any(|e| matches!(e,Effect::Input(c) if c.operation_id==denied.operation_id))
    );
    assert_eq!(
        service::receipt(
            &f.state,
            f.actor,
            first.terminal.terminal_id,
            denied.operation_id
        )
        .await
        .unwrap()
        .disposition,
        aq_session_protocol::InputDisposition::Rejected
    );
}
#[tokio::test]
async fn in_flight_terminal_keeps_its_fifo_but_does_not_block_other_terminal_queue() {
    use aq_terminal_relay::draft::Effect;
    let (mut f, _dir, first) = running().await;
    register(&f, &first).await;
    let second = sibling_terminal(&mut f, &first).await;
    let in_flight = claimed_input_as(&f, &first, f.actor).await;
    let mut behind = in_flight.clone();
    behind.operation_id = Uuid::new_v4();
    service::mutate(&f.state, f.actor, &service::Mutation::Input(behind.clone()))
        .await
        .unwrap();
    f.time
        .store((NOW + 1) as i64, std::sync::atomic::Ordering::SeqCst);
    let other = queued_input_as(&f, &second, f.actor).await;
    let node = authenticated(&f).await;
    let response = aq_server::terminal::peer::poll(&f.state, &node, &polling(&first, 0))
        .await
        .unwrap();
    assert!(
        response
            .effects
            .iter()
            .any(|e| matches!(e,Effect::Input(c) if c.operation_id==other.operation_id))
    );
    assert!(!response.effects.iter().any(|e|matches!(e,Effect::Input(c) if c.operation_id==behind.operation_id||c.operation_id==in_flight.operation_id)));
    let pool = f.db.pool().await;
    assert!(sqlx::query_scalar::<_,bool>("SELECT dispatched FROM terminal_input_receipts WHERE terminal_id=$1 AND operation_id=$2").bind(first.terminal.terminal_id).bind(in_flight.operation_id).fetch_one(&pool).await.unwrap());
    assert!(!sqlx::query_scalar::<_,bool>("SELECT dispatched FROM terminal_input_receipts WHERE terminal_id=$1 AND operation_id=$2").bind(first.terminal.terminal_id).bind(behind.operation_id).fetch_one(&pool).await.unwrap());
}

#[tokio::test]
async fn sibling_attempts_share_node_terminal_subscription_limit() {
    let (mut f, _dir, first) = running().await;
    register(&f, &first).await;
    let mut terminals = vec![first.clone()];
    for _ in 0..8 {
        terminals.push(sibling_terminal(&mut f, &first).await);
    }
    let watch = |terminal: &Register| {
        service::Mutation::Watch(aq_session_api::WatchRequest {
            terminal: terminal.terminal.clone(),
            operation_id: Uuid::new_v4(),
            expected_session_revision: 1,
            expires_at_ms: NOW
                + if terminal.terminal == first.terminal {
                    1_000
                } else {
                    4_000
                },
            after_sequence: None,
        })
    };
    // Eight attempts fill the shared budget with four viewers each. A ninth
    // has no local viewers, so only the node aggregate can refuse #33.
    for terminal in &terminals[..8] {
        for _ in 0..4 {
            service::mutate(&f.state, f.actor, &watch(terminal))
                .await
                .unwrap();
        }
    }
    let pool = f.db.pool().await;
    let before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM terminal_subscriptions WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, 32);
    let error = service::mutate(&f.state, f.actor, &watch(&terminals[8]))
        .await
        .unwrap_err();
    // The relay journal deliberately maps persistence refusal to Internal.
    assert!(
        matches!(error, aq_server::error::AppError::Internal),
        "{error:?}"
    );
    let after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM terminal_subscriptions WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        after, before,
        "refusal must roll back the extra subscription"
    );
    // Free capacity by the injected clock, then the same empty sibling can watch.
    f.time
        .store((NOW + 1_001) as i64, std::sync::atomic::Ordering::SeqCst);
    service::mutate(&f.state, f.actor, &watch(&terminals[8]))
        .await
        .unwrap();
}
