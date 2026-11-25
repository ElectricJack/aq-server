use super::*;

#[tokio::test]
async fn restore_previous_after_needs_operator_retries_active_once() {
    let (h, mut e, a) = with_a().await;
    // B migrates to format 2, then both B and the rollback fail.
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("start_server", 1, "A would not start");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert!(r.detail.contains("restore-previous"), "{}", r.detail);

    // The data may be B's, so the operator must opt into the restore.
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("--restore-backup"), "{}", r.detail);
    assert!(h.fake.log().is_empty());
    assert!(matches!(
        e.status().state,
        UpdateState::FailedNeedsOperator { .. }
    ));

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_eq!(h.fake.db_format(), Some(1));
    assert!(h.fake.position("restore").unwrap() < h.fake.position("start_server").unwrap());
    assert!(e.journal_state().current.is_none());
    assert!(!h.fake.maintenance_on());
    assert_eq!(e.status().state, UpdateState::Idle);
}

/// B changes the data without changing its state format (a migration added
/// without a format bump): the rollback starts A without a restore, A cannot
/// read the data, and the update ends in `FailedNeedsOperator`.
async fn needs_operator_after_unflagged_migration() -> (H, TestEngine, ReleaseManifest) {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_start_migrates(&b.images.server, 2);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    let cur = e.journal_state().current.clone().unwrap();
    assert!(!cur.needs_restore_on_rollback, "the formats say no restore");
    assert!(cur.backup.as_ref().is_some_and(|b| b.exists()));
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.db_format(), Some(2));
    h.fake.clear_log();
    (h, e, a)
}

#[tokio::test]
async fn restore_backup_flag_forces_the_in_flight_restore() {
    let (h, mut e, a) = needs_operator_after_unflagged_migration().await;
    let backup = e.journal_state().current.clone().unwrap().backup.unwrap();

    // Without the flag the retry brings A back on the data as it is, and
    // fails again the same way.
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    h.fake.clear_log();

    // With it, the recorded in-flight backup is restored first.
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    let log = h.fake.log();
    let pos = |p: &str| log.iter().position(|c| c.starts_with(p)).unwrap();
    assert_eq!(
        h.fake.count(&format!("restore {}", backup.display())),
        1,
        "{log:?}"
    );
    assert!(pos("select_release") < pos("stop_server"), "{log:?}");
    assert!(pos("stop_server") < pos("restore"), "{log:?}");
    assert!(pos("restore") < pos("start_server"), "{log:?}");
    assert_eq!(h.fake.db_format(), Some(1));
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(e.journal_state().current.is_none());
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn restore_backup_flag_needs_the_backup_file() {
    let (h, mut e, _a) = needs_operator_after_unflagged_migration().await;
    let backup = e.journal_state().current.clone().unwrap().backup.unwrap();
    std::fs::remove_file(&backup).unwrap();
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("missing"), "{}", r.detail);
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert!(e.journal_state().current.is_some());
}

#[tokio::test]
async fn restore_backup_flag_never_restores_before_the_target_ran() {
    // Held for the operator at `BackedUp` (history holds an outcome this
    // updater does not know): the target never started and the dump may be
    // partial, so the flag restores nothing.
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.crash("backup", 0);
    let request = req(B);
    assert!(e.install(request.clone()).await.is_err());
    drop(e);
    let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
    assert_eq!(cur.step, Step::BackedUp);
    assert!(cur.needs_restore_on_rollback);
    // A (partial) dump file at the recorded path.
    std::fs::write(cur.backup.as_ref().unwrap(), b"fmt:1\npartial").unwrap();
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
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}{line}\n")).unwrap();
    let mut e = h.engine();
    let out = e.recover().await.unwrap().unwrap();
    assert_eq!(out.outcome, Outcome::FailedNeedsOperator, "{}", out.detail);
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.step, Step::BackedUp);
    assert!(
        !out.detail.contains("--restore-backup"),
        "no restore is offered: {}",
        out.detail
    );
    h.fake.clear_log();

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.count("stop_server"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}
