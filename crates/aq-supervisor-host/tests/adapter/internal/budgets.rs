use super::*;

#[tokio::test]
async fn a_refreshed_login_is_copied_back_when_nothing_else_changed_it() {
    let f = Fixture::new(&refreshing()).await;
    let id = f.submit(&[codex_candidate()]).await;
    let other = async {
        f.started().await;
        std::fs::write(f.rec("go"), b"").unwrap();
    };
    let (claimed, ()) = tokio::join!(f.tick(), other);
    assert!(claimed);
    assert_eq!(f.row(id).await.status, "answered");
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("B"));
    let entries: Vec<_> = std::fs::read_dir(f.root().join("login"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["auth.json"], "the lock creates nothing on disk");
}

#[tokio::test]
async fn a_login_in_a_read_only_directory_serves_a_run_that_does_not_refresh_it() {
    let f = Fixture::new(&answering()).await;
    let _read_only = ReadOnly::new(f.root().join("login"));
    let id = f.submit(&[codex_candidate()]).await;
    assert!(f.tick().await);
    let row = f.row(id).await;
    assert_eq!(row.status, "answered", "{:?}", row.attempts);
    assert_eq!(std::fs::read_to_string(f.secret()).unwrap(), login("A"));
}

#[tokio::test]
async fn codex_killed_at_the_candidate_budget_is_deadline_exceeded() {
    // A grandchild holds stdout open: only killing the session ends the run.
    let f = Fixture::new("sleep 30\nprintf '%s' 'late' > \"$out\"").await;
    let short = Request {
        deadline_ms: 1_000,
        ..request()
    };
    let id = f.submit_request(&short, &[codex_candidate()]).await;
    let started = std::time::Instant::now();
    assert!(f.tick().await);
    assert!(started.elapsed() < Duration::from_secs(10));
    let row = f.row(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(aq_internal_work::Reason::DeadlineExceeded));
    let left: Vec<_> = std::fs::read_dir(f.root().join("scratch"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "scratch keeps {left:?}");
}

/// A native that never answers is stopped at its candidate budget and
/// the runner records it, while a sweeper runs as the server's would; the
/// next candidate's budget is what is left of the deadline.
#[tokio::test]
async fn a_native_that_never_answers_is_reported_by_the_runner_before_the_sweep() {
    let silent = "touch \"$rec/started\"\nsleep 30";
    let f = Fixture::with_claude(silent, silent).await;
    // Two candidates, 4 s: the floor gives the first all 4 s.
    let short = Request {
        deadline_ms: 4_000,
        ..request()
    };
    let id = f
        .submit_request(&short, &[codex_candidate(), claude_candidate()])
        .await;
    let done = std::sync::atomic::AtomicBool::new(false);
    let sweeper = async {
        while !done.load(std::sync::atomic::Ordering::SeqCst) {
            store::sweep(&f.pool, now_ms()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    // The first runner leaves at most a tenth of the 4 s. While it runs
    // (its stop was fixed when it was claimed), give the task 2 s more,
    // so a slow machine cannot let the second's remainder expire queued.
    let extend = async {
        f.started().await;
        sqlx::query("UPDATE internal_tasks SET deadline_at_ms=deadline_at_ms+2000 WHERE id=$1")
            .bind(id)
            .execute(&f.pool)
            .await
            .unwrap();
    };
    let runs = async {
        let mut claims = Vec::new();
        for index in 0..2 {
            if index == 1 {
                let row = f.row(id).await;
                assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
            }
            let deadline = f.row(id).await.deadline_at_ms;
            let claimed_at = now_ms();
            tick(&f.config, &f.pool, RUNNER, claimed_at).await.unwrap();
            claims.push((claimed_at, deadline));
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        claims
    };
    let ((), claims, ()) = tokio::join!(sweeper, runs, extend);
    assert_runner_deadlines(&f.row(id).await, &claims);
    for dir in ["scratch", "homes"] {
        let left: Vec<_> = std::fs::read_dir(f.root().join(dir)).unwrap().collect();
        assert!(left.is_empty(), "{dir} keeps {left:?}");
    }
}

fn assert_runner_deadlines(row: &store::Row, claims: &[(i64, i64)]) {
    use aq_internal_work::{Reason, SWEEP_GRACE_MS, candidate_budget_ms};
    for (index, (attempt, harness)) in row.attempts.iter().zip(["codex", "claude"]).enumerate() {
        let (claimed_at, deadline) = claims[index];
        let budget = candidate_budget_ms(deadline, claimed_at, index as u32, 2);
        if index == 1 {
            // The last candidate gets all that is left.
            assert_eq!(budget, deadline - claimed_at);
        }
        // The rule spelled out (budgets under 10 s): a tenth before the end.
        let stop = budget - budget / 10;
        assert_eq!(attempt.reason, Some(Reason::DeadlineExceeded));
        let seconds = (stop + 999) / 1000;
        let detail = format!("{harness} did not answer within {seconds} s");
        // The runner's detail, not the sweep's "runner did not report".
        assert_eq!(attempt.detail, detail, "candidate {index}");
        // Stopped at the stop, and reported before the sweep would have
        // taken it (on an idle machine also before the budget ends).
        let elapsed = attempt.elapsed_ms as i64;
        assert!(
            (stop..budget + SWEEP_GRACE_MS).contains(&elapsed),
            "candidate {index}: {elapsed} ms, stop {stop}, budget {budget}"
        );
    }
    assert_eq!(row.attempts.len(), 2, "{:?}", row.attempts);
    assert_eq!(row.status, "failed");
}
