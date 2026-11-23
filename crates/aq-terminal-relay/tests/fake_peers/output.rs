use super::*;

#[test]
fn resize_id_history_is_immutable_after_intervening_operations() {
    let (l, mut s, a, p) = setup(remote_peer());
    let (mut l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 800);
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
fn resize_request(
    c: &ControllerLease,
    n: u128,
    sequence: u64,
    (columns, rows): (u16, u16),
) -> api::ResizeRequest {
    api::ResizeRequest {
        terminal: c.terminal.clone(),
        operation_id: id(n),
        lease_id: c.lease_id,
        expected_controller_generation: c.generation,
        expected_session_revision: 1,
        expires_at_ms: 1000,
        sequence,
        columns,
        rows,
    }
}
#[test]
fn resize_reaches_the_worker_only_from_the_current_controller_within_bounds() {
    for peer in [remote_peer(), central_peer()] {
        let (l, mut s, a, p) = setup(peer);
        let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 0, 900);
        // Accepted: exactly one resize effect carrying the checked lease.
        let ok = resize_request(&c, 910, 1, (120, 40));
        let done = apply(l.resize(&a, &ok, 10).unwrap(), &mut s);
        let [Effect::Resize(command)] = &done.effects[..] else {
            panic!("expected one resize effect")
        };
        assert_eq!(command.lease.lease_id, c.lease_id);
        assert_eq!(command.lease.generation, c.generation);
        assert_eq!(
            (command.columns, command.rows, command.sequence),
            (120, 40, 1)
        );
        let l = done.ledger;
        // The protocol bounds are 1..=1000 on both axes.
        for (n, columns, rows) in [(911, 1, 1), (912, 1000, 1000)] {
            let edge = resize_request(&c, n, n as u64 - 909, (columns, rows));
            assert!(l.resize(&a, &edge, 11).is_ok(), "{columns}x{rows}");
        }
        for (n, columns, rows) in [(913, 0, 24), (914, 80, 0), (915, 1001, 24), (916, 80, 1001)] {
            let bad = resize_request(&c, n, 10, (columns, rows));
            assert!(
                matches!(l.resize(&a, &bad, 11), Err(Error::Invalid)),
                "{columns}x{rows}"
            );
        }
        // A viewer that may watch but not control cannot resize.
        let mut viewer = a.clone();
        viewer.control = false;
        assert!(matches!(
            l.resize(&viewer, &resize_request(&c, 917, 10, (80, 24)), 11),
            Err(Error::Forbidden)
        ));
        // Another principal, a stale generation or lease and a stale session
        // revision are all refused.
        let other = access(l.terminal(), "mallory");
        assert!(matches!(
            l.resize(&other, &resize_request(&c, 918, 10, (80, 24)), 11),
            Err(Error::Stale)
        ));
        let mut stale = resize_request(&c, 919, 10, (80, 24));
        stale.expected_controller_generation += 1;
        assert!(matches!(l.resize(&a, &stale, 11), Err(Error::Stale)));
        let mut wrong_lease = resize_request(&c, 920, 10, (80, 24));
        wrong_lease.lease_id = id(999);
        assert!(matches!(l.resize(&a, &wrong_lease, 11), Err(Error::Stale)));
        let mut old_revision = resize_request(&c, 921, 10, (80, 24));
        old_revision.expected_session_revision = 0;
        assert!(matches!(l.resize(&a, &old_revision, 11), Err(Error::Stale)));
        // A released controller can no longer resize.
        let release = l
            .release(
                &a,
                &ReleaseControl {
                    terminal: l.terminal().clone(),
                    operation_id: id(922),
                    lease_id: c.lease_id,
                    expected_controller_generation: c.generation,
                    expected_session_revision: 1,
                },
                id(923),
                12,
            )
            .unwrap();
        let released = apply(release, &mut s).ledger;
        assert!(
            released
                .resize(&a, &resize_request(&c, 924, 10, (80, 24)), 13)
                .is_err()
        );
    }
}
#[test]
fn viewers_are_scoped_independent_and_bounded() {
    let (mut l, mut s, a, _) = setup(remote_peer());
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
    let (l, mut s, a, p) = setup(remote_peer());
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
    let (l, mut s, a, p) = setup(central_peer());
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
fn snapshot_barrier_retires_old_frames_before_output_quota_charging() {
    let t = terminal(&remote_peer());
    let p = remote_peer();
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
