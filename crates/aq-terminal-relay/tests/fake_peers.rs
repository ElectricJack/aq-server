use aq_session_api as api;
use aq_session_protocol::*;
use aq_terminal_relay::{draft::*, output::*, *};
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn terminal(central: bool) -> TerminalIdentity {
    TerminalIdentity {
        session: if central {
            SessionIdentity::Central {
                authority_id: id(1),
                session_id: id(2),
                generation: 1,
            }
        } else {
            SessionIdentity::Remote {
                node_id: id(1),
                boot_id: id(2),
                attempt_id: id(3),
                assignment_generation: 1,
                session_id: id(4),
            }
        },
        terminal_id: id(5),
        epoch: 1,
    }
}
fn peer(central: bool) -> Peer {
    if central {
        Peer::Central {
            authority_id: id(1),
        }
    } else {
        Peer::Remote {
            node_id: id(1),
            boot_id: id(2),
        }
    }
}
fn access(t: &TerminalIdentity, name: &str) -> Access {
    Access {
        principal: Text::new(name).unwrap(),
        terminal: t.clone(),
        session_revision: 1,
        watch: true,
        control: true,
        valid_until_ms: 100000,
        execution_active: true,
        execution_expires_at_ms: 50000,
    }
}
struct Store {
    bytes: Vec<u8>,
    revision: u64,
    fail: bool,
    lost_ack: bool,
    outbox: Vec<Effect>,
}
impl Store {
    fn new(l: &Ledger) -> Self {
        Self {
            bytes: l.encode().unwrap(),
            revision: l.revision(),
            fail: false,
            lost_ack: false,
            outbox: vec![],
        }
    }
}
impl Journal for Store {
    fn commit(
        &mut self,
        _: &TerminalIdentity,
        expected: u64,
        next: &[u8],
        outbox: &[Effect],
    ) -> aq_terminal_relay::Result<()> {
        if self.fail {
            return Err(Error::Persistence);
        }
        if expected != self.revision {
            return Err(Error::Stale);
        }
        let ledger: Ledger = serde_json::from_slice(next).unwrap();
        self.revision = ledger.revision();
        self.bytes = next.to_vec();
        self.outbox = outbox.to_vec();
        if self.lost_ack {
            return Err(Error::Persistence);
        }
        Ok(())
    }
}
fn apply(plan: Plan, store: &mut Store) -> Committed {
    plan.commit(store).unwrap()
}
fn setup(central: bool) -> (Ledger, Store, Access, Peer) {
    let l = Ledger::new(terminal(central), 1, 90000, None).unwrap();
    let s = Store::new(&l);
    let a = access(l.terminal(), "alice");
    (l, s, a, peer(central))
}
fn grant(
    l: Ledger,
    s: &mut Store,
    a: &Access,
    p: &Peer,
    now: u64,
    n: u128,
) -> (Ledger, ControllerLease) {
    let req = api::AcquireControlRequest {
        terminal: l.terminal().clone(),
        operation_id: id(n),
        expected_session_revision: 1,
        expected_controller_generation: l.generation(),
        expires_at_ms: now + 10000,
    };
    let c = apply(l.acquire(a, &req, id(n + 1), id(n + 2), now).unwrap(), s);
    let Effect::Fence(f) = c.effects[0].clone() else {
        panic!()
    };
    let lease = f.controller.clone().unwrap();
    let c = apply(
        c.ledger
            .acknowledge_fence(
                p,
                &FenceAck {
                    terminal: f.terminal,
                    fence_id: f.fence_id,
                    generation: f.generation,
                },
            )
            .unwrap(),
        s,
    );
    (c.ledger, lease)
}
fn input(l: &Ledger, c: &ControllerLease, n: u128, now: u64) -> api::InputRequest {
    api::InputRequest {
        terminal: l.terminal().clone(),
        operation_id: id(n),
        lease_id: c.lease_id,
        expected_controller_generation: c.generation,
        expected_session_revision: 1,
        expires_at_ms: now + 1000,
        bytes: b"literal\n".to_vec(),
    }
}
fn snapshot(t: &TerminalIdentity, n: u128, capture: Uuid, through: u64) -> Snapshot {
    let state = b"synthetic screen".to_vec();
    Snapshot {
        snapshot_id: id(n),
        capture_request_id: capture,
        terminal: t.clone(),
        through_sequence: through,
        columns: 80,
        rows: 24,
        renderer: ArtifactRef {
            id: Text::new("synthetic-renderer").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"renderer"),
        },
        digest: Digest::of(&state),
        state,
    }
}
fn frame(t: &TerminalIdentity, n: u64, bytes: Vec<u8>) -> OutputFrame {
    OutputFrame {
        terminal: t.clone(),
        sequence: n,
        byte_length: bytes.len() as u32,
        digest: Digest::of(&bytes),
        bytes,
    }
}
#[test]
fn remote_and_central_share_scope_and_exact_identity_checks() {
    for central in [false, true] {
        let (l, mut s, a, p) = setup(central);
        let mut viewer = a.clone();
        viewer.control = false;
        let w = api::WatchRequest {
            terminal: l.terminal().clone(),
            operation_id: id(20),
            expected_session_revision: 1,
            expires_at_ms: 5000,
            after_sequence: None,
        };
        let l = apply(l.watch(&viewer, &w, 0).unwrap(), &mut s).ledger;
        assert!(l.authorize_watch(&viewer, w.operation_id, 1).is_ok());
        let r = api::AcquireControlRequest {
            terminal: l.terminal().clone(),
            operation_id: id(21),
            expected_session_revision: 1,
            expected_controller_generation: 0,
            expires_at_ms: 1000,
        };
        assert!(matches!(
            l.acquire(&viewer, &r, id(22), id(23), 0),
            Err(Error::Forbidden)
        ));
        let (l, c) = grant(l, &mut s, &a, &p, 10, 30);
        let mut r = input(&l, &c, 40, 20);
        let original = r.terminal.clone();
        r.terminal.epoch += 1;
        assert!(matches!(l.input(&a, &r, 20), Err(Error::Stale)));
        r.terminal = original;
        let wrong = if central {
            Peer::Central {
                authority_id: id(99),
            }
        } else {
            Peer::Remote {
                node_id: id(1),
                boot_id: id(99),
            }
        };
        assert!(matches!(
            l.acknowledge_fence(
                &wrong,
                &FenceAck {
                    terminal: l.terminal().clone(),
                    fence_id: id(31),
                    generation: 1
                }
            ),
            Err(Error::Stale)
        ));
        let mut stale = a.clone();
        stale.session_revision = 2;
        assert!(matches!(l.input(&stale, &r, 20), Err(Error::Stale)));
    }
}
#[test]
fn commit_failure_and_lost_ack_never_expose_dispatch_effects() {
    let (l, mut s, a, p) = setup(false);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 100);
    let r = input(&l, &c, 110, 10);
    s.fail = true;
    assert!(matches!(
        l.input(&a, &r, 10).unwrap().commit(&mut s),
        Err(Error::Persistence)
    ));
    assert!(matches!(l.input_receipt(&a, &r, 11), Err(Error::Stale)));
    s.fail = false;
    let l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
    s.lost_ack = true;
    assert!(matches!(
        l.dispatch_input(&a, r.operation_id, 11)
            .unwrap()
            .commit(&mut s),
        Err(Error::Persistence)
    ));
    s.lost_ack = false;
    let recovered = apply(Ledger::recover(&s.bytes).unwrap(), &mut s);
    assert!(recovered.effects.is_empty());
    assert_eq!(
        recovered
            .ledger
            .input_receipt(&a, &r, 12)
            .unwrap()
            .disposition,
        InputDisposition::DeliveryUnknown
    );
    assert!(recovered.ledger.paused());
}
#[test]
fn takeover_needs_exact_fence_ack_and_revocation_denies_new_input() {
    let (l, mut s, a, p) = setup(false);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 200);
    let bob = access(l.terminal(), "bob");
    let request = api::AcquireControlRequest {
        terminal: l.terminal().clone(),
        operation_id: id(210),
        expected_session_revision: 1,
        expected_controller_generation: 1,
        expires_at_ms: 12000,
    };
    let next = apply(
        l.acquire(&bob, &request, id(211), id(212), 100).unwrap(),
        &mut s,
    );
    let Effect::Fence(f) = next.effects[0].clone() else {
        panic!()
    };
    let lease = f.controller.unwrap();
    assert!(matches!(
        next.ledger
            .input(&bob, &input(&next.ledger, &lease, 220, 101), 101),
        Err(Error::PendingFence)
    ));
    assert!(matches!(
        next.ledger
            .input(&a, &input(&next.ledger, &c, 221, 101), 101),
        Err(Error::Stale)
    ));
    assert!(matches!(
        next.ledger.acknowledge_fence(
            &p,
            &FenceAck {
                terminal: next.ledger.terminal().clone(),
                fence_id: id(201),
                generation: 1
            }
        ),
        Err(Error::Stale)
    ));
    let l = apply(
        next.ledger
            .acknowledge_fence(
                &p,
                &FenceAck {
                    terminal: next.ledger.terminal().clone(),
                    fence_id: id(211),
                    generation: 2,
                },
            )
            .unwrap(),
        &mut s,
    )
    .ledger;
    assert!(l.input(&bob, &input(&l, &lease, 222, 102), 102).is_ok());
    let l = apply(l.revoke(id(230)).unwrap(), &mut s).ledger;
    assert!(matches!(
        l.input(&bob, &input(&l, &lease, 223, 103), 103),
        Err(Error::Stale)
    ));
}
#[test]
fn conservative_expiry_requires_explicit_clock_margin_and_never_renews_execution() {
    let t = terminal(false);
    let mut a = access(&t, "alice");
    a.execution_expires_at_ms = 20000;
    let l = Ledger::new(t, 1, 50000, Some(500)).unwrap();
    let mut s = Store::new(&l);
    let p = peer(false);
    let (l, _) = grant(l, &mut s, &a, &p, 0, 300);
    let request = api::AcquireControlRequest {
        terminal: l.terminal().clone(),
        operation_id: id(310),
        expected_session_revision: 1,
        expected_controller_generation: 1,
        expires_at_ms: 19000,
    };
    let next = apply(
        l.acquire(&a, &request, id(311), id(312), 1000).unwrap(),
        &mut s,
    );
    let Effect::Fence(f) = next.effects[0].clone() else {
        panic!()
    };
    let c = f.controller.unwrap();
    assert_eq!(c.execution_expires_at_ms, 20000);
    assert_eq!(c.expires_at_ms, 16000);
    assert!(matches!(
        next.ledger
            .input(&a, &input(&next.ledger, &c, 320, 10499), 10499),
        Err(Error::PendingFence)
    ));
    assert!(
        next.ledger
            .input(&a, &input(&next.ledger, &c, 321, 10500), 10500)
            .is_ok()
    );
    let mut expired = a.clone();
    expired.execution_active = false;
    assert!(matches!(
        next.ledger
            .input(&expired, &input(&next.ledger, &c, 322, 10501), 10501),
        Err(Error::Expired)
    ));
}
#[test]
fn exact_duplicate_readback_does_not_reinject_or_accept_changed_bytes() {
    let (l, mut s, a, p) = setup(true);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 400);
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
    let (l, mut s, a, p) = setup(false);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 500);
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
    let (l, c) = grant(l, &mut s, &a, &p, 20, 520);
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
        let (l, mut s, a, p) = setup(false);
        let (l, c) = grant(l, &mut s, &a, &p, 0, 600);
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
    let (l, mut s, a, p) = setup(false);
    let (mut l, c) = grant(l, &mut s, &a, &p, 0, 700);
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
fn resize_id_history_is_immutable_after_intervening_operations() {
    let (l, mut s, a, p) = setup(false);
    let (mut l, c) = grant(l, &mut s, &a, &p, 0, 800);
    let original = api::ResizeRequest {
        terminal: l.terminal().clone(),
        operation_id: id(810),
        lease_id: c.lease_id,
        expected_controller_generation: c.generation,
        expected_session_revision: 1,
        expires_at_ms: 1000,
        sequence: 1,
        columns: 80,
        rows: 24,
    };
    l = apply(l.resize(&a, &original, 10).unwrap(), &mut s).ledger;
    let mut second = original.clone();
    second.operation_id = id(811);
    second.sequence = 2;
    second.columns = 90;
    l = apply(l.resize(&a, &second, 11).unwrap(), &mut s).ledger;
    let mut changed = original.clone();
    changed.sequence = 3;
    changed.columns = 100;
    assert!(matches!(l.resize(&a, &changed, 12), Err(Error::Conflict)));
    assert!(
        apply(l.resize(&a, &original, 12).unwrap(), &mut s)
            .effects
            .is_empty()
    );
    let mut stale = second;
    stale.operation_id = id(812);
    stale.sequence = 1;
    assert!(matches!(l.resize(&a, &stale, 13), Err(Error::Stale)));
}
#[test]
fn viewers_are_scoped_independent_and_bounded() {
    let (mut l, mut s, a, _) = setup(false);
    for i in 0..4 {
        let who = access(l.terminal(), &format!("viewer-{i}"));
        let r = api::WatchRequest {
            terminal: l.terminal().clone(),
            operation_id: id(900 + i),
            expected_session_revision: 1,
            expires_at_ms: 1000,
            after_sequence: None,
        };
        l = apply(l.watch(&who, &r, 0).unwrap(), &mut s).ledger;
        assert!(l.authorize_watch(&who, r.operation_id, 1).is_ok());
        assert!(matches!(
            l.authorize_watch(&a, r.operation_id, 1),
            Err(Error::Forbidden)
        ));
    }
    let request = api::WatchRequest {
        terminal: l.terminal().clone(),
        operation_id: id(910),
        expected_session_revision: 1,
        expires_at_ms: 1000,
        after_sequence: None,
    };
    assert!(matches!(l.watch(&a, &request, 1), Err(Error::Capacity)));
    assert!(matches!(
        l.authorize_watch(&a, id(900), 1000),
        Err(Error::Forbidden) | Err(Error::Expired)
    ));
}
#[test]
fn replay_eviction_missing_ranges_barriers_and_epoch_fences_are_explicit() {
    let (l, mut s, a, p) = setup(false);
    let request = api::WatchRequest {
        terminal: l.terminal().clone(),
        operation_id: id(1000),
        expected_session_revision: 1,
        expires_at_ms: 10000,
        after_sequence: None,
    };
    let l = apply(l.watch(&a, &request, 0).unwrap(), &mut s).ledger;
    let mut ring = Replay::new(l.terminal().clone(), 64).unwrap();
    let f = frame(l.terminal(), 1, vec![1; 30]);
    assert!(ring.frame(&p, f.clone(), 1).unwrap());
    assert!(!ring.frame(&p, f.clone(), 2).unwrap());
    let mut bad = f;
    bad.bytes[0] = 2;
    bad.digest = Digest::of(&bad.bytes);
    assert!(matches!(ring.frame(&p, bad, 2), Err(Error::Conflict)));
    ring.frame(&p, frame(l.terminal(), 3, vec![3; 30]), 3)
        .unwrap();
    ring.frame(&p, frame(l.terminal(), 4, vec![4; 30]), 4)
        .unwrap();
    assert!(ring.retained_bytes() <= 64);
    let view = ring.view(&l, &a, request.operation_id, 0, 5).unwrap();
    assert_eq!(view.frames.len(), 2);
    assert!(
        view.gaps
            .iter()
            .any(|g| g.first_missing == 1 && g.last_missing >= 2)
    );
    assert!(view.truncated);
    let snap = snapshot(l.terminal(), 1010, id(1011), 3);
    ring.snapshot(&p, snap.clone(), 6).unwrap();
    let view = ring.view(&l, &a, request.operation_id, 0, 7).unwrap();
    assert_eq!(
        view.frames.iter().map(|f| f.sequence).collect::<Vec<_>>(),
        vec![4]
    );
    assert!(view.gaps.is_empty());
    assert!(!view.truncated);
    let mut changed = snap.clone();
    changed.state.push(1);
    changed.digest = Digest::of(&changed.state);
    assert!(matches!(
        ring.snapshot(&p, changed, 8),
        Err(Error::Conflict)
    ));
    let mut stale = frame(l.terminal(), 5, vec![5]);
    stale.terminal.epoch += 1;
    assert!(matches!(ring.frame(&p, stale, 8), Err(Error::Stale)));
    ring.disconnect();
    assert!(
        ring.view(&l, &a, request.operation_id, 0, 100)
            .unwrap()
            .disconnected
    );
}
#[test]
fn many_tiny_frames_and_many_gaps_do_not_create_unbounded_metadata() {
    let (l, mut s, a, p) = setup(true);
    let r = api::WatchRequest {
        terminal: l.terminal().clone(),
        operation_id: id(1100),
        expected_session_revision: 1,
        expires_at_ms: 10000,
        after_sequence: None,
    };
    let l = apply(l.watch(&a, &r, 0).unwrap(), &mut s).ledger;
    let mut ring = Replay::new(l.terminal().clone(), 1024 * 1024).unwrap();
    for i in 1..1100 {
        ring.frame(&p, frame(l.terminal(), i * 2, vec![1]), i)
            .unwrap();
    }
    let view = ring.view(&l, &a, r.operation_id, 0, 2000).unwrap();
    assert!(view.frames.len() <= 1024);
    assert!(view.gaps.len() <= 64);
    assert!(view.truncated);
    assert!(matches!(
        ring.frame(&p, frame(l.terminal(), 2, vec![1]), 2001),
        Err(Error::Stale)
    ));
}
#[test]
fn terminal_saturation_cannot_consume_reserved_execution_capacity() {
    let mut pools = Capacity::new(2, 3).unwrap();
    pools.acquire(Class::Terminal).unwrap();
    pools.acquire(Class::Terminal).unwrap();
    assert!(matches!(
        pools.acquire(Class::Terminal),
        Err(Error::Capacity)
    ));
    for _ in 0..3 {
        pools.acquire(Class::Execution).unwrap();
    }
    assert!(matches!(
        pools.acquire(Class::Execution),
        Err(Error::Capacity)
    ));
    pools.release(Class::Execution).unwrap();
    pools.acquire(Class::Execution).unwrap();
}
#[test]
fn draft_negotiation_and_peer_binding_are_exact_and_old_documents_are_unchanged() {
    let capability = Capability {
        id: Text::new(CAPABILITY).unwrap(),
        version: 1,
    };
    negotiate(std::slice::from_ref(&capability)).unwrap();
    let mut wrong = capability.clone();
    wrong.version = 2;
    assert!(negotiate(&[wrong]).is_err());
    assert!(negotiate(&[capability.clone(), capability]).is_err());
    let envelope = Envelope {
        version: 1,
        capability: Text::new(CAPABILITY).unwrap(),
        peer: peer(true),
        exchange_id: id(1200),
        cursor: 1,
        effects: vec![Effect::Fence(Fence {
            terminal: terminal(true),
            fence_id: id(1201),
            generation: 1,
            controller: None,
        })],
    };
    envelope.validate().unwrap();
    let mut value = serde_json::to_value(&envelope).unwrap();
    value["unknown_mutation"] = serde_json::json!(true);
    assert!(aq_terminal_relay::draft::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut wrong = envelope;
    wrong.peer = peer(false);
    assert!(wrong.validate().is_err());
}
fn epoch_setup() -> (Ledger, Store, Access, Peer, ControllerLease) {
    let l = Ledger::new_epoch(terminal(false), 1, 90000, None).unwrap();
    let mut s = Store::new(&l);
    let a = access(l.terminal(), "alice");
    let p = peer(false);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 1300);
    let l = apply(l.open_epoch(&a, id(1303), 1).unwrap(), &mut s).ledger;
    (l, s, a, p, c)
}

