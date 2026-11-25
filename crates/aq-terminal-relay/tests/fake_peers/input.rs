use super::*;

#[test]
fn exact_duplicate_readback_does_not_reinject_or_accept_changed_bytes() {
    let (l, mut s, a, p) = setup(central_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 400);
    let r = input(&l, &c, 410, 10);
    let l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
    let duplicate = apply(l.input(&a, &r, 11).unwrap(), &mut s);
    assert!(duplicate.effects.is_empty());
    let l = duplicate.ledger;
    let mut changed = r.clone();
    changed.bytes.push(1);
    assert!(matches!(l.input(&a, &changed, 12), Err(Error::Conflict)));
    let dispatched = apply(l.dispatch_input(&a, r.operation_id, 12).unwrap(), &mut s);
    let Effect::Input(command) = &dispatched.effects[0] else {
        panic!()
    };
    let receipt = aq_session_protocol::InputReceipt {
        operation_id: r.operation_id,
        command_digest: canonical_digest(command).unwrap(),
        terminal: l.terminal().clone(),
        disposition: InputDisposition::Applied,
    };
    let l = apply(
        dispatched.ledger.observe_input(&p, &receipt).unwrap(),
        &mut s,
    )
    .ledger;
    assert_eq!(
        l.input_receipt(&a, &r, 5000).unwrap().disposition,
        InputDisposition::Applied
    );
    let retry = apply(l.input(&a, &r, 5000).unwrap(), &mut s);
    assert!(retry.effects.is_empty());
    assert!(
        retry
            .ledger
            .dispatch_input(&a, r.operation_id, 5000)
            .is_err()
    );
}
#[test]
fn unknown_survives_new_controller_and_requires_post_unknown_capture() {
    let (l, mut s, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 500);
    let r = input(&l, &c, 510, 10);
    let l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
    let dispatched = apply(l.dispatch_input(&a, r.operation_id, 11).unwrap(), &mut s);
    let Effect::Input(command) = &dispatched.effects[0] else {
        panic!()
    };
    let receipt = aq_session_protocol::InputReceipt {
        operation_id: r.operation_id,
        command_digest: canonical_digest(command).unwrap(),
        terminal: l.terminal().clone(),
        disposition: InputDisposition::DeliveryUnknown,
    };
    let l = apply(
        dispatched.ledger.observe_input(&p, &receipt).unwrap(),
        &mut s,
    )
    .ledger;
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 20, 520);
    assert!(l.paused());
    assert!(matches!(
        l.input(&a, &input(&l, &c, 530, 30), 30),
        Err(Error::Unknown)
    ));
    let old = snapshot(l.terminal(), 540, id(541), 0);
    assert!(matches!(l.observe_capture(&p, &old, 30), Err(Error::Stale)));
    let l = apply(
        l.request_capture(&a, r.operation_id, id(542), c.lease_id, c.generation, 30)
            .unwrap(),
        &mut s,
    )
    .ledger;
    let snapshot = snapshot(l.terminal(), 543, id(542), 0);
    let l = apply(l.observe_capture(&p, &snapshot, 31).unwrap(), &mut s).ledger;
    let req = api::ResolveInputRequest {
        terminal: l.terminal().clone(),
        operation_id: id(544),
        uncertain_operation_id: r.operation_id,
        lease_id: c.lease_id,
        expected_controller_generation: c.generation,
        expected_session_revision: 1,
        fresh_snapshot_id: snapshot.snapshot_id,
        fresh_capture_request_id: snapshot.capture_request_id,
        fresh_snapshot_sequence: 0,
        expires_at_ms: 1000,
    };
    let l = apply(l.resolve(&a, &req, 32).unwrap(), &mut s).ledger;
    assert!(!l.paused());
    assert!(l.input(&a, &input(&l, &c, 545, 33), 33).is_ok());
    assert_eq!(
        l.input_receipt(&a, &r, 34).unwrap().disposition,
        InputDisposition::DeliveryUnknown
    );
    assert!(l.dispatch_input(&a, r.operation_id, 34).is_err());
    assert!(matches!(
        l.request_capture(&a, r.operation_id, id(542), c.lease_id, c.generation, 34),
        Err(Error::Transition) | Err(Error::Conflict)
    ));
}
#[test]
fn restart_expires_undispatched_input_and_never_replays_received_or_injecting() {
    for status in [
        None,
        Some(InputDisposition::Received),
        Some(InputDisposition::Injecting),
    ] {
        let (l, mut s, a, p) = setup(remote_peer());
        let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 600);
        let r = input(&l, &c, 610, 10);
        let mut l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
        if let Some(status) = status {
            let dispatched = apply(l.dispatch_input(&a, r.operation_id, 11).unwrap(), &mut s);
            let Effect::Input(command) = &dispatched.effects[0] else {
                panic!()
            };
            let receipt = aq_session_protocol::InputReceipt {
                operation_id: r.operation_id,
                command_digest: canonical_digest(command).unwrap(),
                terminal: l.terminal().clone(),
                disposition: status,
            };
            l = apply(
                dispatched.ledger.observe_input(&p, &receipt).unwrap(),
                &mut s,
            )
            .ledger;
        }
        let recovered = apply(Ledger::recover(&l.encode().unwrap()).unwrap(), &mut s);
        assert!(recovered.effects.is_empty());
        assert_eq!(
            recovered
                .ledger
                .input_receipt(&a, &r, 12)
                .unwrap()
                .disposition,
            if status.is_none() {
                InputDisposition::Expired
            } else {
                InputDisposition::DeliveryUnknown
            }
        );
    }
}
#[test]
fn pending_bytes_deadlines_and_receipt_identity_are_bounded() {
    let (l, mut s, a, p) = setup(remote_peer());
    let (mut l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 700);
    for i in 0..8 {
        let mut r = input(&l, &c, 710 + i, 10);
        r.bytes = vec![7; 4096];
        l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
    }
    let r = input(&l, &c, 730, 10);
    assert!(matches!(l.input(&a, &r, 10), Err(Error::Capacity)));
    let l = apply(l.expire(1010).unwrap(), &mut s).ledger;
    assert!(!l.paused());
    assert!(l.input(&a, &input(&l, &c, 731, 1011), 1011).is_ok());
    let mut oversized = input(&l, &c, 732, 1011);
    oversized.bytes = vec![0; 4097];
    assert!(matches!(l.input(&a, &oversized, 1011), Err(Error::Invalid)));
    let nil = aq_session_protocol::InputReceipt {
        operation_id: id(710),
        command_digest: Digest::of(b"wrong"),
        terminal: l.terminal().clone(),
        disposition: InputDisposition::Applied,
    };
    assert!(matches!(l.observe_input(&p, &nil), Err(Error::Conflict)));
}
#[test]
fn composed_plans_commit_once_fifo_uses_admission_order_and_revoke_preserves_unknown() {
    let (l, mut s, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 100, 900);
    let first = input(&l, &c, 9999, 101);
    let second = input(&l, &c, 1, 101);
    let compose = || {
        l.input(&a, &first, 101)
            .unwrap()
            .and_then(|next| next.input(&a, &second, 101))
            .unwrap()
    };
    let old = s.bytes.clone();
    s.fail = true;
    assert!(matches!(compose().commit(&mut s), Err(Error::Persistence)));
    assert_eq!(s.bytes, old);
    s.fail = false;
    let l = apply(compose(), &mut s).ledger;
    assert_eq!(
        l.queued_inputs(),
        vec![first.operation_id, second.operation_id]
    );
    assert!(matches!(
        l.dispatch_input(&a, second.operation_id, 102),
        Err(Error::Transition)
    ));
    let l = apply(
        l.dispatch_input(&a, first.operation_id, 102).unwrap(),
        &mut s,
    )
    .ledger;
    let l = apply(l.revoke(id(999)).unwrap(), &mut s).ledger;
    let statuses = l.input_metadata();
    assert_eq!(
        statuses
            .iter()
            .find(|m| m.operation_id == first.operation_id)
            .unwrap()
            .disposition,
        InputDisposition::DeliveryUnknown
    );
    assert_eq!(
        statuses
            .iter()
            .find(|m| m.operation_id == second.operation_id)
            .unwrap()
            .disposition,
        InputDisposition::Rejected
    );
    assert!(l.summary().control.input_paused);
}
#[test]
fn trusted_scope_refusal_rejects_only_never_dispatched_and_keeps_all_replay_fences() {
    let (l, mut store, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut store, &a, &p).grant(l, 100, 5000);
    let first = input(&l, &c, 5100, 101);
    let l = apply(l.input(&a, &first, 101).unwrap(), &mut store).ledger;
    let l = apply(
        l.dispatch_input(&a, first.operation_id, 102).unwrap(),
        &mut store,
    )
    .ledger;
    let second = input(&l, &c, 5101, 102);
    let l = apply(l.input(&a, &second, 102).unwrap(), &mut store).ledger;
    let l = apply(l.reject_queued_for(&a.principal).unwrap(), &mut store).ledger;
    let metadata = l.input_metadata();
    let old = metadata
        .iter()
        .find(|m| m.operation_id == first.operation_id)
        .unwrap();
    assert!(old.dispatched);
    assert_eq!(old.disposition, InputDisposition::Queued);
    assert_eq!(
        l.receipt(&a, second.operation_id, 103).unwrap().disposition,
        InputDisposition::Rejected
    );
    let retried = apply(l.input(&a, &second, 103).unwrap(), &mut store);
    assert!(retried.effects.is_empty());
    assert_eq!(
        retried
            .ledger
            .receipt(&a, second.operation_id, 103)
            .unwrap()
            .disposition,
        InputDisposition::Rejected
    );
    let expired = apply(retried.ledger.expire(1200).unwrap(), &mut store).ledger;
    let refused = apply(expired.reject_queued_for(&a.principal).unwrap(), &mut store).ledger;
    assert_eq!(
        refused
            .receipt(&a, first.operation_id, 1200)
            .unwrap()
            .disposition,
        InputDisposition::DeliveryUnknown
    );
}

