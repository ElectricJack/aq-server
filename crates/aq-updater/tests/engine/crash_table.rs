use super::*;
use aq_updater::journal::JournalState;

/// One row of the crash table: crash right after `step` was persisted.
struct CrashCase {
    step: Step,
    /// Script the fake so the next runtime call after `step` crashes.
    script: fn(&FakeRuntime),
    /// Rewrite the persisted journal before recovery (seed a state the
    /// engine itself never writes).
    rollback_attempted_override: Option<bool>,
    /// The crash came after the outcome line was appended. With
    /// `drop_history_line` the line is removed again, i.e. the crash landed
    /// between the journal write and the history append.
    recorded: bool,
    drop_history_line: bool,
    outcome: Outcome,
    active: &'static str,
    maintenance_on: bool,
}

impl CrashCase {
    fn before_commit(step: Step, script: fn(&FakeRuntime), outcome: Outcome) -> Self {
        Self {
            step,
            script,
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome,
            active: A,
            maintenance_on: false,
        }
    }
}

fn forward_step_crashes() -> [CrashCase; 7] {
    [
        CrashCase::before_commit(
            Step::Requested,
            |f| f.crash("server_state_format", 0),
            Outcome::Aborted,
        ),
        CrashCase::before_commit(
            Step::Validated,
            |f| f.crash("free_bytes", 0),
            Outcome::Aborted,
        ),
        CrashCase::before_commit(Step::Pulled, |f| f.crash("pull", 0), Outcome::Aborted),
        CrashCase::before_commit(Step::Maintenance, |f| f.crash("enter", 0), Outcome::Aborted),
        CrashCase::before_commit(Step::BackedUp, |f| f.crash("backup", 0), Outcome::Aborted),
        CrashCase::before_commit(
            Step::Replacing,
            |f| f.crash("start_server", 0),
            Outcome::RolledBack,
        ),
        CrashCase::before_commit(
            Step::Probing,
            |f| f.crash("wait_healthy", 0),
            Outcome::RolledBack,
        ),
    ]
}

fn activation_crashes() -> [CrashCase; 2] {
    [
        // Swap persisted, crash before the history line: recovery finishes.
        CrashCase {
            step: Step::Active,
            script: |f| f.crash("leave", 0),
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: true,
            outcome: Outcome::Succeeded,
            active: B,
            maintenance_on: false,
        },
        // Outcome recorded, crash after leave(): recovery only clears.
        CrashCase {
            step: Step::Active,
            script: |f| f.crash_after("leave", 0),
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: false,
            outcome: Outcome::Succeeded,
            active: B,
            maintenance_on: false,
        },
    ]
}

fn rollback_crashes() -> [CrashCase; 4] {
    [
        // Rollback finished and maintenance left, but the history line is
        // missing: the engine cannot tell the rollback worked, so it stops
        // for the operator and turns maintenance back on.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash_after("leave", 0);
            },
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: true,
            outcome: Outcome::FailedNeedsOperator,
            active: A,
            maintenance_on: true,
        },
        // Same crash with the line present: history is written before
        // leave(), so the rollback is known to have worked.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash_after("leave", 0);
            },
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
        // Crash during the rollback itself: never retried automatically.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash("start_server", 1);
            },
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::FailedNeedsOperator,
            active: A,
            maintenance_on: true,
        },
        // RollingBack persisted without the attempt flag: roll back now.
        CrashCase {
            step: Step::RollingBack,
            script: |f| f.crash("wait_healthy", 0),
            rollback_attempted_override: Some(false),
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
    ]
}

/// Crash right after each persisted step of an A -> B install, restart, and
/// check what recovery concludes. B has `b_format`; when it differs from A's
/// (1), B migrates the data and every rollback must restore it.
async fn run_crash_table(b_format: u32, b_readable: &[u32]) {
    let cases: Vec<_> = forward_step_crashes()
        .into_iter()
        .chain(activation_crashes())
        .chain(rollback_crashes())
        .collect();
    // Every persisted step is covered.
    for s in [
        Step::Requested,
        Step::Validated,
        Step::Pulled,
        Step::Maintenance,
        Step::BackedUp,
        Step::Replacing,
        Step::Probing,
        Step::Active,
        Step::RollingBack,
    ] {
        assert!(cases.iter().any(|c| c.step == s), "no crash case for {s:?}");
    }

    for c in &cases {
        run_crash_case(c, b_format, b_readable).await;
    }
}