#[test]
fn finite_and_epoch_input_name_the_deadline_rule_and_preserve_exact_replay() {
    let t = terminal(false);
    let a = access(&t, "alice");
    let l = Ledger::new(t, 1, 90000, None).unwrap();
    let mut store = Store::new(&l);
    let (l, lease) = grant(l, &mut store, &a, &peer(false), 100, 3000);
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
fn epoch_apply(
    l: Ledger,
    s: &mut Store,
    a: &Access,
    p: &Peer,
    r: &aq_terminal_relay::epoch::Request,
    now: u64,
) -> Ledger {
    let l = apply(l.epoch_input(a, r, now).unwrap(), s).ledger;
    let dispatched = apply(
        l.dispatch_epoch_input(a, r.input.operation_id, now + 1)
            .unwrap(),
        s,
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
    apply(dispatched.ledger.observe_input(p, &receipt).unwrap(), s).ledger
}
#[test]
fn epoch_receipt_window_retires_prefix_without_accepting_old_uuid_at_new_sequence() {
    let (mut l, mut s, a, p, c) = epoch_setup();
    let first = epoch_request(&l, &c, 1, 10);
    let mut last = first.clone();
    for sequence in 1..=80 {
        let r = epoch_request(&l, &c, sequence, sequence * 3 + 10);
        l = epoch_apply(l, &mut s, &a, &p, &r, sequence * 3 + 10);
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
    let (l, newc) = grant(l, &mut s, &a, &p, 600, 1400);
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
        l = epoch_apply(l, &mut s, &a, &p, &r, seq * 3);
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
    let (l, c) = grant(recovered, &mut s, &a, &p, 20, 1500);
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
#[test]
fn duplicate_noop_revisions_and_authority_rebind_do_not_replay_or_extend_execution() {
    let (l, mut s, a, p) = setup(false);
    let (l, c) = grant(l, &mut s, &a, &p, 0, 1600);
    let r = input(&l, &c, 1610, 10);
    let l = apply(l.input(&a, &r, 10).unwrap(), &mut s).ledger;
    let revision = l.revision();
    let l = apply(l.input(&a, &r, 11).unwrap(), &mut s).ledger;
    assert_eq!(l.revision(), revision);
    let dispatched = apply(l.dispatch_input(&a, r.operation_id, 12).unwrap(), &mut s);
    let rebound = apply(
        dispatched
            .ledger
            .rebind_authority(2, 90000, id(1611))
            .unwrap(),
        &mut s,
    );
    assert!(rebound.ledger.paused());
    assert!(matches!(
        rebound.effects[0],
        Effect::Fence(Fence {
            controller: None,
            ..
        })
    ));
    assert!(matches!(
        rebound.ledger.input(&a, &r, 13),
        Err(Error::Stale)
    ));
    let mut current = a;
    current.session_revision = 2;
    assert_eq!(
        rebound
            .ledger
            .receipt(&current, r.operation_id, 13)
            .unwrap()
            .disposition,
        InputDisposition::DeliveryUnknown
    );
    assert!(rebound.ledger.rebind_authority(3, 90001, id(1612)).is_err());
}

#[test]
fn composed_plans_commit_once_fifo_uses_admission_order_and_revoke_preserves_unknown() {
    let (l, mut s, a, p) = setup(false);
    let (l, c) = grant(l, &mut s, &a, &p, 100, 900);
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
fn snapshot_barrier_retires_old_frames_before_output_quota_charging() {
    let t = terminal(false);
    let p = peer(false);
    let mut replay = Replay::new(t.clone(), MAX_REPLAY_BYTES).unwrap();
    replay.frame(&p, frame(&t, 1, vec![1]), 100).unwrap();
    let same = frame(&t, 1, vec![1]);
    assert!(!replay.is_new_frame(&p, &same).unwrap());
    assert!(matches!(
        replay.is_new_frame(&p, &frame(&t, 1, vec![2])),
        Err(Error::Conflict)
    ));
    let snap = snapshot(&t, 100, id(101), 1);
    replay.snapshot(&p, snap.clone(), 101).unwrap();
    assert!(!replay.is_new_snapshot(&p, &snap).unwrap());
    assert!(!replay.is_new_frame(&p, &frame(&t, 1, vec![2])).unwrap());
    assert!(!replay.frame(&p, frame(&t, 1, vec![2]), 102).unwrap());
}

#[test]
fn exact_sealed_policy_is_durable_and_lower_input_pending_ceilings_are_enforced() {
    let t = terminal(false);
    let a = access(&t, "alice");
    let p = peer(false);
    let policy = TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: true,
        max_input_bytes: 2,
        max_pending_input_bytes: 3,
        max_frame_bytes: 2,
        replay_bytes: 4,
    };
    let l = Ledger::new_profiled(t.clone(), 1, 90000, None, policy.clone(), false).unwrap();
    let mut s = Store::new(&l);
    let (l, c) = grant(l, &mut s, &a, &p, 100, 1000);
    let mut first = input(&l, &c, 2000, 101);
    first.bytes = vec![1; 3];
    assert!(matches!(l.input(&a, &first, 101), Err(Error::Invalid)));
    first.bytes = vec![1; 2];
    let l = apply(l.input(&a, &first, 101).unwrap(), &mut s).ledger;
    let mut second = input(&l, &c, 2001, 101);
    second.bytes = vec![2; 2];
    assert!(matches!(l.input(&a, &second, 101), Err(Error::Capacity)));
    assert_eq!(
        Ledger::load(&l.encode().unwrap())
            .unwrap()
            .terminal_policy(),
        &policy
    );
    let mut tampered: serde_json::Value = serde_json::from_slice(&l.encode().unwrap()).unwrap();
    tampered["terminal_policy"]["max_pending_input_bytes"] = serde_json::json!(1);
    assert!(Ledger::load(&serde_json::to_vec(&tampered).unwrap()).is_err());
    let mut replay = Replay::new_profiled(t.clone(), &policy).unwrap();
    assert!(replay.frame(&p, frame(&t, 1, vec![1; 3]), 102).is_err());
    assert!(replay.frame(&p, frame(&t, 1, vec![1; 2]), 102).is_ok());
    let mut readonly = policy;
    readonly.input_enabled = false;
    let l = Ledger::new_profiled(t.clone(), 1, 90000, None, readonly.clone(), false).unwrap();
    assert!(matches!(
        l.acquire(
            &a,
            &api::AcquireControlRequest {
                terminal: t.clone(),
                operation_id: id(3000),
                expected_session_revision: 1,
                expected_controller_generation: 0,
                expires_at_ms: 1000
            },
            id(3001),
            id(3002),
            100
        ),
        Err(Error::Forbidden)
    ));
    assert!(
        l.watch(
            &a,
            &api::WatchRequest {
                terminal: t.clone(),
                operation_id: id(3003),
                expected_session_revision: 1,
                expires_at_ms: 1000,
                after_sequence: None
            },
            100
        )
        .is_ok()
    );
    assert!(Ledger::new_profiled(t, 1, 90000, None, readonly, true).is_err());
}

#[test]
fn replayable_effects_recheck_fresh_scope_execution_and_never_replay_input() {
    let (l, mut store, a, p) = setup(false);
    let (l, c) = grant(l, &mut store, &a, &p, 100, 4000);
    let fence = Effect::Fence(Fence {
        terminal: l.terminal().clone(),
        fence_id: id(4001),
        generation: c.generation,
        controller: Some(c.clone()),
    });
    l.authorize_replayable_effect(Some(&a), &fence, 101)
        .unwrap();
    let mut inactive = a.clone();
    inactive.execution_active = false;
    assert!(
        l.authorize_replayable_effect(Some(&inactive), &fence, 101)
            .is_err()
    );
    let watched = apply(
        l.watch(
            &a,
            &api::WatchRequest {
                terminal: l.terminal().clone(),
                operation_id: id(4100),
                expected_session_revision: 1,
                expires_at_ms: 1000,
                after_sequence: None,
            },
            101,
        )
        .unwrap(),
        &mut store,
    );
    let watch = watched.effects[0].clone();
    let l = watched.ledger;
    l.authorize_replayable_effect(Some(&inactive), &watch, 102)
        .unwrap();
    let mut revoked = a.clone();
    revoked.watch = false;
    assert!(
        l.authorize_replayable_effect(Some(&revoked), &watch, 102)
            .is_err()
    );
    let mut shorter = a.clone();
    shorter.valid_until_ms = 500;
    assert!(
        l.authorize_replayable_effect(Some(&shorter), &watch, 102)
            .is_err()
    );
    let request = input(&l, &c, 4200, 102);
    let l = apply(l.input(&a, &request, 102).unwrap(), &mut store).ledger;
    let dispatched = apply(
        l.dispatch_input(&a, request.operation_id, 103).unwrap(),
        &mut store,
    );
    assert!(matches!(
        dispatched
            .ledger
            .authorize_replayable_effect(Some(&a), &dispatched.effects[0], 104),
        Err(Error::Transition)
    ));
}

#[test]
fn trusted_scope_refusal_rejects_only_never_dispatched_and_keeps_all_replay_fences() {
    let (l, mut store, a, p) = setup(false);
    let (l, c) = grant(l, &mut store, &a, &p, 100, 5000);
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