#[test]
fn finite_relay_refuses_split_input_before_dispatch() {
    let (ledger, mut store, access, peer) = setup(remote_peer());
    let (ledger, lease) = PeerSession::new(&mut store, &access, &peer).grant(ledger, 0, 800);
    let mut request = input(&ledger, &lease, 810, 10);
    for bytes in [b"\x1b".as_slice(), b"\x1b[<43;37"] {
        request.bytes = bytes.into();
        assert!(matches!(
            ledger.input(&access, &request, 10),
            Err(Error::Invalid)
        ));
    }
    request.bytes = b"\x1b[1;5D".to_vec();
    assert!(ledger.input(&access, &request, 10).is_ok());
    request.bytes = vec![0x1b];
    request.escape_key = true;
    assert!(ledger.input(&access, &request, 10).is_ok());
}

#[test]
fn a_pending_split_packet_does_not_block_ledger_reload_or_dispatch() {
    let (ledger, mut store, access, peer) = setup(remote_peer());
    let (ledger, lease) = PeerSession::new(&mut store, &access, &peer).grant(ledger, 0, 800);
    let request = input(&ledger, &lease, 810, 10);
    let next = apply(ledger.input(&access, &request, 10).unwrap(), &mut store).ledger;
    let mut stored: serde_json::Value = serde_json::from_slice(&next.encode().unwrap()).unwrap();
    let packet = &mut stored["inputs"][request.operation_id.to_string()];
    packet["command"]["bytes"] = serde_json::json!([27]);
    packet["byte_count"] = 1.into();
    let mut command = packet["command"].clone();
    command.sort_all_objects();
    packet["command_digest"] =
        serde_json::to_value(Digest::of(&serde_json::to_vec(&command).unwrap())).unwrap();
    let ledger = Ledger::load(&serde_json::to_vec(&stored).unwrap()).unwrap();
    assert!(matches!(
        ledger.dispatch_input(&access, request.operation_id, 11),
        Err(Error::Transition)
    ));
    let expired = apply(ledger.expire(11).unwrap(), &mut store).ledger;
    assert_eq!(
        expired
            .receipt(&access, request.operation_id, 11)
            .unwrap()
            .disposition,
        InputDisposition::Rejected
    );
}