async fn run_crash_case(c: &CrashCase, b_format: u32, b_readable: &[u32]) {
    let label = format!(
        "B format {b_format}: {:?} (override {:?}, recorded {}, dropped {})",
        c.step, c.rollback_attempted_override, c.recorded, c.drop_history_line
    );
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, b_format, b_readable);
    h.publish(&b);
    (c.script)(&h.fake);
    let request = req(B);
    assert!(
        e.install(request.clone()).await.is_err(),
        "{label}: expected a crash"
    );
    drop(e);

    verify_persisted_step(&h, c, &label);
    if c.drop_history_line {
        remove_last_history_line(&h, request.id, &label);
    }
    let kept = c.recorded && !c.drop_history_line;
    let history_before = h.history().len();
    h.fake.clear_log();

    let mut e = h.engine();
    let out = match e.recover().await.unwrap() {
        Some(out) => {
            assert!(!kept, "{label}: outcome was already recorded");
            out
        }
        None => {
            assert!(kept, "{label}: no outcome");
            h.history()[0].clone()
        }
    };
    assert_eq!(out.outcome, c.outcome, "{label}: {}", out.detail);
    assert_eq!(out.request_id, Some(request.id), "{label}");
    assert_eq!(active_version(&e), Some(ver(c.active)), "{label}");
    if c.outcome != Outcome::FailedNeedsOperator {
        assert_running(&h.fake, e.journal_state(), &label);
    }
    if c.outcome == Outcome::RolledBack && !kept {
        assert_rollback(&h.fake, &a, b_format, &label);
    }
    assert_eq!(
        h.fake.maintenance_on(),
        c.maintenance_on,
        "{label}: maintenance"
    );
    assert_eq!(
        e.journal_state().current.is_some(),
        c.outcome == Outcome::FailedNeedsOperator,
        "{label}: in-flight cleared"
    );
    // Exactly one history entry per terminal outcome.
    let added = if kept { 0 } else { 1 };
    assert_eq!(h.history().len(), history_before + added, "{label}");
    assert_eq!(h.history()[0], out, "{label}");

    // A second restart is a no-op.
    drop(e);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None, "{label}: second recover");
    assert_eq!(h.history().len(), history_before + added, "{label}");
    assert_eq!(h.fake.maintenance_on(), c.maintenance_on, "{label}");
}

#[tokio::test]
async fn recover_after_crash_at_each_step() {
    run_crash_table(1, &[1]).await;
}

#[tokio::test]
async fn recover_after_crash_at_each_step_with_state_format_change() {
    run_crash_table(2, &[1, 2]).await;
}

fn verify_persisted_step(h: &H, c: &CrashCase, label: &str) {
    let mut on_disk = Journal::read(&h.state_dir).unwrap();
    let cur = on_disk.current.as_mut().expect("in flight after crash");
    let expected_step = if c.rollback_attempted_override.is_some() {
        Step::Probing
    } else {
        c.step
    };
    assert_eq!(cur.step, expected_step, "{label}: persisted step");
    if let Some(flag) = c.rollback_attempted_override {
        cur.step = c.step;
        cur.rollback_attempted = flag;
        let mut j = Journal::open(lock(&h.state_dir).unwrap()).unwrap();
        j.record(on_disk.clone()).unwrap();
    }
}

fn remove_last_history_line(h: &H, request_id: Uuid, label: &str) {
    let path = h.state_dir.join("history.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    let last: HistoryEntry = serde_json::from_str(lines.pop().unwrap()).unwrap();
    assert_eq!(last.request_id, Some(request_id), "{label}: dropped line");
    let rest: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, rest).unwrap();
}

fn assert_running(fake: &FakeRuntime, journal: &JournalState, label: &str) {
    let active_image = journal.active.as_ref().unwrap().images.server.clone();
    assert_eq!(
        fake.running(),
        Some(active_image),
        "{label}: active running"
    );
    let active_format = journal.active.as_ref().unwrap().state_format;
    assert_eq!(fake.db_format(), Some(active_format), "{label}: data");
}

fn assert_rollback(fake: &FakeRuntime, a: &ReleaseManifest, b_format: u32, label: &str) {
    let log = fake.log();
    let start_a = fake
        .position(&format!("start_server {}", a.images.server))
        .unwrap_or_else(|| panic!("{label}: A not started: {log:?}"));
    if b_format != a.state_format {
        // B may have migrated the data: restore before starting A.
        let stop = fake.position("stop_server");
        let restore = fake.position("restore");
        assert!(
            stop.is_some_and(|s| restore.is_some_and(|r| s < r && r < start_a)),
            "{label}: expected stop, restore, start(A): {log:?}"
        );
    } else {
        assert_eq!(fake.count("restore"), 0, "{label}: {log:?}");
    }
}
