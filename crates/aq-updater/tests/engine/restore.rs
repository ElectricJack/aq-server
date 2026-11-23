use super::*;

#[tokio::test]
async fn restore_previous_requires_flag_when_state_format_differs() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    assert_eq!(h.fake.db_format(), Some(2));
    let a_backup = e.journal_state().active_backup.clone().unwrap();
    h.fake.clear_log();

    let r = e.restore_previous(false).await.unwrap();
    assert_explicit_consent_required(&h, &e, &r, &a_backup);

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(r.from, Some(ver(B)));
    assert_eq!(r.to, Some(ver(A)));
    assert_restored_data(&h, &e, &a, &a_backup);

    // Going forward again needs a backup that no longer exists.
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
}

#[tokio::test]
async fn restore_previous_same_format_swaps_without_restore() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_no_calls(&h.fake, &["restore", "stop_server"]);
    assert_release_running(&h, &e, &a);
    assert_eq!(e.journal_state().previous.as_ref().unwrap().version, ver(B));
}

#[tokio::test]
async fn restore_previous_without_previous_refused() {
    let (h, mut e, _a) = with_a().await;
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(h.fake.log().is_empty());
}

#[tokio::test]
async fn restore_previous_failure_rolls_back_to_active() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    h.fake.clear_log();
    h.fake.fail("wait_healthy", 0, "A unhealthy");
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_release_running(&h, &e, &b);
    assert_eq!(
        h.fake.db_format(),
        Some(2),
        "B's data restored from the fresh backup"
    );
    assert!(
        e.journal_state().active_backup.is_some(),
        "unchanged on failure"
    );
}

/// Crash points inside `restore_previous(true)` from B (format 2) back to A
/// (format 1); recovery must leave a consistent release and its data.
#[tokio::test]
async fn restore_previous_recovers_after_crash() {
    let cases = [
        RestoreCrash {
            name: "after backup started",
            script: |f| f.crash("backup", 0),
            step: Step::BackedUp,
            outcome: Outcome::Aborted,
            active: B,
        },
        RestoreCrash {
            name: "after restore(active_backup), before start_server(A)",
            script: |f| f.crash("start_server", 0),
            step: Step::Replacing,
            outcome: Outcome::RolledBack,
            active: B,
        },
        RestoreCrash {
            name: "while probing A",
            script: |f| f.crash("wait_healthy", 0),
            step: Step::Probing,
            outcome: Outcome::RolledBack,
            active: B,
        },
        RestoreCrash {
            name: "after the swap",
            script: |f| f.crash("leave", 0),
            step: Step::Active,
            outcome: Outcome::Succeeded,
            active: A,
        },
    ];
    for c in cases {
        check_restore_crash(c).await;
    }
}

struct RestoreCrash {
    name: &'static str,
    script: fn(&FakeRuntime),
    step: Step,
    outcome: Outcome,
    active: &'static str,
}

struct RestoreFixture {
    h: H,
    a: ReleaseManifest,
    b: ReleaseManifest,
    a_backup: PathBuf,
}

async fn check_restore_crash(c: RestoreCrash) {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    let a_backup = e.journal_state().active_backup.clone().unwrap();
    h.fake.clear_log();
    (c.script)(&h.fake);
    assert!(e.restore_previous(true).await.is_err(), "{}: crash", c.name);
    if c.step == Step::Replacing {
        assert!(
            h.fake
                .position(&format!("restore {}", a_backup.display()))
                .is_some()
        );
        assert_eq!(h.fake.db_format(), Some(1), "{}: A's data restored", c.name);
    }
    drop(e);
    let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
    assert_eq!(cur.step, c.step, "{}", c.name);

    let fixture = RestoreFixture { h, a, b, a_backup };
    verify_restore_recovery(&fixture, &c).await;
}

