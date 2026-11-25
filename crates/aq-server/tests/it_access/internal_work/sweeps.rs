use super::*;

/// The sweep computes the candidate budget in SQL; for each case it must end
/// the attempt exactly [`SWEEP_GRACE_MS`] after the budget
/// [`candidate_budget_ms`] gives, not a millisecond earlier.
#[tokio::test]
async fn the_sweep_and_candidate_budget_ms_agree_on_every_budget() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    for (remaining, index, count) in [
        (600_000, 0, 2),
        (295_000, 1, 2),
        (120_000, 0, 2),
        (60_000, 1, 2),
        (600_000, 0, 1),
        (100_000, 1, 3),
        (100_001, 0, 2),
        (60_000, 0, 3),
        (20_000, 0, 3),
        (7, 0, 2),
        (450_000, 3, 4),
        (0, 0, 2),
        (-10_000, 1, 2),
    ] {
        let candidates = vec![api("openai"); count];
        let id = store::submit(&pool, &request(false), &candidates, NOW)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE internal_tasks SET status='running', runner='r', claimed_at_ms=$2,
                 deadline_at_ms=$3, candidate_index=$4 WHERE id=$1",
        )
        .bind(id)
        .bind(NOW)
        .bind(NOW + remaining)
        .bind(index as i32)
        .execute(&pool)
        .await
        .unwrap();
        let budget = candidate_budget_ms(NOW + remaining, NOW, index, count);
        let case = format!("remaining {remaining}, candidate {index} of {count}: budget {budget}");
        let held = |row: store::Row| row.status == "running" && row.candidate_index == index;
        let last_moment = NOW + budget + SWEEP_GRACE_MS;
        store::sweep(&pool, last_moment).await.unwrap();
        let row = store::get(&pool, id).await.unwrap().unwrap();
        assert!(held(row), "swept too early; {case}");
        store::sweep(&pool, last_moment + 1).await.unwrap();
        let row = store::get(&pool, id).await.unwrap().unwrap();
        assert!(!held(row), "not swept; {case}");
    }
}

/// While a host's Codex login is busy it passes over the Codex candidate of
/// a task that cannot wait: one with a later candidate that is blocking or
/// has less than 60 s left. A task whose last candidate is Codex, or that
/// has time, is left to wait.
#[tokio::test]
async fn only_tasks_that_cannot_wait_are_passed_over_while_a_login_is_busy() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let fallback = [login("openai", "codex"), login("anthropic", "claude")];
    let only_codex = [login("openai", "codex")];
    let long = |blocking| Request {
        deadline_ms: 600_000,
        ..request(blocking)
    };
    let mut ids = Vec::new();
    for (request, candidates) in [
        (long(false), &fallback[..]),
        (long(true), &only_codex[..]),
        (long(true), &fallback[..]),
        (request(false), &fallback[..]),
    ] {
        ids.push(
            store::submit(&pool, &request, candidates, NOW)
                .await
                .unwrap(),
        );
    }
    let [waits_long, waits_alone, blocking, short] = ids[..] else {
        unreachable!()
    };
    let passable = |now| store::Queue::at(&pool, now).claim_passable("codex", "host", 60_000);
    let mut passed = Vec::new();
    while let Some(claimed) = passable(NOW + 1_000).await.unwrap() {
        passed.push(claimed.id);
        store::Queue::at(&pool, NOW + 1_000)
            .pass_candidate(
                store::CandidateAttempt::new(claimed.id, 0),
                "codex login busy",
            )
            .await
            .unwrap();
    }
    assert_eq!(passed, [blocking, short]);
    for id in [blocking, short] {
        let row = store::get(&pool, id).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
        assert_eq!(row.attempts[0].reason, Some(Reason::LoginBusy));
        assert_eq!(row.attempts[0].detail, "codex login busy");
    }
    for id in [waits_long, waits_alone] {
        let row = store::get(&pool, id).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 0));
        assert!(row.attempts.is_empty());
    }
}

/// Passing a candidate over never fails a task with `login_busy`: if the
/// deadline passed meanwhile, the task fails as a queued task past its
/// deadline does.
#[tokio::test]
async fn passing_over_past_the_deadline_fails_the_task_as_queued_past_its_deadline() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let candidates = [login("openai", "codex"), login("anthropic", "claude")];
    let id = store::submit(&pool, &request(true), &candidates, NOW)
        .await
        .unwrap();
    let claimed = store::Queue::at(&pool, NOW + 59_000)
        .claim_passable("codex", "host", 60_000)
        .await
        .unwrap()
        .expect("a blocking task with a later candidate");
    store::Queue::at(&pool, NOW + 60_001)
        .pass_candidate(
            store::CandidateAttempt::new(claimed.id, 0),
            "codex login busy",
        )
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(Reason::DeadlineExceeded));
    assert_eq!(
        row.detail.as_deref(),
        Some("deadline passed before a runner took the task")
    );
    assert!(row.attempts.is_empty());
}
