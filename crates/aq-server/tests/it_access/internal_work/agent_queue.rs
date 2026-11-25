use super::*;

/// An agent task is stored with workspace kind `agent`; a request stored
/// before agent work existed (no `agent` key) still decodes as `none` work.
#[tokio::test]
async fn agent_tasks_have_the_agent_workspace_kind_and_older_rows_still_decode() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let agent = store::submit(
        &pool,
        &agent_request(false),
        &[login("openai", "codex")],
        NOW,
    )
    .await
    .unwrap();
    let plain = store::submit(&pool, &request(false), &[login("openai", "codex")], NOW)
        .await
        .unwrap();
    let row = store::get(&pool, agent).await.unwrap().unwrap();
    assert_eq!(row.workspace_kind, "agent");
    assert_eq!(row.request.agent, agent_request(false).agent);
    sqlx::query("UPDATE internal_tasks SET request=request-'agent' WHERE id=$1")
        .bind(plain)
        .execute(&pool)
        .await
        .unwrap();
    let row = store::get(&pool, plain).await.unwrap().unwrap();
    assert_eq!(row.workspace_kind, "none");
    assert!(row.request.agent.is_none());
    let refused = sqlx::query("UPDATE internal_tasks SET workspace_kind='repository' WHERE id=$1")
        .bind(plain)
        .execute(&pool)
        .await;
    assert!(refused.is_err(), "only none and agent are workspace kinds");
}

/// The sweep's SQL gives agent work the same budget as
/// [`agent_candidate_budget_ms`]: five minutes kept for each later candidate.
#[tokio::test]
async fn the_sweep_and_agent_candidate_budget_ms_agree_on_every_budget() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    for (remaining, index, count) in [
        (1_800_000, 0, 2),
        (400_000, 0, 2),
        (1_500_000, 1, 2),
        (1_800_000, 0, 3),
        (1_800_000, 0, 1),
        (200_000, 0, 2),
        (0, 0, 2),
        (-10_000, 1, 2),
    ] {
        let candidates = vec![login("openai", "codex"); count];
        let id = store::submit(&pool, &agent_request(false), &candidates, NOW)
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
        let budget = agent_candidate_budget_ms(NOW + remaining, NOW, index, count);
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

/// A login host that runs an agent already claims only answer tasks; one
/// that runs none takes the agent task in its turn.
#[tokio::test]
async fn a_host_running_an_agent_claims_only_answer_tasks() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let claude = [login("anthropic", "claude")];
    let agent = store::submit(&pool, &agent_request(true), &claude, NOW)
        .await
        .unwrap();
    let answer = store::submit(&pool, &request(false), &claude, NOW + 1)
        .await
        .unwrap();
    let claim = |agents| store::Queue::at(&pool, NOW + 2).claim_login(&["claude"], agents, "host");
    assert_eq!(claim(false).await.unwrap().unwrap().id, answer);
    assert!(
        claim(false).await.unwrap().is_none(),
        "the agent task waits"
    );
    assert_eq!(claim(true).await.unwrap().unwrap().id, agent);
}

/// A Codex agent run holds the Codex login for up to 30 minutes, so it is
/// not claimed while a blocking answer task that ends on Codex is waiting
/// (queued, or running on an earlier candidate it may fail over from).
/// Other agent tasks and tasks with a later non-Codex candidate do not hold
/// it back.
#[tokio::test]
async fn a_codex_agent_task_is_not_claimed_while_a_blocking_task_ends_on_codex() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let agent = store::submit(
        &pool,
        &agent_request(false),
        &[login("openai", "codex")],
        NOW,
    )
    .await
    .unwrap();
    let claim = || store::Queue::at(&pool, NOW + 5).claim_login(&["codex"], true, "host");
    // Neither of these ends on Codex, and another agent task never counts.
    store::submit(
        &pool,
        &request(true),
        &[login("openai", "codex"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    // Served by the server's API loop first, then by this host's Codex.
    let ends_on_codex = [api("anthropic"), login("openai", "codex")];
    let waiting = store::submit(&pool, &request(true), &ends_on_codex, NOW + 1)
        .await
        .unwrap();
    store::submit(&pool, &agent_request(true), &ends_on_codex, NOW + 2)
        .await
        .unwrap();
    // The blocking answer task ahead of it is claimed first; take it away.
    let first = claim().await.unwrap().unwrap();
    assert_ne!(first.id, agent, "a blocking answer task is claimed first");
    assert!(claim().await.unwrap().is_none(), "the agent task must wait");
    let on_api = store::Queue::at(&pool, NOW + 6)
        .claim(Way::Api, &[], "server")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(on_api.id, waiting);
    assert!(
        claim().await.unwrap().is_none(),
        "it may still fail over to Codex"
    );
    store::Queue::at(&pool, NOW + 7)
        .complete(
            store::CandidateAttempt::new(waiting, 0),
            &json!({"choice": "small"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(claim().await.unwrap().unwrap().id, agent);
}

/// A transcript is kept once per candidate and bounded in the database too.
#[tokio::test]
async fn a_transcript_is_kept_once_per_candidate_and_bounded() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &agent_request(false),
        &[login("openai", "codex")],
        NOW,
    )
    .await
    .unwrap();
    let first = json!([{"kind": "text", "text": "reading the document"}]);
    store::save_transcript(&pool, id, 0, &first).await.unwrap();
    store::save_transcript(&pool, id, 0, &json!([]))
        .await
        .unwrap();
    store::save_transcript(&pool, id, 1, &json!([]))
        .await
        .unwrap();
    let kept = store::transcripts(&pool, id).await.unwrap();
    assert_eq!(kept, [(0, first.clone()), (1, json!([]))]);
    let bytes: i32 = sqlx::query_scalar(
        "SELECT bytes FROM internal_task_transcripts WHERE task_id=$1 AND candidate_index=0",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bytes as usize, first.to_string().len());
    let huge = json!(["x".repeat(aq_internal_work::TRANSCRIPT_BYTES)]);
    assert!(store::save_transcript(&pool, id, 2, &huge).await.is_err());
    assert!(
        store::save_transcript(&pool, id, 3, &json!({}))
            .await
            .is_err()
    );
}