async fn verify_restore_recovery(fixture: &RestoreFixture, c: &RestoreCrash) {
    let RestoreFixture { h, a, b, .. } = fixture;
    h.fake.clear_log();
    let mut e = h.engine();
    // A crash in leave() comes after the outcome line: nothing to redo.
    let out = match e.recover().await.unwrap() {
        Some(out) => out,
        None if c.step == Step::Active => h.history()[0].clone(),
        None => panic!("{}: no outcome", c.name),
    };
    assert_eq!(out.outcome, c.outcome, "{}: {}", c.name, out.detail);
    assert_eq!(active_version(&e), Some(ver(c.active)), "{}", c.name);
    let (image, format) = if c.active == A {
        (&a.images.server, 1)
    } else {
        (&b.images.server, 2)
    };
    assert_eq!(h.fake.running().as_ref(), Some(image), "{}", c.name);
    assert_eq!(h.fake.db_format(), Some(format), "{}: data", c.name);
    assert!(!h.fake.maintenance_on(), "{}", c.name);
    assert!(e.journal_state().current.is_none(), "{}", c.name);
    verify_recovery_backup(fixture, &e, c);
}

fn verify_recovery_backup(fixture: &RestoreFixture, e: &TestEngine, c: &RestoreCrash) {
    let RestoreFixture { h, b, a_backup, .. } = fixture;
    if c.outcome == Outcome::RolledBack {
        // B's data comes back from the fresh pre-restore backup.
        let log = h.fake.log();
        let stop = h.fake.position("stop_server").unwrap();
        let restore = h.fake.position("restore").unwrap();
        let start_b = h
            .fake
            .position(&format!("start_server {}", b.images.server))
            .unwrap();
        assert!(stop < restore && restore < start_b, "{}: {log:?}", c.name);
        assert_ne!(log[restore], format!("restore {}", a_backup.display()));
        assert_eq!(e.journal_state().active_backup.as_ref(), Some(a_backup));
    }
    if c.outcome == Outcome::Succeeded {
        assert!(e.journal_state().active_backup.is_none(), "{}", c.name);
    }
}

#[tokio::test]
async fn restore_previous_refuses_missing_backup_file_without_downtime() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    let backup = e.journal_state().active_backup.clone().unwrap();
    std::fs::remove_file(&backup).unwrap();
    h.fake.clear_log();
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("missing"), "{}", r.detail);
    assert_restore_refused_without_changes(&h, &e);
}

#[tokio::test]
async fn restore_previous_refuses_unreadable_current_data() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    // Same manifest format, but the database says 3, which A cannot read.
    h.fake.set_db_format(Some(3));
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("state format 3"), "{}", r.detail);
    assert_eq!(h.fake.log(), vec!["server_state_format".to_string()]);
}

fn assert_explicit_consent_required(
    h: &H,
    e: &TestEngine,
    r: &HistoryEntry,
    a_backup: &std::path::Path,
) {
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("--restore-backup"), "{}", r.detail);
    assert!(r.detail.contains("lost"), "{}", r.detail);
    assert!(
        r.detail.contains(&a_backup.display().to_string()),
        "{}",
        r.detail
    );
    assert!(
        h.fake.log().is_empty(),
        "nothing stopped: {:?}",
        h.fake.log()
    );
    assert_eq!(active_version(e), Some(ver(B)));
}

fn assert_restored_data(h: &H, e: &TestEngine, a: &ReleaseManifest, a_backup: &std::path::Path) {
    let log = h.fake.log();
    let stop = h.fake.position("stop_server").unwrap();
    let restore = h
        .fake
        .position(&format!("restore {}", a_backup.display()))
        .unwrap();
    let start = h
        .fake
        .position(&format!("start_server {}", a.images.server))
        .unwrap();
    assert!(stop < restore && restore < start, "{log:?}");
    assert_eq!(h.fake.db_format(), Some(1));
    let s = e.journal_state();
    assert_eq!(s.active.as_ref().unwrap().version, ver(A));
    assert_eq!(s.previous.as_ref().unwrap().version, ver(B));
    assert!(s.active_backup.is_none(), "no backup to go forward again");
    assert!(!h.fake.maintenance_on());
    assert!(a_backup.exists(), "the restored backup is kept");
}

fn assert_restore_refused_without_changes(h: &H, e: &TestEngine) {
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert!(e.journal_state().current.is_none());
}
