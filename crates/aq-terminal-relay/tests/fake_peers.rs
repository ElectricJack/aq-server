#[path = "fake_peers/epoch.rs"]
mod epoch_tests;
#[path = "fake_peers/input.rs"]
mod input_tests;
#[path = "fake_peers/output.rs"]
mod output_tests;
#[path = "fake_peers/policy.rs"]
mod policy_tests;

use aq_session_api as api;
use aq_session_protocol::*;
use aq_terminal_relay::{draft::*, output::*, *};
use uuid::Uuid;
fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn terminal(peer: &Peer) -> TerminalIdentity {
    let session = match peer {
        Peer::Central { authority_id } => SessionIdentity::Central {
            authority_id: *authority_id,
            session_id: id(2),
            generation: 1,
        },
        Peer::Remote { node_id, boot_id } => SessionIdentity::Remote {
            node_id: *node_id,
            boot_id: *boot_id,
            attempt_id: id(3),
            assignment_generation: 1,
            session_id: id(4),
        },
    };
    TerminalIdentity {
        session,
        terminal_id: id(5),
        epoch: 1,
    }
}
fn central_peer() -> Peer {
    Peer::Central {
        authority_id: id(1),
    }
}
fn remote_peer() -> Peer {
    Peer::Remote {
        node_id: id(1),
        boot_id: id(2),
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
fn setup(peer: Peer) -> (Ledger, Store, Access, Peer) {
    let l = Ledger::new(terminal(&peer), 1, 90000, None).unwrap();
    let s = Store::new(&l);
    let a = access(l.terminal(), "alice");
    (l, s, a, peer)
}
struct PeerSession<'a> {
    store: &'a mut Store,
    access: &'a Access,
    peer: &'a Peer,
}

impl<'a> PeerSession<'a> {
    fn new(store: &'a mut Store, access: &'a Access, peer: &'a Peer) -> Self {
        Self {
            store,
            access,
            peer,
        }
    }

    fn grant(&mut self, l: Ledger, now: u64, n: u128) -> (Ledger, ControllerLease) {
        let req = api::AcquireControlRequest {
            terminal: l.terminal().clone(),
            operation_id: id(n),
            expected_session_revision: 1,
            expected_controller_generation: l.generation(),
            expires_at_ms: now + 10000,
        };
        let c = apply(
            l.acquire(self.access, &req, id(n + 1), id(n + 2), now)
                .unwrap(),
            self.store,
        );
        let Effect::Fence(f) = c.effects[0].clone() else {
            panic!()
        };
        let lease = f.controller.clone().unwrap();
        let c = apply(
            c.ledger
                .acknowledge_fence(
                    self.peer,
                    &FenceAck {
                        terminal: f.terminal,
                        fence_id: f.fence_id,
                        generation: f.generation,
                    },
                )
                .unwrap(),
            self.store,
        );
        (c.ledger, lease)
    }
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
        escape_key: false,
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
    for peer in [remote_peer(), central_peer()] {
        let (l, mut s, a, p) = setup(peer);
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
        let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 10, 30);
        let mut r = input(&l, &c, 40, 20);
        let original = r.terminal.clone();
        r.terminal.epoch += 1;
        assert!(matches!(l.input(&a, &r, 20), Err(Error::Stale)));
        r.terminal = original;
        let wrong = if matches!(p, Peer::Central { .. }) {
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
    let (l, mut s, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 100);
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
    let (l, mut s, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 200);
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
    let t = terminal(&remote_peer());
    let mut a = access(&t, "alice");
    a.execution_expires_at_ms = 20000;
    let l = Ledger::new(t, 1, 50000, Some(500)).unwrap();
    let mut s = Store::new(&l);
    let p = remote_peer();
    let (l, _) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 300);
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
        peer: central_peer(),
        exchange_id: id(1200),
        cursor: 1,
        effects: vec![Effect::Fence(Fence {
            terminal: terminal(&central_peer()),
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
    wrong.peer = remote_peer();
    assert!(wrong.validate().is_err());
}
#[test]
fn duplicate_noop_revisions_and_authority_rebind_do_not_replay_or_extend_execution() {
    let (l, mut s, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 1600);
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
fn replayable_effects_recheck_fresh_scope_execution_and_never_replay_input() {
    let (l, mut store, a, p) = setup(remote_peer());
    let (l, c) = PeerSession::new(&mut store, &a, &p).grant(l, 100, 4000);
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
