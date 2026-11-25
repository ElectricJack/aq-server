use super::*;

#[tokio::test]
async fn recover_after_backup_crash_removes_partial_dump() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.crash("backup", 0);
    assert!(e.install(req(B)).await.is_err());
    let partial = Journal::read(&h.state_dir)
        .unwrap()
        .current
        .unwrap()
        .backup
        .unwrap();
    std::fs::write(&partial, b"torn").unwrap();
    drop(e);
    let mut e = h.engine();
    assert_eq!(
        e.recover().await.unwrap().unwrap().outcome,
        Outcome::Aborted
    );
    assert!(!partial.exists());
    assert!(e.journal_state().active_backup.as_ref().unwrap().exists());
}

#[tokio::test]
async fn recover_with_terminal_entry_already_recorded_does_not_duplicate() {
    // Crash between appending the outcome and clearing the journal.
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("pull", 0, "nope");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    drop(e);
    let mut state = Journal::read(&h.state_dir).unwrap();
    let target = manifest(B, 1, &[1]);
    state.current = Some(
        serde_json::from_value(serde_json::json!({
            "request_id": r.request_id.unwrap(),
            "target": target,
            "step": "pulled",
            "backup": null,
            "needs_restore_on_rollback": false,
            "started_at": r.started_at,
            "rollback_attempted": false
        }))
        .unwrap(),
    );
    Journal::open(lock(&h.state_dir).unwrap())
        .unwrap()
        .record(state)
        .unwrap();
    let n = h.history().len();
    h.fake.clear_log();
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.history().len(), n);
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.fake.count("start_server"), 0);
}

#[tokio::test]
async fn idle_recover_clears_stale_maintenance() {
    let (h, e, _a) = with_a().await;
    drop(e);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.fake.log(), vec!["leave".to_string()]);
}

#[tokio::test]
async fn unknown_recorded_outcome_holds_for_operator() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.crash("wait_healthy", 0);
    let request = req(B);
    assert!(e.install(request.clone()).await.is_err());
    drop(e);
    // A newer updater recorded an outcome this one does not know.
    let line = serde_json::json!({
        "request_id": request.id,
        "from": A,
        "to": B,
        "outcome": "some_future_outcome",
        "detail": "?",
        "started_at": Utc::now(),
        "finished_at": Utc::now()
    });
    let path = h.state_dir.join("history.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&path, text).unwrap();
    h.fake.clear_log();

    let mut e = h.engine();
    let out = e.recover().await.unwrap().unwrap();
    assert_eq!(out.outcome, Outcome::FailedNeedsOperator, "{}", out.detail);
    assert_eq!(h.fake.log(), vec!["enter".to_string()], "no runtime action");
    assert!(h.fake.maintenance_on());
    let cur = e.journal_state().current.clone().expect("current kept");
    assert!(cur.rollback_attempted);
    assert!(matches!(
        e.status().state,
        UpdateState::FailedNeedsOperator { .. }
    ));
    drop(e);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
}

#[tokio::test]
async fn first_install_recovers_after_crash() {
    struct Case {
        script: fn(&FakeRuntime),
        step: Step,
        outcome: Outcome,
        installed: bool,
    }
    let cases = [
        Case {
            script: |f| f.crash("pull", 0),
            step: Step::Pulled,
            outcome: Outcome::Aborted,
            installed: false,
        },
        Case {
            script: |f| f.crash("wait_healthy", 0),
            step: Step::Probing,
            outcome: Outcome::Aborted,
            installed: false,
        },
        Case {
            script: |f| f.crash("leave", 0),
            step: Step::Active,
            outcome: Outcome::Succeeded,
            installed: true,
        },
    ];
    for c in cases {
        let h = H::new();
        let a = manifest(A, 1, &[1]);
        h.publish(&a);
        (c.script)(&h.fake);
        let mut e = h.engine();
        assert!(e.install(req(A)).await.is_err());
        drop(e);
        assert_eq!(
            Journal::read(&h.state_dir).unwrap().current.unwrap().step,
            c.step
        );
        h.fake.clear_log();
        let mut e = h.engine();
        let out = match e.recover().await.unwrap() {
            Some(out) => out,
            None if c.step == Step::Active => h.history()[0].clone(),
            None => panic!("{:?}: no outcome", c.step),
        };
        assert_eq!(out.outcome, c.outcome, "{:?}: {}", c.step, out.detail);
        assert_eq!(
            e.journal_state().active.is_some(),
            c.installed,
            "{:?}",
            c.step
        );
        assert!(e.journal_state().current.is_none());
        assert!(!h.fake.maintenance_on());
        if c.installed {
            assert_eq!(h.fake.running().unwrap(), a.images.server);
        } else {
            // Nothing to roll back to: no release left running, none started.
            assert!(h.fake.running().is_none(), "{:?}", c.step);
            assert_eq!(h.fake.count("start_server"), 0, "{:?}", c.step);
        }
    }
}

#[tokio::test]
async fn early_step_recovery_leaves_a_healthy_active_release_running() {
    for healthy in [true, false] {
        let (h, mut e, a) = with_a().await;
        h.publish(&manifest(B, 1, &[1]));
        h.fake.crash("pull", 0);
        assert!(e.install(req(B)).await.is_err());
        drop(e);
        assert_eq!(
            Journal::read(&h.state_dir).unwrap().current.unwrap().step,
            Step::Pulled
        );
        h.fake.clear_log();
        if !healthy {
            h.fake.fail("wait_healthy", 0, "A unhealthy");
        }
        let mut e = h.engine();
        let out = e.recover().await.unwrap().unwrap();
        assert_eq!(out.outcome, Outcome::Aborted, "{}", out.detail);
        assert!(
            out.detail.contains(&format!("{A} is running")),
            "{}",
            out.detail
        );
        let starts = h.fake.count("start_server");
        if healthy {
            assert_eq!(starts, 0, "A was recreated: {:?}", h.fake.log());
            assert_eq!(h.fake.count("wait_healthy"), 1);
        } else {
            assert_eq!(starts, 1, "{:?}", h.fake.log());
            let log = h.fake.log();
            let first_probe = log.iter().position(|c| c == "wait_healthy").unwrap();
            let start = log
                .iter()
                .position(|c| c.starts_with("start_server"))
                .unwrap();
            assert!(first_probe < start, "{log:?}");
        }
        assert_eq!(h.fake.running().unwrap(), a.images.server);
    }
}
