use super::*;

fn epoch_setup() -> (Ledger, Store, Access, Peer, ControllerLease) {
    let l = Ledger::new_epoch(terminal(&remote_peer()), 1, 90000, None).unwrap();
    let mut s = Store::new(&l);
    let a = access(l.terminal(), "alice");
    let p = remote_peer();
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 1300);
    let l = apply(l.open_epoch(&a, id(1303), 1).unwrap(), &mut s).ledger;
    (l, s, a, p, c)
}

#[test]
fn finite_and_epoch_input_name_the_deadline_rule_and_preserve_exact_replay() {
    let t = terminal(&remote_peer());
    let a = access(&t, "alice");
    let l = Ledger::new(t, 1, 90000, None).unwrap();
    let mut store = Store::new(&l);
    let (l, lease) = PeerSession::new(&mut store, &a, &remote_peer()).grant(l, 100, 3000);
    let before = l.encode().unwrap();
    for ttl in [0, 2001] {
        let mut r = input(&l, &lease, 4000, 101);
        r.expires_at_ms = 101 + ttl;
        assert!(matches!(l.input(&a, &r, 101), Err(Error::InputDeadline)));
        assert_eq!(l.encode().unwrap(), before);
    }
    let mut r = input(&l, &lease, 4000, 101);
    r.expires_at_ms = 102;
    assert!(l.input(&a, &r, 101).is_ok());
    r.expires_at_ms = 2101;
    let l = apply(l.input(&a, &r, 101).unwrap(), &mut store).ledger;
    // Same operation replay keeps its original observation after the deadline.
    assert!(
        apply(l.input(&a, &r, 3000).unwrap(), &mut store)
            .effects
            .is_empty()
    );
    let (l, _, a, _, lease) = epoch_setup();
    let mut r = epoch_request(&l, &lease, 1, 10);
    r.input.expires_at_ms = 2011;
    assert!(matches!(
        l.epoch_input(&a, &r, 10),
        Err(Error::InputDeadline)
    ));
    r.input.expires_at_ms = 2010;
    assert!(l.epoch_input(&a, &r, 10).is_ok());
}
fn epoch_request(
    l: &Ledger,
    c: &ControllerLease,
    sequence: u64,
    now: u64,
) -> aq_terminal_relay::epoch::Request {
    let permit = l.epoch_permit().unwrap();
    let operation = aq_terminal_relay::epoch::operation_id(&permit, sequence).unwrap();
    let inner = input(l, c, operation.as_u128(), now);
    aq_terminal_relay::epoch::Request {
        version: 1,
        capability: Text::new(aq_terminal_relay::epoch::CAPABILITY).unwrap(),
        epoch_id: permit.epoch_id,
        sequence,
        input: inner,
    }
}
impl PeerSession<'_> {
    fn apply_epoch_input(
        &mut self,
        l: Ledger,
        r: &aq_terminal_relay::epoch::Request,
        now: u64,
    ) -> Ledger {
        let l = apply(l.epoch_input(self.access, r, now).unwrap(), self.store).ledger;
        let dispatched = apply(
            l.dispatch_epoch_input(self.access, r.input.operation_id, now + 1)
                .unwrap(),
            self.store,
        );
        let Effect::EpochInput(command) = &dispatched.effects[0] else {
            panic!()
        };
        command.validate().unwrap();
        let receipt = aq_session_protocol::InputReceipt {
            operation_id: command.input.operation_id,
            command_digest: canonical_digest(&command.input).unwrap(),
            terminal: l.terminal().clone(),
            disposition: InputDisposition::Applied,
        };
        apply(
            dispatched
                .ledger
                .observe_input(self.peer, &receipt)
                .unwrap(),
            self.store,
        )
        .ledger
    }
}

