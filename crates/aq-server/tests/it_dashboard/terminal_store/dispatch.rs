use super::*;

#[tokio::test]
async fn lost_poll_response_never_redispatches_input_and_expiry_is_durable_unknown() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay::{api::MutationReply, draft::Effect};
    let (f, _dir, r) = running().await;
    register(&f, &r).await;
    let node = authenticated(&f).await;
    let status = acquire_control(&f, &r, f.actor).await;
    let lease = status.lease.unwrap();
    assert!(!status.fence_acknowledged);
    let (cursor, fence) = pending_fence(&f, &r, &node).await;
    let upload = fence_upload(&r, cursor, &fence);
    aq_server::terminal::peer::upload(&f.state, &node, &upload)
        .await
        .unwrap();
    let request = literal_request(&r, &lease);
    service::mutate(
        &f.state,
        f.actor,
        &service::Mutation::Input(request.clone()),
    )
    .await
    .unwrap();
    let claimed = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, cursor))
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
    let retry = aq_server::terminal::peer::poll(&f.state, &node, &polling(&r, cursor))
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
    assert_unknown_persisted(&f, &r).await;
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
    let mut upload = empty_upload(&r, 0);
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
        &service::Mutation::Control(control_request(&r, 4_000)),
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
    let actor = new_viewer(&pool).await;
    service::set_access(&f.state, f.actor, &Grant::Control.for_actor(&first, actor))
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

async fn assert_unknown_persisted(f: &Fixture, r: &Register) {
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
