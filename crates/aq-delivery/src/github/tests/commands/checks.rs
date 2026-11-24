use super::*;

#[tokio::test]
async fn await_checks_completes_once_with_passed_failed_conflict_closed_or_timeout() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let open = || pull(7, json!({}));
    let ok = || runs(&[("unit", "completed", Some("success"))]);
    let pending = || runs(&[("unit", "in_progress", None)]);

    poll(&mock, open(), open(), ok());
    let p = poll_of(&delivery, 1_000, 5_000).await;
    let c = p.completion.unwrap();
    assert_eq!((c.outcome, c.output), ("passed", json!({})));
    assert_eq!(p.observed.unwrap().checks_state, "passed");

    poll(
        &mock,
        open(),
        open(),
        runs(&[("unit", "completed", Some("failure"))]),
    );
    let p = poll_of(&delivery, 1_000, 5_000).await;
    let c = p.completion.unwrap();
    assert_eq!(
        (c.outcome, c.output),
        ("failed", json!({"names": ["unit"]}))
    );
    assert_eq!(p.observed.unwrap().checks_failed, ["unit"]);

    // A conflict is found before the checks are read.
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    let p = poll_of(&delivery, 1_000, 5_000).await;
    assert_eq!(p.completion.unwrap().outcome, "conflict");
    assert_eq!(p.observed.unwrap().mergeable, "conflicting");

    // Closed without a merge (and merged elsewhere) is `closed`.
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    assert_eq!(
        step(&delivery, 1_000, 5_000).await.unwrap().outcome,
        "closed"
    );

    // Pending before the deadline: no completion. At the deadline: timeout.
    poll(&mock, open(), open(), pending());
    let p = poll_of(&delivery, 4_999, 5_000).await;
    assert!(p.completion.is_none());
    assert_eq!(p.observed.unwrap().checks_state, "pending");
    poll(&mock, open(), open(), pending());
    let c = step(&delivery, 5_000, 5_000).await.unwrap();
    assert_eq!((c.outcome, c.output), ("timeout", json!({})));

    // A read that fails decides nothing; the deadline still ends the wait.
    mock.reply(503, vec![], json!({"message": "down"}));
    assert!(step(&delivery, 1_000, 5_000).await.is_none());
    mock.reply(503, vec![], json!({"message": "down"}));
    assert_eq!(
        step(&delivery, 9_000, 5_000).await.unwrap().outcome,
        "timeout"
    );

    // A decision that is made wins over a deadline that has also passed.
    poll(&mock, open(), open(), ok());
    assert_eq!(
        step(&delivery, 9_000, 5_000).await.unwrap().outcome,
        "passed"
    );
}

#[test]
fn the_wait_is_bounded_by_the_longest_check_wait_setting() {
    assert_eq!(bounded_wait_ms(1_800_000, 3_600_000), 1_800_000);
    assert_eq!(bounded_wait_ms(604_800_000, 3_600_000), 3_600_000);
}

#[tokio::test]
async fn a_check_poll_requires_a_frozen_head_and_tracked_base_for_a_behind_repair() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let mut old_target = target();
    old_target.git.binding_generation -= 1;
    let stale = json!({
        "pull_request": 7, "result_head": sha('a'), "check_target": old_target,
        "required": ["check", "deployment"], "deadline_at_ms": 5_000
    });
    for (now, outcome) in [(1_000, None), (5_000, Some("timeout"))] {
        let run = delivery
            .step(
                FrozenCommand::new(Uuid::new_v4(), "pull_request.await_checks", &stale),
                &target(),
                now,
            )
            .await
            .unwrap();
        assert_eq!(run.step.map(|step| done(step).outcome), outcome);
    }
    assert!(mock.seen().is_empty());

    let behind = || pull(7, json!({"mergeable_state": "behind"}));
    mock.reply(200, vec![], behind());
    assert!(poll_result(&delivery, 7, &sha('b')).await.is_none());
    mock.reply(200, vec![], behind());
    assert!(poll_of(&delivery, 1_000, 5_000).await.completion.is_none());

    let current = pull(7, json!({}));
    let retargeted = pull(
        7,
        json!({"mergeable_state": "behind", "base": {"ref": "other", "sha": sha('d')}}),
    );
    poll(&mock, current.clone(), retargeted, runs(&[]));
    assert!(poll_result(&delivery, 7, &sha('a')).await.is_none());

    let advanced = pull(7, json!({"base": {"ref": "main", "sha": sha('d')}}));
    poll(
        &mock,
        current.clone(),
        advanced,
        runs(&[
            ("check", "completed", Some("success")),
            ("deployment", "completed", Some("success")),
        ]),
    );
    assert!(poll_result(&delivery, 7, &sha('a')).await.is_none());

    poll(&mock, current, behind(), runs(&[]));
    assert_eq!(
        poll_result(&delivery, 7, &sha('a')).await.unwrap().outcome,
        "conflict"
    );
    assert!(git.operations().is_empty());
}
