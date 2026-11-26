use super::*;

#[test]
fn exact_sealed_policy_is_durable_and_lower_input_pending_ceilings_are_enforced() {
    let t = terminal(&remote_peer());
    let a = access(&t, "alice");
    let p = remote_peer();
    let policy = bounded_policy();
    let l = Ledger::new_profiled(t.clone(), 1, 90000, None, policy.clone(), false).unwrap();
    let mut s = Store::new(&l);
    let (l, c) = PeerSession::new(&mut s, &a, &p).grant(l, 100, 1000);
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
}

#[test]
fn output_only_policy_allows_watches_and_refuses_control() {
    let t = terminal(&remote_peer());
    let a = access(&t, "alice");
    let mut readonly = bounded_policy();
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

fn bounded_policy() -> TerminalPolicy {
    TerminalPolicy {
        host: TerminalHost::Tmux,
        input_enabled: true,
        max_input_bytes: 2,
        max_pending_input_bytes: 3,
        max_frame_bytes: 2,
        replay_bytes: 4,
    }
}
