use super::*;

#[tokio::test]
async fn backup_failure_aborts_and_leaves_maintenance() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("backup", 0, "No space left on device");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("No space left on device"), "{}", r.detail);
    assert!(h.fake.position("enter").unwrap() < h.fake.position("leave").unwrap());
    assert!(!h.fake.maintenance_on(), "maintenance cleared");
    assert_no_calls(&h.fake, &["start_server", "stop_server"]);
    assert_release_running(&h, &e, &a);
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.dumps().len(), 1, "no partial dump left behind");
}

#[tokio::test]
async fn existing_backup_file_is_never_overwritten_or_deleted() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    // Occupy the backup name for this second and the next two.
    let now = Utc::now();
    let taken: Vec<PathBuf> = (0..3)
        .map(|s| {
            let t = now + chrono::Duration::seconds(s);
            let p = h
                .backup_dir
                .join(format!("{}-{A}-{B}.dump", t.format("%Y%m%dT%H%M%SZ")));
            std::fs::write(&p, b"someone else's").unwrap();
            p
        })
        .collect();
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted, "{}", r.detail);
    assert!(r.detail.contains("already exists"), "{}", r.detail);
    assert_eq!(h.fake.count("backup"), 0);
    for p in &taken {
        assert_eq!(std::fs::read(p).unwrap(), b"someone else's");
    }
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn backups_pruned_keeping_active_backup() {
    let h = H::new();
    std::fs::create_dir_all(&h.backup_dir).unwrap();
    for i in 0..4 {
        std::fs::write(
            h.backup_dir
                .join(format!("2020010{i}T000000Z-0.0.{i}-0.0.9.dump")),
            b"x",
        )
        .unwrap();
    }
    std::fs::write(h.backup_dir.join("notes.txt"), b"keep me").unwrap();
    let settings = EngineSettings {
        backups_keep: 2,
        ..h.settings()
    };
    let mut e = h.engine_with(settings);
    h.publish(&manifest(A, 1, &[1]));
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    let a_backup = e.journal_state().active_backup.clone().unwrap();
    assert_eq!(h.dumps().len(), 2);
    // B fails: its backup is the newest, A's is protected as active_backup.
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("wait_healthy", 1, "no");
    assert_eq!(
        e.install(req(B)).await.unwrap().outcome,
        Outcome::RolledBack
    );
    h.publish(&manifest("1.2.0", 1, &[1]));
    assert_eq!(
        e.install(req("1.2.0")).await.unwrap().outcome,
        Outcome::Succeeded
    );
    assert_pruned_backups(&h, &e, &a_backup);
}

#[tokio::test]
async fn backup_waits_for_the_maintenance_drain() {
    let drain = Duration::from_millis(400);
    let (h, _e, _a) = with_a().await;
    drop(_e);
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let mut e = h.engine_with(EngineSettings {
        maintenance_drain: drain,
        ..h.settings()
    });
    let fake = h.fake.clone();
    let marks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = marks.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        if let UpdateState::InProgress { step, .. } = s.state {
            sink.lock().unwrap().push((
                step,
                std::time::Instant::now(),
                fake.maintenance_on(),
                fake.count("backup"),
            ));
        }
    }));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    let marks = marks.lock().unwrap();
    assert_drain_before_backup(&marks, drain);
}

#[tokio::test]
async fn backup_rotation_keeps_five_by_default() {
    let h = H::new();
    std::fs::create_dir_all(&h.backup_dir).unwrap();
    for i in 0..7 {
        std::fs::write(
            h.backup_dir
                .join(format!("2020010{i}T000000Z-0.0.{i}-0.0.9.dump")),
            b"x",
        )
        .unwrap();
    }
    let settings = h.settings();
    assert_eq!(settings.backups_keep, 5);
    let mut e = h.engine_with(settings);
    h.publish(&manifest(A, 1, &[1]));
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    let dumps = h.dumps();
    assert_eq!(dumps.len(), 5, "{dumps:?}");
    // The new backup and the four newest old ones survive.
    assert!(dumps.contains(e.journal_state().active_backup.as_ref().unwrap()));
    for i in 0..3 {
        let old = format!("2020010{i}T");
        assert!(
            !dumps.iter().any(|d| d.to_str().unwrap().contains(&old)),
            "{dumps:?}"
        );
    }
}

#[test]
fn backup_space_check_needs_newest_plus_ten_percent() {
    use aq_updater::engine::{InsufficientBackupSpace, backup_space_needed, check_backup_space};
    let dir = std::path::Path::new("/var/lib/agent-q/backups");
    for (newest, required) in [(0, 0), (1000, 1100), (1001, 1102), (u64::MAX, u64::MAX)] {
        assert_eq!(backup_space_needed(newest), required, "newest {newest}");
    }
    for (free, newest) in [(1100, 1000), (0, 0)] {
        assert_eq!(check_backup_space(dir, free, newest), Ok(()));
    }

    let err = check_backup_space(dir, 1099, 1000).unwrap_err();
    assert_eq!(
        err,
        InsufficientBackupSpace {
            dir: dir.to_path_buf(),
            free: 1099,
            newest: 1000,
            need: 1100,
        }
    );
    assert!(err.to_string().contains("plus 10%"), "{err}");
}

#[tokio::test]
async fn backup_refused_when_free_space_below_newest_backup_plus_ten_percent() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    let settings = EngineSettings {
        min_free_bytes: 10,
        ..h.settings()
    };
    h.fake.set_backup_size(1000);
    let mut e = h.engine_with(settings);
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    h.publish(&manifest(B, 1, &[1]));
    h.fake.clear_log();
    // Enough at validation; the pull then leaves 1099 bytes, below the
    // 1000-byte newest backup plus 10%.
    h.fake.set_free_seq(&[1_000_000, 1099]);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(
        r.detail.contains("1099 bytes free, 1100 needed"),
        "{}",
        r.detail
    );
    assert_eq!(h.fake.count("pull"), 1);
    assert_no_calls(&h.fake, &["backup", "enter"]);
    assert_eq!(h.fake.running().as_deref(), Some(a.images.server.as_str()));
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.dumps().len(), 1);

    // Exactly the newest backup plus 10% is enough.
    h.fake.set_free_seq(&[1_000_000, 1100]);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
}

fn assert_pruned_backups(h: &H, e: &TestEngine, a_backup: &std::path::Path) {
    let dumps = h.dumps();
    assert!(dumps.iter().any(|d| d == a_backup), "{dumps:?}");
    assert!(dumps.contains(e.journal_state().active_backup.as_ref().unwrap()));
    assert!(
        !dumps.iter().any(|d| d.to_str().unwrap().contains("2020")),
        "{dumps:?}"
    );
    assert!(h.backup_dir.join("notes.txt").exists());
}

fn assert_drain_before_backup(marks: &[(Step, std::time::Instant, bool, usize)], drain: Duration) {
    let at = |want: Step| marks.iter().find(|m| m.0 == want).cloned().unwrap();
    let (_, entered, on_before, _) = at(Step::Maintenance);
    let (_, backed_up, on, backups) = at(Step::BackedUp);
    assert!(!on_before, "Maintenance is persisted before the marker");
    assert!(on, "the marker is on before the backup step");
    assert_eq!(backups, 0, "the dump starts after the drain");
    assert!(
        backed_up.duration_since(entered) >= drain,
        "{:?}",
        backed_up.duration_since(entered)
    );
}
