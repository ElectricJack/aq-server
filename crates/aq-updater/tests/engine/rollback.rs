use super::*;

#[tokio::test]
async fn unhealthy_b_rolls_back_to_a() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("wait_healthy", 0, "health: unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("wait_healthy"), "{}", r.detail);
    assert_release_running(&h, &e, &a);
    assert_no_pending_release(&e);
    assert!(!h.fake.maintenance_on());
    assert_eq!(h.history()[0], r);
}

#[tokio::test]
async fn readiness_version_mismatch_rolls_back() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    // B's image answers as A: the probe must not accept it.
    h.fake.set_report(
        &b.images.server,
        ReadinessReport {
            version: ver(A),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "cloudflare_access".into(),
        },
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("version"), "{}", r.detail);
    assert_release_running(&h, &e, &a);
}

#[tokio::test]
async fn readiness_checks_revision_database_protocol_and_auth() {
    let mk = |f: &dyn Fn(&mut ReadinessReport)| {
        let b = manifest(B, 1, &[1]);
        let mut r = ReadinessReport {
            version: ver(B),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "cloudflare_access".into(),
        };
        f(&mut r);
        r
    };
    let cases: Vec<(&str, ReadinessReport)> = vec![
        (
            "source_revision",
            mk(&|r| r.source_revision = "other".into()),
        ),
        ("state_format", mk(&|r| r.state_format = 2)),
        ("database", mk(&|r| r.database = "error".into())),
        ("protocol", mk(&|r| r.protocol_max = 2)),
        ("auth_mode", mk(&|r| r.auth_mode = "dev".into())),
    ];
    for (what, report) in cases {
        let (h, mut e, _a) = with_a().await;
        let b = manifest(B, 1, &[1]);
        h.publish(&b);
        h.fake.set_report(&b.images.server, report);
        let r = e.install(req(B)).await.unwrap();
        assert_eq!(r.outcome, Outcome::RolledBack, "{what}: {}", r.detail);
        assert!(r.detail.contains(what), "{what}: {}", r.detail);
    }
}

#[tokio::test]
async fn auth_mode_must_match_settings() {
    // Matching mode passes.
    let h = H::new();
    h.publish(&manifest(A, 1, &[1]));
    h.fake.set_auth_mode("dev");
    let settings = EngineSettings {
        expected_auth_mode: "dev".into(),
        ..h.settings()
    };
    let mut e = h.engine_with(settings);
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);

    // B comes up with dev auth where Access is expected: rolled back.
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_report(
        &b.images.server,
        ReadinessReport {
            version: ver(B),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "dev".into(),
        },
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("auth_mode"), "{}", r.detail);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn rollback_restores_backup_when_state_format_changed() {
    let (h, mut e, a) = with_a().await;
    // B reads format 1 and migrates it to 2; A cannot read 2.
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.fail("wait_healthy", 0, "health: unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);

    assert_restored_before_start(&h, &a);
    assert_eq!(h.fake.db_format(), Some(1), "A's data is back");
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn rollback_skips_restore_when_state_format_same() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("readiness", 0, "connection refused");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_no_calls(&h.fake, &["restore", "stop_server"]);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn failed_rollback_stops_with_needs_operator() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("wait_healthy", 1, "A unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert!(
        r.detail.contains("B unhealthy") && r.detail.contains("A unhealthy"),
        "{}",
        r.detail
    );
    assert_failed_rollback_state(&h, &e, &a, &r);

    // Nothing loops: a restart takes no runtime action and adds no entry,
    // but makes sure maintenance is on even if the marker was lost.
    drop(e);
    h.fake.clear_log();
    h.fake.set_maintenance(false);
    let mut e = h.engine();
    assert_rollback_waits_for_operator(&h, &mut e).await;
    // And new requests are rejected while the operator decides.
    let r2 = e.install(req(B)).await.unwrap();
    assert_eq!(r2.outcome, Outcome::Rejected);
    assert!(h.fake.log().is_empty());
}

#[tokio::test]
async fn first_install_failed_probe_aborts() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    h.fake.fail("wait_healthy", 0, "never healthy");
    let mut e = h.engine();
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted, "{}", r.detail);
    assert!(r.detail.contains("never healthy"), "{}", r.detail);
    assert_eq!(h.fake.count("stop_server"), 1);
    assert!(h.fake.position("start_server").unwrap() < h.fake.position("stop_server").unwrap());
    assert!(h.fake.running().is_none());
    assert!(!h.fake.maintenance_on());
    assert_no_active_release(&e);
}

#[tokio::test]
async fn rollback_restore_selects_the_active_release_before_stopping() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("restore", 0, "pg_restore failed");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    // B was started (and selected), then the rollback selected A again
    // before stopping B, so a manual `compose up` now starts A.
    let select_a = format!("select_release {}", a.images.server);
    let log = h.fake.log();
    let sel = log.iter().position(|c| *c == select_a).expect("A selected");
    let start_b = log
        .iter()
        .position(|c| *c == format!("start_server {}", b.images.server))
        .unwrap();
    let stop = log.iter().position(|c| c == "stop_server").unwrap();
    assert!(start_b < sel && sel < stop, "{log:?}");
    assert_eq!(h.fake.selected().as_deref(), Some(a.images.server.as_str()));
    assert!(h.fake.running().is_none());
}

fn assert_no_pending_release(e: &TestEngine) {
    assert!(e.journal_state().previous.is_none());
    assert!(e.journal_state().current.is_none());
}

fn assert_no_active_release(e: &TestEngine) {
    assert!(e.journal_state().active.is_none());
    assert!(e.journal_state().current.is_none());
}

fn assert_restored_before_start(h: &H, a: &ReleaseManifest) {
    let suffix = format!("-{A}-{B}.dump");
    let backup = h
        .dumps()
        .into_iter()
        .find(|d| d.to_str().unwrap().ends_with(&suffix))
        .unwrap();
    let log = h.fake.log();
    let restore = h.fake.position("restore").expect("restore called");
    assert_eq!(log[restore], format!("restore {}", backup.display()));
    let stop = h.fake.position("stop_server").unwrap();
    let start_a = log
        .iter()
        .rposition(|c| *c == format!("start_server {}", a.images.server))
        .unwrap();
    assert!(stop < restore && restore < start_a, "{log:?}");
}

fn assert_failed_rollback_state(h: &H, e: &TestEngine, a: &ReleaseManifest, entry: &HistoryEntry) {
    let start_a = format!("start_server {}", a.images.server);
    assert_eq!(h.fake.count(&start_a), 1, "exactly one rollback attempt");
    assert_eq!(h.fake.count("wait_healthy"), 2);
    assert!(h.fake.maintenance_on(), "maintenance stays entered");
    assert_eq!(h.fake.count("leave"), 0);
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.step, Step::RollingBack);
    assert!(cur.rollback_attempted);
    match e.status().state {
        UpdateState::FailedNeedsOperator { detail } => assert_eq!(detail, entry.detail),
        other => panic!("expected FailedNeedsOperator, got {other:?}"),
    }
}

async fn assert_rollback_waits_for_operator(h: &H, e: &mut TestEngine) {
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.fake.log(), vec!["enter".to_string()]);
    assert!(h.fake.maintenance_on());
    h.fake.clear_log();
    assert_eq!(
        h.history()
            .iter()
            .filter(|x| x.outcome == Outcome::FailedNeedsOperator)
            .count(),
        1
    );
}