#[test]
fn epoch_receipt_window_retires_prefix_without_accepting_old_uuid_at_new_sequence() {
    let (mut l, mut s, a, p, c) = epoch_setup();
    let first = epoch_request(&l, &c, 1, 10);
    let mut last = first.clone();
    for sequence in 1..=80 {
        let r = epoch_request(&l, &c, sequence, sequence * 3 + 10);
        l = PeerSession::new(&mut s, &a, &p).apply_epoch_input(l, &r, sequence * 3 + 10);
        last = r;
    }
    assert!(matches!(l.epoch_input(&a, &first, 500), Err(Error::Stale)));
    let mut old_new_seq = epoch_request(&l, &c, 81, 500);
    old_new_seq.input.operation_id = first.input.operation_id;
    assert!(matches!(
        l.epoch_input(&a, &old_new_seq, 500),
        Err(Error::Conflict)
    ));
    let retry = apply(l.epoch_input(&a, &last, 500).unwrap(), &mut s);
    assert!(retry.effects.is_empty());
    let l = retry.ledger;
    let mut changed = last.clone();
    changed.input.bytes.push(1);
    assert!(matches!(
        l.epoch_input(&a, &changed, 501),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        l.input(&a, &last.input, 501),
        Err(Error::Unsupported)
    ));
    assert!(matches!(
        l.dispatch_input(&a, last.input.operation_id, 501),
        Err(Error::Unsupported)
    ));
    let (l, newc) = PeerSession::new(&mut s, &a, &p).grant(l, 600, 1400);
    let l = apply(l.open_epoch(&a, id(1403), 601).unwrap(), &mut s).ledger;
    assert!(matches!(l.epoch_input(&a, &last, 602), Err(Error::Stale)));
    let mut reused = epoch_request(&l, &newc, 1, 602);
    reused.input.operation_id = first.input.operation_id;
    assert!(matches!(
        l.epoch_input(&a, &reused, 602),
        Err(Error::Conflict)
    ));
}
#[test]
fn epoch_lost_admission_ack_cannot_replay_retired_or_undispatched_input_after_restart() {
    let (mut l, mut s, a, p, c) = epoch_setup();
    let mut first = None;
    for seq in 1..=64 {
        let r = epoch_request(&l, &c, seq, seq * 3);
        if seq == 1 {
            first = Some(r.clone())
        }
        l = PeerSession::new(&mut s, &a, &p).apply_epoch_input(l, &r, seq * 3);
    }
    let r = epoch_request(&l, &c, 65, 300);
    s.lost_ack = true;
    assert!(matches!(
        l.epoch_input(&a, &r, 300).unwrap().commit(&mut s),
        Err(Error::Persistence)
    ));
    s.lost_ack = false;
    let recovered = apply(Ledger::recover(&s.bytes).unwrap(), &mut s);
    assert!(recovered.effects.is_empty());
    let l = apply(
        recovered
            .ledger
            .acknowledge_fence(
                &p,
                &FenceAck {
                    terminal: recovered.ledger.terminal().clone(),
                    fence_id: id(1301),
                    generation: 1,
                },
            )
            .unwrap(),
        &mut s,
    )
    .ledger;
    assert!(matches!(
        l.epoch_input(&a, &first.unwrap(), 301),
        Err(Error::Stale)
    ));
    assert_eq!(
        l.input_receipt(&a, &r.input, 301).unwrap().disposition,
        InputDisposition::Expired
    );
    let retry = apply(l.epoch_input(&a, &r, 301).unwrap(), &mut s);
    assert!(retry.effects.is_empty());
    assert!(
        retry
            .ledger
            .dispatch_epoch_input(&a, r.input.operation_id, 301)
            .is_err()
    );
}
#[test]
fn epoch_unknown_survives_restart_and_takeover_without_retirement_or_replay() {
    let (l, mut s, a, p, c) = epoch_setup();
    let r = epoch_request(&l, &c, 1, 10);
    let l = apply(l.epoch_input(&a, &r, 10).unwrap(), &mut s).ledger;
    s.lost_ack = true;
    assert!(matches!(
        l.dispatch_epoch_input(&a, r.input.operation_id, 11)
            .unwrap()
            .commit(&mut s),
        Err(Error::Persistence)
    ));
    s.lost_ack = false;
    let recovered = apply(Ledger::recover(&s.bytes).unwrap(), &mut s).ledger;
    assert!(recovered.paused());
    assert_eq!(
        recovered
            .input_receipt(&a, &r.input, 12)
            .unwrap()
            .disposition,
        InputDisposition::DeliveryUnknown
    );
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(recovered, 20, 1500);
    let l = apply(l.open_epoch(&a, id(1503), 21).unwrap(), &mut s).ledger;
    assert!(l.paused());
    let next = epoch_request(&l, &c, 1, 22);
    assert!(matches!(l.epoch_input(&a, &next, 22), Err(Error::Unknown)));
}
#[test]
fn epoch_uuid_derivation_has_frozen_independently_computed_vectors() {
    let p = aq_terminal_relay::epoch::Permit {
        terminal: TerminalIdentity {
            session: SessionIdentity::Central {
                authority_id: id(1),
                session_id: id(2),
                generation: 3,
            },
            terminal_id: id(4),
            epoch: 5,
        },
        epoch_id: id(6),
        lease_id: id(7),
        controller_generation: 8,
        expires_at_ms: 9,
        derivation_version: 1,
    };
    for (sequence, expected) in [
        (1, "8da947e6-212b-8e9c-b631-f670ca25ba73"),
        (2, "2bb83041-7173-835c-9847-9c25f8d014b8"),
        (u64::MAX, "79e034f4-981d-8eb3-953f-4be646dc30df"),
    ] {
        assert_eq!(
            aq_terminal_relay::epoch::operation_id(&p, sequence)
                .unwrap()
                .to_string(),
            expected
        );
    }
}
