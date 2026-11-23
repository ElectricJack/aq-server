use super::*;

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
async fn lower_sealed_input_pending_frame_replay_policy_is_binding_in_sql_and_output() {
    use aq_session_protocol::{Digest, TerminalHost, TerminalPolicy};
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
    let lease = acquire_control(&f, &r, f.actor).await.lease.unwrap();
    let (_, fence) = pending_fence(&f, &r, &node).await;
    let mut upload = fence_upload(&r, 0, &fence);
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.fence_acks.clear();
    assert_pending_input_limit(&f, &r, &lease).await;
    let subscription = watch(&f, &r, f.actor, 10_000).await;
    let snapshot = retained_snapshot(&r);
    upload.snapshots = vec![snapshot.clone()];
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    upload.snapshots.clear();
    upload.frames = vec![output_frame(&r, &[1; 3])];
    assert!(
        aq_server::terminal::peer::upload(&f.state, &node, &upload)
            .await
            .is_err()
    );
    upload.frames = vec![output_frame(&r, &[1; 2])];
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
    assert_retained_output(&f, &r, subscription, snapshot.snapshot_id).await;
}

#[tokio::test]
async fn saturated_data_connections_do_not_block_metadata_control_ack_and_input_receipts() {
    use aq_session_protocol::{InputDisposition, InputReceipt, canonical_digest};
    use aq_terminal_relay::draft::PeerUpload;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    let lease = acquire_control(&f, &r, f.actor).await.lease.unwrap();
    let (_, fence) = pending_fence(&f, &r, &node).await;
    let held = exhaust_data_pool(&f).await;
    let app = aq_server::build_worker_router(f.state.clone());
    let post = |upload: &PeerUpload| {
        Request::post("/v1/terminal-relay/control-upload")
            .header("authorization", format!("Bearer {}", f.credential))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(upload).unwrap()))
            .unwrap()
    };
    let mut upload = fence_upload(&r, 0, &fence);
    assert_control_upload(&app, post(&upload)).await;
    let (request, command) = claim_input(&f, &r, &node, &lease).await;
    upload.fence_acks.clear();
    upload.exchange_id = Uuid::new_v4();
    upload.receipts = vec![InputReceipt {
        terminal: r.terminal.clone(),
        operation_id: request.operation_id,
        command_digest: canonical_digest(&command).unwrap(),
        disposition: InputDisposition::Applied,
    }];
    assert_control_upload(&app, post(&upload)).await;
    let pool = f.db.pool().await;
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT disposition FROM terminal_input_receipts WHERE terminal_id=$1 AND operation_id=$2").bind(r.terminal.terminal_id).bind(request.operation_id).fetch_one(&pool).await.unwrap(),"applied");
    upload.frames = vec![output_frame(&r, &[1])];
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
    drop(held);
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
    watch(&f, &r, f.actor, 10_000).await;
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
    let connections = acquire_all_control_connections(&f).await;
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

fn output_frame(r: &Register, bytes: &[u8]) -> aq_session_protocol::OutputFrame {
    aq_session_protocol::OutputFrame {
        terminal: r.terminal.clone(),
        sequence: 1,
        bytes: bytes.to_vec(),
        byte_length: bytes.len() as u32,
        digest: aq_session_protocol::Digest::of(bytes),
    }
}

async fn assert_pending_input_limit(
    f: &Fixture,
    r: &Register,
    lease: &aq_session_protocol::ControllerLease,
) {
    let mut input = aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2000,
        bytes: vec![1; 3],
        escape_key: false,
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
}

fn retained_snapshot(r: &Register) -> aq_session_protocol::Snapshot {
    use aq_session_protocol::{ArtifactRef, Digest, Snapshot};
    Snapshot {
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
    }
}

async fn exhaust_data_pool(f: &Fixture) -> Vec<sqlx::pool::PoolConnection<sqlx::Postgres>> {
    let data = f.state.store.terminal_data_pool_for_testing();
    let mut held = Vec::new();
    for _ in 0..data.options().get_max_connections() {
        held.push(data.acquire().await.unwrap());
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), data.acquire())
            .await
            .is_err()
    );
    held
}

async fn claim_input(
    f: &Fixture,
    r: &Register,
    node: &aq_server::terminal::peer::AuthenticatedNode,
    lease: &aq_session_protocol::ControllerLease,
) -> (
    aq_session_api::InputRequest,
    aq_session_protocol::InputCommand,
) {
    use aq_terminal_relay::draft::Effect;
    let request = aq_session_api::InputRequest {
        terminal: r.terminal.clone(),
        operation_id: Uuid::new_v4(),
        lease_id: lease.lease_id,
        expected_controller_generation: lease.generation,
        expected_session_revision: 1,
        expires_at_ms: NOW + 2000,
        bytes: b"literal".to_vec(),
        escape_key: false,
    };
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(request.clone()),
    )
    .await
    .unwrap();
    let poll = aq_server::terminal::peer::poll(&f.state, node, &polling(r, 0))
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
    (request, command)
}

async fn acquire_all_control_connections(
    f: &Fixture,
) -> Vec<sqlx::pool::PoolConnection<sqlx::Postgres>> {
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
    connections
}

async fn assert_retained_output(
    f: &Fixture,
    r: &Register,
    subscription: Uuid,
    expected_snapshot: Uuid,
) {
    use aq_terminal_relay::api::{OutputReply, ViewQuery};
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
    assert_eq!(retained.snapshot_id, expected_snapshot);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].bytes.len(), 2);
}

async fn assert_control_upload(app: &axum::Router, request: axum::http::Request<axum::body::Body>) {
    use axum::http::StatusCode;
    use tower::ServiceExt;
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.clone().oneshot(request)
        )
        .await
        .unwrap()
        .unwrap()
        .status(),
        StatusCode::OK
    );
}
