//! aq-internal-work queue (spec 0012 §3, §4): blocking work first, one claim
//! per task, failover across the candidate list, and late runners ignored.
use aq_internal_work::{
    Affected, AnswerShape, ApiBackend, ApiEndpoint, Candidate, Claimed, Outcome, Purpose, Reason,
    Request, SWEEP_GRACE_MS, Usage, Way, agent_candidate_budget_ms, candidate_budget_ms, store,
    validate,
};
use aq_server::test_support::TestDb;
use serde_json::json;
use sqlx::PgPool;

const NOW: i64 = 1_000_000;

async fn migrated(db: &TestDb) -> PgPool {
    db.store().await;
    db.pool().await
}

fn request(blocking: bool) -> Request {
    Request {
        purpose: Purpose::RoutingClassification,
        instructions: "Classify the task.".into(),
        input: "Task: fix the login page".into(),
        answer: AnswerShape::Choice {
            choices: vec!["small".into(), "large".into()],
            rationale_max_bytes: 512,
        },
        blocking,
        affected: Affected {
            kind: "task".into(),
            id: "T-1".into(),
            project_id: None,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: true,
        profile_override: None,
        agent: None,
    }
}

fn api(provider: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: format!("{provider}-model"),
        reasoning: Some("medium".into()),
        reasoning_reference: format!("v1-{provider}-5749150b@1"),
        options: Default::default(),
        harness: None,
        way: Way::Api,
        credential_index: 0,
    }
}

fn login(provider: &str, harness: &str) -> Candidate {
    Candidate {
        harness: Some(harness.into()),
        way: Way::Login,
        ..api(provider)
    }
}

#[tokio::test]
async fn blocking_tasks_are_claimed_before_older_background_tasks() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let background = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let blocking = store::submit(&pool, &request(true), &[api("openai")], NOW + 10)
        .await
        .unwrap();
    let first = store::claim(&pool, Way::Api, &[], "runner-a", NOW + 20)
        .await
        .unwrap()
        .expect("a queued task");
    assert_eq!(first.id, blocking);
    assert_eq!(first.candidate_index, 0);
    assert_eq!(first.deadline_at_ms, NOW + 10 + 60_000);
    assert_eq!((first.claimed_at_ms, first.candidate_count), (NOW + 20, 1));
    let second = store::claim(&pool, Way::Api, &[], "runner-a", NOW + 30)
        .await
        .unwrap()
        .expect("the background task");
    assert_eq!(second.id, background);
    let row = store::get(&pool, blocking).await.unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.runner.as_deref(), Some("runner-a"));
    assert_eq!(row.claimed_at_ms, Some(NOW + 20));
    assert!(
        store::claim(&pool, Way::Api, &[], "runner-a", NOW + 40)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_task_is_claimed_once_by_concurrent_runners() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let wide = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let id = store::submit(&wide, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let claims = (0..8).map(|i| {
        let pool = wide.clone();
        tokio::spawn(async move {
            store::claim(&pool, Way::Api, &[], &format!("runner-{i}"), NOW + 1)
                .await
                .unwrap()
                .map(|c| c.id)
        })
    });
    let mut won = Vec::new();
    for claim in claims.collect::<Vec<_>>() {
        if let Some(claimed) = claim.await.unwrap() {
            won.push(claimed);
        }
    }
    assert_eq!(won, [id]);
}

#[tokio::test]
async fn failed_candidate_requeues_on_the_next_and_records_the_attempt() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(false),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    let claimed = store::claim(&pool, Way::Api, &[], "r", NOW + 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.candidate.provider, "openai");
    store::fail_candidate(&pool, id, 0, Reason::ProviderError, "HTTP 500", NOW + 105)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.candidate_index, 1);
    assert_eq!(row.runner, None);
    assert_eq!(row.attempts.len(), 1);
    let attempt = &row.attempts[0];
    assert_eq!(
        (attempt.provider.as_str(), attempt.model.as_str()),
        ("openai", "openai-model")
    );
    assert_eq!(attempt.way, Way::Api);
    assert_eq!(attempt.reason, Some(Reason::ProviderError));
    assert_eq!(attempt.detail, "HTTP 500");
    assert_eq!(attempt.elapsed_ms, 100);
    assert!(row.outcome().is_none());

    let next = store::claim(&pool, Way::Api, &[], "r", NOW + 110)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((next.id, next.candidate_index), (id, 1));
    assert_eq!(next.candidate.provider, "anthropic");
    store::complete(
        &pool,
        id,
        1,
        &json!({"choice": "small"}),
        Some(&Usage {
            input_tokens: 10,
            output_tokens: 2,
        }),
        NOW + 150,
    )
    .await
    .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "answered");
    match row.outcome().unwrap() {
        Outcome::Answered {
            answer,
            provider,
            way,
            usage,
            attempts,
            ..
        } => {
            assert_eq!(answer, json!({"choice": "small"}));
            assert_eq!(provider, "anthropic");
            assert_eq!(way, Way::Api);
            assert_eq!(usage.unwrap().output_tokens, 2);
            assert_eq!(attempts[0].reason, Some(Reason::ProviderError));
        }
        other => panic!("expected an answer, got {other:?}"),
    }
}

#[tokio::test]
async fn last_candidate_failure_fails_the_task_with_all_attempts() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(
        &pool,
        id,
        0,
        Reason::ProviderUnavailable,
        "login expired",
        NOW + 1,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW + 2)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 1, Reason::InvalidOutput, "not JSON", NOW + 3)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.finished_at_ms, Some(NOW + 3));
    match row.outcome().unwrap() {
        Outcome::Failed {
            reason,
            detail,
            attempts,
        } => {
            assert_eq!(reason, Reason::InvalidOutput);
            assert_eq!(detail, "not JSON");
            assert_eq!(
                attempts.iter().map(|a| a.reason).collect::<Vec<_>>(),
                [
                    Some(Reason::ProviderUnavailable),
                    Some(Reason::InvalidOutput)
                ]
            );
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    // BudgetExceeded fails at once even with a candidate left.
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::BudgetExceeded, "over budget", NOW + 1)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.reason),
        ("failed", Some(Reason::BudgetExceeded))
    );

    // A candidate left but the task deadline passed: fail with that reason.
    let id = store::submit(
        &pool,
        &request(true),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::DeadlineExceeded, "slow", NOW + 60_000)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(
        (row.status.as_str(), row.reason, row.candidate_index),
        ("failed", Some(Reason::DeadlineExceeded), 0)
    );
}

#[tokio::test]
async fn late_completion_from_a_superseded_candidate_is_ignored() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(
        &pool,
        &request(false),
        &[api("openai"), api("anthropic")],
        NOW,
    )
    .await
    .unwrap();
    store::claim(&pool, Way::Api, &[], "r", NOW).await.unwrap();
    store::fail_candidate(&pool, id, 0, Reason::DeadlineExceeded, "slow", NOW + 10)
        .await
        .unwrap();
    // The superseded runner answers late, while the row is queued again.
    store::complete(&pool, id, 0, &json!({"choice": "large"}), None, NOW + 11)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 0, Reason::ProviderError, "late", NOW + 11)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
    assert_eq!(row.answer, None);
    assert_eq!(row.attempts.len(), 1);

    // Still ignored once the next candidate is running.
    store::claim(&pool, Way::Api, &[], "r", NOW + 12)
        .await
        .unwrap()
        .unwrap();
    store::complete(&pool, id, 0, &json!({"choice": "large"}), None, NOW + 13)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.answer), ("running", None));

    store::complete(&pool, id, 1, &json!({"choice": "small"}), None, NOW + 14)
        .await
        .unwrap();
    // A finished task cannot be overwritten either.
    store::complete(&pool, id, 1, &json!({"choice": "large"}), None, NOW + 15)
        .await
        .unwrap();
    store::fail_candidate(&pool, id, 1, Reason::ProviderError, "late", NOW + 15)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "answered");
    assert_eq!(row.answer, Some(json!({"choice": "small"})));
    assert_eq!(row.finished_at_ms, Some(NOW + 14));
}

#[tokio::test]
async fn login_claim_skips_tasks_whose_candidate_is_api_or_another_harness() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    store::submit(&pool, &request(true), &[api("openai")], NOW)
        .await
        .unwrap();
    store::submit(
        &pool,
        &request(true),
        &[login("anthropic", "claude")],
        NOW + 1,
    )
    .await
    .unwrap();
    let codex = store::submit(&pool, &request(false), &[login("openai", "codex")], NOW + 2)
        .await
        .unwrap();
    let claimed = store::claim(&pool, Way::Login, &["codex"], "r", NOW + 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, codex);
    assert_eq!(claimed.candidate.harness.as_deref(), Some("codex"));
    assert!(
        store::claim(&pool, Way::Login, &["codex"], "r", NOW + 4)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store::claim(&pool, Way::Login, &[], "r", NOW + 4)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn submit_without_candidates_is_failed_no_candidate() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let id = store::submit(&pool, &request(true), &[], NOW)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.reason, Some(Reason::NoCandidate));
    assert_eq!(row.finished_at_ms, Some(NOW));
    assert_eq!(row.workspace_kind, "none");
    assert_eq!(row.escalation, "none");
    assert_eq!(
        (row.affected_kind.as_str(), row.affected_id.as_str()),
        ("task", "T-1")
    );
    assert!(matches!(
        row.outcome(),
        Some(Outcome::Failed {
            reason: Reason::NoCandidate,
            ..
        })
    ));
    assert!(
        store::claim(&pool, Way::Api, &[], "r", NOW)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_bounds_are_enforced() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    assert!(request(false).validate().is_ok());
    let zero_deadline = Request {
        deadline_ms: 0,
        ..request(false)
    };
    assert!(zero_deadline.validate().is_err());
    assert!(
        store::submit(&pool, &zero_deadline, &[api("openai")], NOW)
            .await
            .is_err()
    );
    let big_input = Request {
        input: "x".repeat(2 * 1024 * 1024),
        ..request(false)
    };
    assert!(
        store::submit(&pool, &big_input, &[api("openai")], NOW)
            .await
            .is_err()
    );
    for bad in [
        Request {
            deadline_ms: 900_001,
            ..request(false)
        },
        Request {
            instructions: "x".repeat(256 * 1024 + 1),
            ..request(false)
        },
        Request {
            max_output_bytes: 0,
            ..request(false)
        },
        Request {
            max_output_bytes: 1_048_577,
            ..request(false)
        },
    ] {
        assert!(bad.validate().is_err());
    }
    let mut unknown = serde_json::to_value(request(false)).unwrap();
    unknown["purpose"] = json!("routing_clasification");
    assert!(serde_json::from_value::<Request>(unknown).is_err());
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(none, 0);
}

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
    let passable = |now| store::claim_passable(&pool, "codex", "host", now, 60_000);
    let mut passed = Vec::new();
    while let Some(claimed) = passable(NOW + 1_000).await.unwrap() {
        passed.push(claimed.id);
        store::pass_candidate(&pool, claimed.id, 0, "codex login busy", NOW + 1_000)
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
    let claimed = store::claim_passable(&pool, "codex", "host", NOW + 59_000, 60_000)
        .await
        .unwrap()
        .expect("a blocking task with a later candidate");
    store::pass_candidate(&pool, claimed.id, 0, "codex login busy", NOW + 60_001)
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

/// A request for agent work (spec 0013 §4.2): files in a workspace, an
/// outcome file as the answer, up to 30 minutes.
fn agent_request(blocking: bool) -> Request {
    Request {
        purpose: Purpose::PlaybookCompilation,
        answer: AnswerShape::Json {
            schema: json!({"type": "object", "required": ["kind"]}),
        },
        deadline_ms: 1_800_000,
        max_output_bytes: 1_048_576,
        agent: Some(aq_internal_work::AgentWork {
            files: [("doc.md".to_string(), "# Doc".to_string())].into(),
            max_turns: 200,
        }),
        ..request(blocking)
    }
}

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
    let claim = |agents| store::claim_login(&pool, &["claude"], agents, "host", NOW + 2);
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
    let claim = || store::claim_login(&pool, &["codex"], true, "host", NOW + 5);
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
    let on_api = store::claim(&pool, Way::Api, &[], "server", NOW + 6)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(on_api.id, waiting);
    assert!(
        claim().await.unwrap().is_none(),
        "it may still fail over to Codex"
    );
    store::complete(
        &pool,
        waiting,
        0,
        &json!({"choice": "small"}),
        None,
        NOW + 7,
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

/// Candidate resolution (spec 0012 §5, §6): pure, so plain tests.
mod resolve {
    use aq_intelligence_config::{CredentialConfig, IntelligenceConfig, LoadedRegistry};
    use aq_internal_work::{Reason, Way, resolve};
    use aq_profile_settings::ModelChoice;
    use aq_profiles::{ModelCandidate, ValidatedCatalog};
    use aq_selection::{
        ArtifactRef, AuthorizedCredential, Availability, AvailabilityObservation, CredentialKind,
        Digest, ObservationScope, ObservationSource, Text,
    };
    use serde_json::json;
    use uuid::Uuid;

    const NOW: u64 = 1_000_000;

    pub(super) fn candidate(id: &str, provider: &str, harnesses: &[&str]) -> ModelCandidate {
        ModelCandidate {
            id: id.into(),
            provider: provider.into(),
            model: format!("{provider}-model"),
            reasoning_configuration_reference: reference(provider),
            harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
        }
    }

    /// The catalog reference of `provider`'s fixture candidate: an identifier
    /// like the live ones, never an effort, so the two cannot be confused.
    pub(super) fn reference(provider: &str) -> String {
        format!("v1-{provider}-dcedeb62@1")
    }

    /// The model choices of Settings for the fixture candidates: openai on
    /// Codex at `low` with the fast service tier, anthropic on Claude at
    /// `high`, claude-direct on Claude at `medium`.
    pub(super) fn choices() -> Vec<ModelChoice> {
        [
            (
                "openai",
                "codex",
                "low",
                json!({"reasoning_effort": "low", "service_tier": "fast"}),
            ),
            ("anthropic", "claude", "high", json!({"thinking": "high"})),
            ("claude-direct", "claude", "medium", json!({})),
        ]
        .into_iter()
        .map(|(provider, harness, effort, options)| {
            serde_json::from_value(json!({
                "provider": provider, "model": format!("{provider}-model"),
                "reasoning_configuration_reference": reference(provider),
                "reasoning": effort, "harness": harness, "options": options,
            }))
            .unwrap()
        })
        .collect()
    }

    pub(super) fn catalog(profiles: Vec<(&str, Vec<ModelCandidate>)>) -> ValidatedCatalog {
        let base = aq_profiles::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../aq-profiles/examples/catalog"),
        )
        .unwrap();
        let instructions = base.artifact().instructions.clone();
        let mut catalog = base.artifact().catalog.clone();
        let template = catalog.intelligence[0].clone();
        let engineering = template.id.clone();
        catalog.intelligence = profiles
            .into_iter()
            .map(|(id, candidates)| {
                let mut profile = template.clone();
                profile.id = id.into();
                profile.candidates = candidates;
                profile
            })
            .collect();
        // Agents point at the example's own profile; keep it present.
        if !catalog.intelligence.iter().any(|p| p.id == engineering) {
            let mut keep = template;
            keep.id = engineering;
            catalog.intelligence.push(keep);
        }
        aq_profiles::validate(catalog, instructions).unwrap()
    }

    fn profile_ref(name: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(name).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(name.as_bytes()),
        }
    }

    pub(super) fn credential(name: &str, provider: &str, enabled: bool) -> AuthorizedCredential {
        AuthorizedCredential {
            profile: profile_ref(name),
            provider: provider.into(),
            account_reference: format!("{name}-account"),
            endpoint_reference: format!("{name}-endpoint"),
            revision: 1,
            enabled,
            provisionable: false,
            material_owner: None,
            allowed_backends: aq_selection::default_allowed_backends(),
        }
    }

    pub(super) fn registry(items: Vec<(AuthorizedCredential, CredentialKind)>) -> LoadedRegistry {
        IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: items
                .into_iter()
                .map(|(credential, kind)| CredentialConfig {
                    kind,
                    allowed_backends: aq_selection::default_allowed_backends(),
                    credential,
                    allowed_node_ids: vec![],
                    allow_enrolled_nodes: true,
                    secret_file: None,
                    delivery_mode: None,
                })
                .collect(),
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap()
    }

    fn observation(
        credential: &AuthorizedCredential,
        state: Availability,
        expires_at_ms: u64,
        cooldown_until_ms: u64,
    ) -> AvailabilityObservation {
        AvailabilityObservation {
            observation_id: Uuid::new_v4(),
            scope: ObservationScope {
                credential_revision: credential.revision,
                provider: credential.provider.clone(),
                credential_profile: credential.profile.clone(),
                account_reference: credential.account_reference.clone(),
                endpoint_reference: credential.endpoint_reference.clone(),
                model: None,
                node_id: None,
            },
            source: ObservationSource::Operator {
                principal_id: Uuid::from_u128(1),
            },
            sequence: 1,
            revision: 1,
            state,
            received_at_ms: NOW - 10,
            expires_at_ms,
            cooldown_until_ms,
        }
    }

    fn two_providers() -> (ValidatedCatalog, LoadedRegistry, AuthorizedCredential) {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("a", "anthropic", &["claude-code"]),
                candidate("o", "openai", &["codex"]),
            ],
        )]);
        let login = credential("anthropic-login", "anthropic", true);
        let registry = registry(vec![
            (login.clone(), CredentialKind::SubscriptionLogin),
            (
                credential("openai-key", "openai", true),
                CredentialKind::ApiKey,
            ),
        ]);
        (catalog, registry, login)
    }

    #[test]
    fn candidates_keep_profile_order_and_carry_the_way_of_their_credential() {
        let (catalog, registry, _) = two_providers();
        let resolved = resolve(&catalog, &registry, &choices(), &[], "internal", NOW).unwrap();
        assert_eq!(resolved.profile_id, "internal");
        assert_eq!(resolved.profile_version, 1);
        let got: Vec<_> = resolved
            .candidates
            .iter()
            .map(|c| {
                (
                    c.provider.as_str(),
                    c.way,
                    c.harness.as_deref(),
                    c.credential_index,
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("anthropic", Way::Login, Some("claude"), 0),
                ("openai", Way::Api, None, 1),
            ]
        );
        assert_eq!(resolved.candidates[0].reasoning.as_deref(), Some("high"));
        assert_eq!(resolved.candidates[1].reasoning.as_deref(), Some("low"));
        assert_eq!(resolved.candidates[1].model, "openai-model");
    }

    /// The one candidate of a profile listing openai on a Codex login,
    /// resolved against `choices`.
    fn codex_login(choices: &[ModelChoice]) -> aq_internal_work::Candidate {
        let catalog = catalog(vec![(
            "internal",
            vec![candidate("o", "openai", &["codex"])],
        )]);
        let registry = registry(vec![(
            credential("openai-login", "openai", true),
            CredentialKind::SubscriptionLogin,
        )]);
        let resolved = resolve(&catalog, &registry, choices, &[], "internal", NOW).unwrap();
        assert_eq!(resolved.candidates.len(), 1);
        resolved.candidates[0].clone()
    }

    /// `choices()` with the openai choice's label and options replaced.
    fn openai_choice(label: &str, options: serde_json::Value) -> Vec<ModelChoice> {
        let mut choices = choices();
        choices[0].reasoning = label.into();
        choices[0].options = serde_json::from_value(options).unwrap();
        choices
    }

    #[test]
    fn a_reference_resolves_to_the_effort_and_options_of_its_model_choice() {
        let codex = codex_login(&choices());
        assert_eq!(codex.harness.as_deref(), Some("codex"));
        assert_eq!(codex.reasoning.as_deref(), Some("low"));
        assert_eq!(codex.reasoning_reference, "v1-openai-dcedeb62@1");
        assert_eq!(codex.options["service_tier"], "fast");
    }

    /// The label is for display; `options.reasoning_effort` is the effort.
    #[test]
    fn a_label_that_is_not_an_effort_takes_the_effort_from_the_options() {
        let options = json!({"reasoning_effort": "low", "service_tier": "fast"});
        let codex = codex_login(&openai_choice("low (fast service)", options));
        assert_eq!(codex.reasoning.as_deref(), Some("low"));
        assert_eq!(codex.options["service_tier"], "fast");
        assert_eq!(codex.reasoning_reference, "v1-openai-dcedeb62@1");
    }

    #[test]
    fn a_label_that_is_not_an_effort_without_an_effort_option_leaves_the_harness_default() {
        let codex = codex_login(&openai_choice("Provider default", json!({})));
        assert_eq!(codex.reasoning, None);
        assert!(codex.options.is_empty());
    }

    /// Codex checks its own configuration; the option is not second-guessed.
    #[test]
    fn an_effort_option_the_harness_does_not_list_is_passed_through() {
        let options = json!({"reasoning_effort": "turbo-9"});
        let codex = codex_login(&openai_choice("Turbo", options));
        assert_eq!(codex.reasoning.as_deref(), Some("turbo-9"));
    }

    #[test]
    fn a_reference_without_a_model_choice_on_its_harness_drops_the_candidate() {
        let (catalog, registry, _) = two_providers();
        let mut elsewhere = choices();
        // The anthropic choice exists only for another harness.
        elsewhere[1].harness = "opencode".into();
        let without: Vec<_> = choices()
            .into_iter()
            .filter(|c| c.provider != "anthropic")
            .collect();
        for choices in [elsewhere, without] {
            let resolved = resolve(&catalog, &registry, &choices, &[], "internal", NOW).unwrap();
            let providers: Vec<_> = resolved.candidates.iter().map(|c| &c.provider).collect();
            assert_eq!(providers, ["openai"]);
        }
    }

    #[test]
    fn a_profile_whose_candidates_all_lack_a_model_choice_is_no_candidate() {
        let (catalog, registry, _) = two_providers();
        assert_eq!(
            resolve(&catalog, &registry, &[], &[], "internal", NOW).unwrap_err(),
            Reason::NoCandidate
        );
    }

    #[test]
    fn quota_unavailable_login_is_skipped_so_the_next_provider_is_first() {
        let (catalog, registry, login) = two_providers();
        for state in [
            Availability::QuotaUnavailable,
            Availability::AuthenticationFailed,
            Availability::TemporaryUnavailable,
        ] {
            for cooldown in [0, NOW + 30_000] {
                let seen = [observation(&login, state, NOW + 60_000, cooldown)];
                let resolved =
                    resolve(&catalog, &registry, &choices(), &seen, "internal", NOW).unwrap();
                assert_eq!(
                    resolved.candidates.len(),
                    1,
                    "{state:?} cooldown {cooldown}"
                );
                assert_eq!(resolved.candidates[0].provider, "openai");
            }
            let seen = [observation(&login, state, NOW + 60_000, NOW + 30_000)];
            let resolved =
                resolve(&catalog, &registry, &choices(), &seen, "internal", NOW).unwrap();
            assert_eq!(resolved.candidates.len(), 1, "{state:?}");
            assert_eq!(resolved.candidates[0].provider, "openai");
        }
        let available = [observation(
            &login,
            Availability::Available,
            NOW + 60_000,
            0,
        )];
        let resolved =
            resolve(&catalog, &registry, &choices(), &available, "internal", NOW).unwrap();
        assert_eq!(resolved.candidates[0].provider, "anthropic");
    }

    fn lapsed_without_cooldown(login: &AuthorizedCredential) -> AvailabilityObservation {
        observation(login, Availability::QuotaUnavailable, NOW, 0)
    }

    #[test]
    fn expired_observation_does_not_skip() {
        let (catalog, registry, login) = two_providers();
        let lapsed = observation(&login, Availability::QuotaUnavailable, NOW, NOW + 30_000);
        for seen in [lapsed, lapsed_without_cooldown(&login)] {
            let resolved =
                resolve(&catalog, &registry, &choices(), &[seen], "internal", NOW).unwrap();
            assert_eq!(resolved.candidates[0].provider, "anthropic");
        }
    }

    #[test]
    fn missing_profile_and_profile_without_usable_credentials_are_no_candidate() {
        let (catalog, usable, _) = two_providers();
        assert_eq!(
            resolve(&catalog, &usable, &choices(), &[], "nope", NOW).unwrap_err(),
            Reason::NoCandidate
        );
        let disabled = registry(vec![
            (
                credential("anthropic-login", "anthropic", false),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("elsewhere", "other", true),
                CredentialKind::ApiKey,
            ),
        ]);
        assert_eq!(
            resolve(&catalog, &disabled, &choices(), &[], "internal", NOW).unwrap_err(),
            Reason::NoCandidate
        );
    }

    #[test]
    fn login_candidate_whose_only_harness_is_gemini_is_dropped() {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("g", "google", &["gemini"]),
                candidate("o", "openai", &["codex", "gemini"]),
            ],
        )]);
        let registry = registry(vec![
            (
                credential("google-login", "google", true),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("openai-login", "openai", true),
                CredentialKind::SubscriptionLogin,
            ),
        ]);
        let resolved = resolve(&catalog, &registry, &choices(), &[], "internal", NOW).unwrap();
        assert_eq!(resolved.candidates.len(), 1);
        assert_eq!(resolved.candidates[0].provider, "openai");
        assert_eq!(resolved.candidates[0].harness.as_deref(), Some("codex"));
    }

    #[test]
    fn claude_code_in_the_catalog_resolves_to_the_claude_harness() {
        let catalog = catalog(vec![(
            "internal",
            vec![
                candidate("a", "anthropic", &["gemini", "claude-code"]),
                candidate("c", "claude-direct", &["claude"]),
            ],
        )]);
        let registry = registry(vec![
            (
                credential("a-login", "anthropic", true),
                CredentialKind::SubscriptionLogin,
            ),
            (
                credential("c-login", "claude-direct", true),
                CredentialKind::SubscriptionLogin,
            ),
        ]);
        let resolved = resolve(&catalog, &registry, &choices(), &[], "internal", NOW).unwrap();
        let harnesses: Vec<_> = resolved
            .candidates
            .iter()
            .map(|c| c.harness.as_deref())
            .collect();
        assert_eq!(harnesses, vec![Some("claude"), Some("claude")]);
    }

    #[test]
    fn recent_observation_older_than_the_policy_age_without_cooldown_does_not_skip() {
        let (mut catalog_value, registry, login) =
            (two_providers().0, two_providers().1, two_providers().2);
        let mut raw = catalog_value.artifact().catalog.clone();
        raw.intelligence[0].selection_policy = Some(
            serde_json::from_value(serde_json::json!({
                "schema_version":1,"fallback_on":[],"allow_unknown_availability":true,
                "observation_max_age_ms":1000,"max_output_tokens":1000,"max_attempts":2,
                "max_lineage_elapsed_ms":60000,"required_capabilities":[],
                "budget":{"max_cost_microunits":null,"currency":null,"require_complete_accounting":false,
                    "require_hard_cost_enforcement":false,"unknown_cost":"deny"}
            }))
            .unwrap(),
        );
        let instructions = catalog_value.artifact().instructions.clone();
        catalog_value = aq_profiles::validate(raw, instructions).unwrap();
        let mut old = observation(&login, Availability::QuotaUnavailable, NOW + 60_000, 0);
        old.received_at_ms = NOW - 5_000;
        let resolved = resolve(
            &catalog_value,
            &registry,
            &choices(),
            &[old.clone()],
            "internal",
            NOW,
        )
        .unwrap();
        assert_eq!(resolved.candidates[0].provider, "anthropic");
        old.received_at_ms = NOW - 500;
        let resolved = resolve(
            &catalog_value,
            &registry,
            &choices(),
            &[old],
            "internal",
            NOW,
        )
        .unwrap();
        assert_eq!(resolved.candidates[0].provider, "openai");
    }
}

fn choice_shape() -> AnswerShape {
    AnswerShape::Choice {
        choices: vec!["small".into(), "large".into()],
        rationale_max_bytes: 16,
    }
}

#[test]
fn fenced_json_is_accepted_and_prose_is_invalid_output() {
    let shape = choice_shape();
    let fenced = "```json\n{\"choice\": \"small\", \"rationale\": \"ok\"}\n```";
    assert_eq!(
        validate::answer(&shape, fenced, 4_096).unwrap(),
        json!({"choice": "small", "rationale": "ok"})
    );
    let bare = "```\n{\"choice\": \"large\", \"rationale\": \"\"}\n```";
    assert!(validate::answer(&shape, bare, 4_096).is_ok());
    let err = validate::answer(&shape, "I think the task is small.", 4_096).unwrap_err();
    assert!(err.contains("not JSON"), "{err}");
    let err = validate::answer(&shape, "[1]", 4_096).unwrap_err();
    assert!(err.contains("not a JSON object"), "{err}");
    let prompt = validate::prompt(&request(false));
    assert!(prompt.starts_with("Classify the task.\n\nTask: fix the login page\n\n"));
    assert!(prompt.ends_with(
        "Answer with one JSON object and nothing else: {\"choice\": one of [\"small\",\"large\"], \"rationale\": string}"
    ));
}

#[test]
fn choice_outside_the_list_and_oversized_rationale_are_invalid() {
    let shape = choice_shape();
    let err =
        validate::answer(&shape, r#"{"choice": "huge", "rationale": "x"}"#, 4_096).unwrap_err();
    assert!(err.contains("huge") && err.contains("choices"), "{err}");
    let long = format!(
        r#"{{"choice": "small", "rationale": "{}"}}"#,
        "x".repeat(17)
    );
    let err = validate::answer(&shape, &long, 4_096).unwrap_err();
    assert!(err.contains("rationale") && err.contains("16"), "{err}");
    let err = validate::answer(&shape, r#"{"choice": "small"}"#, 4_096).unwrap_err();
    assert!(err.contains("rationale"), "{err}");
}

#[test]
fn json_answer_is_checked_against_the_schema() {
    let shape = AnswerShape::Json {
        schema: json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"],
            "additionalProperties": false
        }),
    };
    assert_eq!(
        validate::answer(&shape, r#"{"n": 3}"#, 4_096).unwrap(),
        json!({"n": 3})
    );
    assert!(validate::answer(&shape, r#"{"n": "three"}"#, 4_096).is_err());
    assert!(validate::answer(&shape, "{}", 4_096).is_err());
    let secret_text = "s".repeat(1_000);
    let wrong = format!(r#"{{"n": "{secret_text}"}}"#);
    let err = validate::answer(&shape, &wrong, 4_096).unwrap_err();
    assert!(
        err.len() <= 220 && !err.contains(&secret_text),
        "{}",
        err.len()
    );
    let bad_schema = AnswerShape::Json {
        schema: json!({"type": 12}),
    };
    assert!(validate::answer(&bad_schema, "{}", 4_096).is_err());
}

#[test]
fn oversized_output_is_invalid_and_the_error_does_not_echo_it() {
    let shape = choice_shape();
    let big = format!("zzz-marker-{}", "z".repeat(5_000));
    let err = validate::answer(&shape, &big, 100).unwrap_err();
    assert!(err.contains("5011") && err.contains("100"), "{err}");
    assert!(!err.contains("zzz"), "{err}");
    let err = validate::answer(&shape, &format!("{{{}", "y".repeat(5_000)), 9_999).unwrap_err();
    assert!(err.len() <= 220, "{}", err.len());
}

use crate::common::fake_api as fake;

fn secret_file(dir: &std::path::Path, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("key");
    std::fs::write(&path, "sk-test-secret-value\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn backend(endpoint: &str, secret: std::path::PathBuf) -> ApiBackend {
    ApiBackend::new(vec![ApiEndpoint {
        provider: "openai".into(),
        endpoint: endpoint.into(),
        secret_file: secret,
    }])
    .unwrap()
}

fn claimed(provider: &str) -> Claimed {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Claimed {
        id: uuid::Uuid::new_v4(),
        request: request(false),
        candidate: api(provider),
        candidate_index: 0,
        deadline_at_ms: now + 30_000,
        claimed_at_ms: now,
        candidate_count: 1,
    }
}

#[tokio::test]
async fn api_backend_returns_text_and_usage_and_never_stores_the_secret() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let dir = tempfile::tempdir().unwrap();
    let text = r#"{"choice": "small", "rationale": "ok"}"#;
    let (endpoint, seen) = fake::serve(200, fake::completed(text)).await;
    let api_backend = backend(&endpoint, secret_file(dir.path(), 0o600));

    let id = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let held = store::claim(&pool, Way::Api, &[], "r", NOW + 5)
        .await
        .unwrap()
        .unwrap();
    // The claimed deadline is in the past relative to the wall clock.
    let past = api_backend.run(&held).await.unwrap_err();
    assert_eq!(past.0, Reason::DeadlineExceeded);

    let live = Claimed {
        deadline_at_ms: held.deadline_at_ms + 4_000_000_000_000,
        ..held
    };
    let (raw, usage) = api_backend.run(&live).await.unwrap();
    assert_eq!(raw, text);
    let usage = usage.unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.authorization, ["Bearer sk-test-secret-value"]);
        let body = &seen.bodies[0];
        assert_eq!(body["model"], "openai-model");
        assert_eq!(body["store"], false);
        assert!(
            body["input"]
                .as_str()
                .unwrap()
                .contains("Task: fix the login page")
        );
    }
    let answer = validate::answer(&choice_shape(), &raw, 4_096).unwrap();
    store::complete(&pool, id, 0, &answer, Some(&usage), NOW + 50)
        .await
        .unwrap();
    let row = store::get(&pool, id).await.unwrap().unwrap();
    let stored = format!("{row:?}{:?}", row.outcome());
    assert!(!stored.contains("sk-test-secret-value"), "{stored}");

    // Error strings carry no key either.
    let bad = backend(&endpoint, dir.path().join("missing"));
    let err = bad.run(&claimed("openai")).await.unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        !err.1.contains("sk-test") && !err.1.contains("missing"),
        "{}",
        err.1
    );
    let none = api_backend.run(&claimed("anthropic")).await.unwrap_err();
    assert_eq!(
        none,
        (
            Reason::ProviderUnavailable,
            "no api endpoint configured for anthropic".to_string()
        )
    );
}

#[tokio::test]
async fn api_429_and_500_are_unavailable_and_400_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let secret = secret_file(dir.path(), 0o600);
    for (status, reason) in [
        (429, Reason::ProviderUnavailable),
        (408, Reason::ProviderUnavailable),
        (500, Reason::ProviderUnavailable),
        (503, Reason::ProviderUnavailable),
        (400, Reason::ProviderError),
        (401, Reason::ProviderError),
    ] {
        let (endpoint, _) = fake::serve(status, json!({"error": "sk-test-secret-value"})).await;
        let err = backend(&endpoint, secret.clone())
            .run(&claimed("openai"))
            .await
            .unwrap_err();
        assert_eq!(err.0, reason, "HTTP {status}");
        assert!(err.1.contains(&status.to_string()), "{}", err.1);
        assert!(!err.1.contains("sk-test"), "{}", err.1);
    }
    // Nothing listens on this port: a transport error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/responses", closed.local_addr().unwrap());
    drop(closed);
    let err = backend(&endpoint, secret)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    // An incomplete response is a provider error.
    let (endpoint, _) = fake::serve(200, json!({"status": "incomplete"})).await;
    let dir2 = tempfile::tempdir().unwrap();
    let err = backend(&endpoint, secret_file(dir2.path(), 0o600))
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderError);
}

#[tokio::test]
async fn secret_file_that_is_group_readable_or_a_symlink_is_refused() {
    let (endpoint, seen) = fake::serve(200, fake::completed("{}")).await;
    let dir = tempfile::tempdir().unwrap();
    let group = secret_file(dir.path(), 0o640);
    let err = backend(&endpoint, group)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(err.1.contains("unusable"), "{}", err.1);

    let dir = tempfile::tempdir().unwrap();
    let real = secret_file(dir.path(), 0o600);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let err = backend(&endpoint, link)
        .run(&claimed("openai"))
        .await
        .unwrap_err();
    assert_eq!(err.0, Reason::ProviderUnavailable);
    assert!(
        seen.lock().unwrap().authorization.is_empty(),
        "nothing was sent"
    );

    // A relative path and a plain-http remote endpoint are refused up front.
    for (url, file) in [
        (endpoint.as_str(), std::path::PathBuf::from("key")),
        ("http://example.com/v1/responses", real.clone()),
    ] {
        assert!(
            ApiBackend::new(vec![ApiEndpoint {
                provider: "openai".into(),
                endpoint: url.into(),
                secret_file: file,
            }])
            .is_err()
        );
    }
}

/// The service (spec 0012 §3, §4, §8 step 1): Settings pick the profile, the
/// in-server loop runs API candidates in profile order, and the sweeper ends
/// tasks whose runner or deadline is gone.
mod service {
    use super::{fake, login, migrated, request, resolve, secret_file};
    use aq_intelligence_config::LoadedRegistry;
    use aq_internal_work::{
        ApiEndpoint, InternalWorkConfig, Outcome, Reason, Request, SWEEP_GRACE_MS, Way,
        candidate_budget_ms, store,
    };
    use aq_selection::CredentialKind;
    use aq_server::{InternalWork, test_support::TestDb};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::watch;
    use uuid::Uuid;

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn user(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'owner',now())")
            .bind(id)
            .bind(format!("{id}@localhost"))
            .execute(pool)
            .await
            .unwrap();
        id
    }

    /// Activates a catalog whose profile `internal` lists openai, then
    /// anthropic, and makes it the internal work default.
    async fn configure(pool: &PgPool) {
        let catalog = resolve::catalog(vec![(
            "internal",
            vec![
                resolve::candidate("o", "openai", &["codex"]),
                resolve::candidate("a", "anthropic", &["claude-code"]),
            ],
        )]);
        sqlx::query(
            "INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms)
             VALUES($1,$2,$3,1)",
        )
        .bind(catalog.digest())
        .bind(catalog.canonical_bytes().unwrap())
        .bind(user(pool).await)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
            .bind(catalog.digest())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(json!({"default_profile_id": "internal", "overrides": {}}))
            .execute(pool)
            .await
            .unwrap();
    }

    fn registry(anthropic: CredentialKind) -> Arc<LoadedRegistry> {
        Arc::new(resolve::registry(vec![
            (
                resolve::credential("openai-key", "openai", true),
                CredentialKind::ApiKey,
            ),
            (
                resolve::credential("anthropic-key", "anthropic", true),
                anthropic,
            ),
        ]))
    }

    struct Fixture {
        _db: TestDb,
        _dir: tempfile::TempDir,
        /// The Settings model choices file the service reads.
        preferences: std::path::PathBuf,
        pool: PgPool,
        work: InternalWork,
        openai: Arc<std::sync::Mutex<fake::Seen>>,
        anthropic: Arc<std::sync::Mutex<fake::Seen>>,
    }

    /// A configured service whose providers answer `openai` and `anthropic`.
    async fn fixture(openai: &str, anthropic: &str, kind: CredentialKind) -> Fixture {
        let db = TestDb::create().await;
        let pool = migrated(&db).await;
        configure(&pool).await;
        let dir = tempfile::tempdir().unwrap();
        let secret = secret_file(dir.path(), 0o600);
        let preferences = dir.path().join("models.json");
        std::fs::write(&preferences, json!(resolve::choices()).to_string()).unwrap();
        let (openai_url, openai) = fake::serve(200, fake::completed(openai)).await;
        let (anthropic_url, anthropic) = fake::serve(200, fake::completed(anthropic)).await;
        let config = InternalWorkConfig {
            api: [("openai", openai_url), ("anthropic", anthropic_url)]
                .into_iter()
                .map(|(provider, endpoint)| ApiEndpoint {
                    provider: provider.into(),
                    endpoint,
                    secret_file: secret.clone(),
                })
                .collect(),
            poll_ms: 20,
            enabled: true,
        };
        let work = InternalWork::new(
            pool.clone(),
            aq_server::system_clock(),
            Some(&config),
            Some(registry(kind)),
            Some(preferences.clone()),
        )
        .unwrap();
        Fixture {
            _db: db,
            _dir: dir,
            preferences,
            pool,
            work,
            openai,
            anthropic,
        }
    }

    const SMALL: &str = r#"{"choice": "small", "rationale": "one page"}"#;
    const LARGE: &str = r#"{"choice": "large", "rationale": "many pages"}"#;

    /// Runs the in-server loop until the returned sender is dropped.
    fn start(work: &InternalWork) -> watch::Sender<bool> {
        let (stop, rx) = watch::channel(false);
        tokio::spawn(work.clone().serve(rx));
        stop
    }

    async fn wait(work: &InternalWork, id: Uuid) -> Outcome {
        tokio::time::timeout(Duration::from_secs(20), work.wait(id))
            .await
            .expect("the task finishes")
            .unwrap()
    }

    fn answered(outcome: Outcome) -> (Value, String, Vec<aq_internal_work::Attempt>) {
        match outcome {
            Outcome::Answered {
                answer,
                provider,
                way,
                attempts,
                ..
            } => {
                assert_eq!(way, Way::Api);
                (answer, provider, attempts)
            }
            failed => panic!("expected an answer, got {failed:?}"),
        }
    }

    #[tokio::test]
    async fn run_returns_the_validated_answer_from_the_first_candidate() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let _stop = start(&f.work);
        let outcome = tokio::time::timeout(Duration::from_secs(20), f.work.run(request(true)))
            .await
            .expect("run returns")
            .unwrap();
        let usage = match &outcome {
            Outcome::Answered { usage, .. } => usage.clone().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
        let (answer, provider, attempts) = answered(outcome);
        assert_eq!(answer, json!({"choice": "small", "rationale": "one page"}));
        assert_eq!(provider, "openai");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].reason, None);
        assert_eq!(f.openai.lock().unwrap().bodies.len(), 1);
        assert!(f.anthropic.lock().unwrap().bodies.is_empty());
    }

    /// An API call is one prompt, so agent work fails its API candidates over
    /// without calling the provider.
    #[tokio::test]
    async fn agent_work_is_never_sent_to_an_api_candidate() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let _stop = start(&f.work);
        let id = f.work.submit(super::agent_request(true)).await.unwrap();
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason, attempts, ..
            } => {
                assert_eq!(reason, Reason::ProviderUnavailable);
                assert_eq!(attempts.len(), 2);
                for attempt in attempts {
                    assert_eq!(attempt.detail, aq_internal_work::service::AGENT_NEEDS_LOGIN);
                }
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        assert!(f.openai.lock().unwrap().bodies.is_empty());
        assert!(f.anthropic.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn invalid_output_from_the_first_candidate_falls_to_the_second() {
        let f = fixture("I would say small.", LARGE, CredentialKind::ApiKey).await;
        let _stop = start(&f.work);
        let (answer, provider, attempts) = answered(f.work.run(request(false)).await.unwrap());
        assert_eq!(answer["choice"], "large");
        assert_eq!(provider, "anthropic");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "openai");
        assert_eq!(attempts[0].reason, Some(Reason::InvalidOutput));
        assert!(
            attempts[0].detail.contains("not JSON"),
            "{}",
            attempts[0].detail
        );
        assert_eq!(attempts[1].provider, "anthropic");
        assert_eq!(attempts[1].reason, None);
    }

    #[tokio::test]
    async fn no_configured_profile_fails_no_candidate_naming_the_setting() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        sqlx::query("UPDATE internal_work_settings SET document=$1 WHERE singleton")
            .bind(json!({"default_profile_id": null, "overrides": {}}))
            .execute(&f.pool)
            .await
            .unwrap();
        let id = f.work.submit(request(true)).await.unwrap();
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason,
                detail,
                attempts,
            } => {
                assert_eq!(reason, Reason::NoCandidate);
                assert_eq!(detail, "no internal work profile configured");
                assert!(attempts.is_empty());
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        // An explicit override still runs without the setting.
        let _stop = start(&f.work);
        let overridden = Request {
            profile_override: Some("internal".into()),
            ..request(true)
        };
        let (_, provider, _) = answered(f.work.run(overridden).await.unwrap());
        assert_eq!(provider, "openai");
    }

    #[tokio::test]
    async fn a_profile_without_a_usable_model_choice_fails_no_candidate_naming_the_profile() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        // Settings read the choices afresh: none is offered any more.
        std::fs::write(&f.preferences, "[]").unwrap();
        let id = f.work.submit(request(true)).await.unwrap();
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason,
                detail,
                attempts,
            } => {
                assert_eq!(reason, Reason::NoCandidate);
                assert!(detail.contains("\"internal\""), "{detail}");
                assert!(attempts.is_empty());
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        assert!(f.openai.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn a_running_task_whose_runner_vanished_is_swept_and_retried_on_the_next_candidate() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let id = f.work.submit(request(false)).await.unwrap();
        let held = store::claim(&f.pool, Way::Api, &[], "vanished", now_ms())
            .await
            .unwrap()
            .expect("the queued task");
        assert_eq!((held.id, held.candidate.provider.as_str()), (id, "openai"));
        // Within its budget (half of the 60 s deadline) the runner is left alone.
        assert_eq!(store::sweep(&f.pool, now_ms()).await.unwrap(), 0);
        assert_eq!(
            store::get(&f.pool, id).await.unwrap().unwrap().status,
            "running"
        );
        sqlx::query("UPDATE internal_tasks SET claimed_at_ms=$2 WHERE id=$1")
            .bind(id)
            .bind(now_ms() - 126_000)
            .execute(&f.pool)
            .await
            .unwrap();
        let _stop = start(&f.work);
        let (answer, provider, attempts) = answered(wait(&f.work, id).await);
        assert_eq!(
            (answer["choice"].as_str(), provider.as_str()),
            (Some("large"), "anthropic")
        );
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].provider, "openai");
        assert_eq!(attempts[0].reason, Some(Reason::DeadlineExceeded));
        assert_eq!(attempts[0].detail, "runner did not report");
        assert!(f.openai.lock().unwrap().bodies.is_empty());
        // The vanished runner reporting late changes nothing.
        store::complete(&f.pool, id, 0, &json!({"choice": "small"}), None, now_ms())
            .await
            .unwrap();
        let (answer, provider, _) = answered(wait(&f.work, id).await);
        assert_eq!(
            (answer["choice"].as_str(), provider.as_str()),
            (Some("large"), "anthropic")
        );
    }

    /// A service on `f`'s database whose openai and anthropic endpoints are
    /// the given ones.
    fn work_on(
        f: &Fixture,
        dir: &std::path::Path,
        openai: String,
        anthropic: String,
    ) -> InternalWork {
        let secret = secret_file(dir, 0o600);
        let config = InternalWorkConfig {
            api: [("openai", openai), ("anthropic", anthropic)]
                .into_iter()
                .map(|(provider, endpoint)| ApiEndpoint {
                    provider: provider.into(),
                    endpoint,
                    secret_file: secret.clone(),
                })
                .collect(),
            poll_ms: 20,
            enabled: true,
        };
        let registry = Some(registry(CredentialKind::ApiKey));
        let clock = aq_server::system_clock();
        let preferences = Some(f.preferences.clone());
        InternalWork::new(f.pool.clone(), clock, Some(&config), registry, preferences).unwrap()
    }

    /// The loop stops a provider that never answers at the candidate budget
    /// and reports it itself, well before the sweep would; the next
    /// candidate gets what is left.
    #[tokio::test]
    async fn an_api_candidate_that_never_answers_is_reported_by_the_loop_not_the_sweep() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let dir = tempfile::tempdir().unwrap();
        let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
        let work = work_on(&f, dir.path(), fake::hang().await, anthropic);
        let _stop = start(&work);
        // Two candidates, 6 s: the floor gives the first all 6 s and the loop
        // stops it a tenth before that; the sweep would wait 5 s past it.
        let short = Request {
            deadline_ms: 6_000,
            ..request(true)
        };
        let id = work.submit(short).await.unwrap();
        let (claimed_at, deadline) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = store::get(&f.pool, id).await.unwrap().unwrap();
                if let (Some(at), "running") = (row.claimed_at_ms, row.status.as_str()) {
                    return (at, row.deadline_at_ms);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the loop claims the task");
        let attempts = match wait(&work, id).await {
            Outcome::Answered {
                provider, attempts, ..
            } => {
                assert_eq!(provider, "anthropic");
                attempts
            }
            // Only on a machine too loaded to finish the second candidate in
            // the 0.6 s the first left it.
            Outcome::Failed { attempts, .. } => attempts,
        };
        let first = &attempts[0];
        assert_eq!(first.reason, Some(Reason::DeadlineExceeded));
        // The loop's detail ("within 6 s" unless the claim was slow): it
        // reported, not the sweep.
        assert!(
            first.detail.starts_with("openai did not answer within "),
            "{}",
            first.detail
        );
        // Not before the stop, the budget less a tenth of it here (spelled
        // out, so a flat 1 s margin fails), and before the sweep would have
        // taken it.
        let budget = candidate_budget_ms(deadline, claimed_at, 0, 2);
        let stop = (budget - budget / 10) as u64;
        let before_the_sweep = (budget + SWEEP_GRACE_MS) as u64;
        assert!(
            (stop..before_the_sweep).contains(&first.elapsed_ms),
            "{} ms, stop {stop}",
            first.elapsed_ms
        );
    }

    /// At stop the loop gives attempts in flight a few seconds to record
    /// what they got, instead of dropping an answer on the floor.
    #[tokio::test]
    async fn at_stop_an_attempt_in_flight_still_records_its_answer() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let dir = tempfile::tempdir().unwrap();
        let delay = Duration::from_millis(800);
        let (openai, seen) = fake::slow(delay, fake::completed(SMALL)).await;
        let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
        let work = work_on(&f, dir.path(), openai, anthropic);
        let stop = start(&work);
        let id = work.submit(request(true)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while seen.lock().unwrap().bodies.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the provider call is in flight");
        drop(stop);
        let recorded = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = store::get(&f.pool, id).await.unwrap().unwrap();
                if row.status == "answered" {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the answer is recorded after the stop");
        assert_eq!(recorded.answer.unwrap()["choice"], "small");
    }

    /// The owner's requirement: internal work never waits behind other
    /// internal work. A provider call that hangs for most of a 600 s
    /// compile does not keep a blocking 60 s judgment from being claimed.
    #[tokio::test]
    async fn a_blocking_judgment_queued_behind_a_hanging_api_candidate_is_claimed_and_answered() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let dir = tempfile::tempdir().unwrap();
        let (openai, seen) = fake::hang_first(fake::completed(SMALL)).await;
        let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
        let work = work_on(&f, dir.path(), openai, anthropic);
        let _stop = start(&work);
        let compile = Request {
            deadline_ms: 600_000,
            ..request(false)
        };
        let compile = work.submit(compile).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while seen.lock().unwrap().bodies.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the compile's provider call is in flight");
        let judgment = work.submit(request(true)).await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(10), work.wait(judgment))
            .await
            .expect("the judgment is answered while the compile hangs")
            .unwrap();
        let (answer, provider, attempts) = answered(outcome);
        assert_eq!(
            (answer["choice"].as_str(), provider.as_str()),
            (Some("small"), "openai")
        );
        assert_eq!(attempts.len(), 1);
        let row = store::get(&f.pool, compile).await.unwrap().unwrap();
        assert_eq!((row.status.as_str(), row.candidate_index), ("running", 0));
    }

    /// The sweep runs on its own interval: a dead login runner's task moves
    /// on while an unrelated provider call is still in flight.
    #[tokio::test]
    async fn the_sweep_runs_while_a_provider_call_hangs() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let dir = tempfile::tempdir().unwrap();
        let (openai, seen) = fake::hang_first(fake::completed(SMALL)).await;
        let (anthropic, _) = fake::serve(200, fake::completed(LARGE)).await;
        let work = work_on(&f, dir.path(), openai, anthropic);
        let _stop = start(&work);
        let compile = Request {
            deadline_ms: 600_000,
            ..request(false)
        };
        work.submit(compile).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while seen.lock().unwrap().bodies.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the compile's provider call is in flight");
        // A login host claimed this one long ago and died.
        let logins = [login("anthropic", "claude"), login("openai", "codex")];
        let now = now_ms();
        let dead = store::submit(&f.pool, &request(false), &logins, now)
            .await
            .unwrap();
        let held = store::claim(&f.pool, Way::Login, &["claude"], "dead-host", now - 200_000)
            .await
            .unwrap()
            .expect("the login task");
        assert_eq!(held.id, dead);
        let moved = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let row = store::get(&f.pool, dead).await.unwrap().unwrap();
                if row.candidate_index == 1 {
                    return row;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the sweep moved the dead runner's task on");
        assert_eq!(moved.status, "queued");
        assert_eq!(moved.attempts[0].detail, "runner did not report");
    }

    #[tokio::test]
    async fn a_queued_task_past_its_deadline_fails_deadline_exceeded() {
        // Only anthropic resolves, by login: nothing on the server claims it yet.
        let f = fixture(SMALL, LARGE, CredentialKind::SubscriptionLogin).await;
        configure_login_only(&f.pool).await;
        let login_only = Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        let id = f.work.submit(login_only).await.unwrap();
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert_eq!(row.status, "queued");
        assert_eq!(row.candidates[0].way, Way::Login);
        let _stop = start(&f.work);
        match wait(&f.work, id).await {
            Outcome::Failed {
                reason, attempts, ..
            } => {
                assert_eq!(reason, Reason::DeadlineExceeded);
                assert!(attempts.is_empty());
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert!(row.finished_at_ms.unwrap() >= row.deadline_at_ms);
    }

    fn failed_deadline(outcome: Outcome) {
        match outcome {
            Outcome::Failed { reason, .. } => assert_eq!(reason, Reason::DeadlineExceeded),
            answered => panic!("expected a failure, got {answered:?}"),
        }
    }

    #[tokio::test]
    async fn with_internal_work_disabled_the_loop_claims_nothing_but_still_fails_past_deadline() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let config = InternalWorkConfig {
            api: vec![],
            poll_ms: 20,
            enabled: false,
        };
        let registry = Some(registry(CredentialKind::ApiKey));
        let clock = aq_server::system_clock();
        let preferences = Some(f.preferences.clone());
        let disabled =
            InternalWork::new(f.pool.clone(), clock, Some(&config), registry, preferences).unwrap();
        let _stop = start(&disabled);
        let short = Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        let started = std::time::Instant::now();
        let id = disabled.submit(short).await.unwrap();
        failed_deadline(wait(&disabled, id).await);
        // The loop's sweep ends it, well before wait's own sweep would.
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let row = store::get(&f.pool, id).await.unwrap().unwrap();
        assert!(row.attempts.is_empty());
        assert!(f.openai.lock().unwrap().bodies.is_empty());
    }

    #[tokio::test]
    async fn wait_without_a_running_loop_still_fails_a_login_task_past_its_deadline() {
        let f = fixture(SMALL, LARGE, CredentialKind::SubscriptionLogin).await;
        configure_login_only(&f.pool).await;
        let short = || Request {
            deadline_ms: 1_000,
            ..request(true)
        };
        // A login host claims one task and dies without reporting.
        let running = f.work.submit(short()).await.unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let held = store::claim(&f.pool, Way::Login, &["claude"], "dead-host", now)
            .await
            .unwrap()
            .expect("the login task");
        assert_eq!(held.id, running);
        // Nothing claims the other one.
        let queued = f.work.submit(short()).await.unwrap();
        let (queued, running) = tokio::join!(wait(&f.work, queued), wait(&f.work, running));
        failed_deadline(queued);
        match running {
            Outcome::Failed {
                reason, attempts, ..
            } => {
                assert_eq!(reason, Reason::DeadlineExceeded);
                assert_eq!(attempts.len(), 1);
                assert_eq!(attempts[0].detail, "runner did not report");
            }
            answered => panic!("expected a failure, got {answered:?}"),
        }
    }

    /// A second active catalog whose profile `internal` lists only anthropic.
    async fn configure_login_only(pool: &PgPool) {
        let catalog = resolve::catalog(vec![(
            "internal",
            vec![resolve::candidate("a", "anthropic", &["claude-code"])],
        )]);
        sqlx::query(
            "INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms)
             VALUES($1,$2,$3,1)",
        )
        .bind(catalog.digest())
        .bind(catalog.canonical_bytes().unwrap())
        .bind(user(pool).await)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
            .bind(catalog.digest())
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn internal_work_runs_while_swarm_availability_is_off_and_adds_no_swarm_admission_rows() {
        let f = fixture(SMALL, LARGE, CredentialKind::ApiKey).await;
        let owner = user(&f.pool).await;
        let mut conn = f.pool.acquire().await.unwrap();
        let off = aq_machine_control::set_owner_availability(
            &mut conn,
            false,
            1,
            owner,
            Uuid::new_v4(),
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(off.state, aq_machine_control::Availability::Off);
        drop(conn);
        let _stop = start(&f.work);
        let (_, provider, _) = answered(f.work.run(request(true)).await.unwrap());
        assert_eq!(provider, "openai");
        for table in ["swarm_supervisor_admissions", "swarm_waiting"] {
            let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&f.pool)
                .await
                .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }
}

/// The owner-only runs routes (spec 0012 §7) through the real admin router.
mod runs_api {
    use super::*;
    use aq_server::{AppState, InternalWork, config::Config, identity::verifier_from_config};
    use axum::{
        Router,
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use chrono::Utc;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::sync::Arc;
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn app(db: &TestDb, dev_email: &str) -> Router {
        let store = db.store().await;
        let now = Utc::now();
        // A second router over the same database finds both users already there.
        let _ = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await;
        let _ = store
            .invite_user("viewer@localhost", aq_server::store::Role::Viewer, now)
            .await;
        let config: Config = toml::from_str(&format!(
            r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="{dev_email}"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
        ))
        .unwrap();
        let clock = Arc::new(Utc::now);
        let work = InternalWork::new(db.pool().await, clock.clone(), None, None, None).unwrap();
        aq_server::build_admin_router(AppState {
            internal_work: work,
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            store,
            clock,
            agent_access: None,
            source_retention: None,
        })
    }

    async fn call(app: &Router, method: &str, path: &str, csrf: bool) -> (StatusCode, Value) {
        let mut req = HttpRequest::builder().uri(path).method(method);
        if csrf {
            req = req.header("X-AQ-Request", "1");
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    fn about(kind: &str, id: &str, purpose: Purpose) -> Request {
        let mut request = request(false);
        request.purpose = purpose;
        request.affected.kind = kind.into();
        request.affected.id = id.into();
        request
    }

    #[tokio::test]
    async fn runs_are_listed_newest_first_and_filtered_by_affected_item() {
        let db = TestDb::create().await;
        let app = app(&db, "owner@localhost").await;
        let pool = db.pool().await;
        let one = [api("openai")];
        let a = store::submit(
            &pool,
            &about("task", "T-1", Purpose::RoutingClassification),
            &one,
            NOW,
        )
        .await
        .unwrap();
        let b = store::submit(
            &pool,
            &about("task", "T-2", Purpose::PlaybookCall),
            &one,
            NOW + 10,
        )
        .await
        .unwrap();
        let c = store::submit(
            &pool,
            &about("task", "T-1", Purpose::RoutingClassification),
            &one,
            NOW + 20,
        )
        .await
        .unwrap();
        let claimed = store::claim(&pool, Way::Api, &[], "r", NOW + 25)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, a);
        let usage = Usage {
            input_tokens: 7,
            output_tokens: 3,
        };
        let answer = json!({"choice": "small", "rationale": "short"});
        store::complete(&pool, a, 0, &answer, Some(&usage), NOW + 125)
            .await
            .unwrap();

        let (status, all) = call(&app, "GET", "/api/internal-work/runs", false).await;
        assert_eq!(status, StatusCode::OK, "{all}");
        let ids: Vec<&str> = all["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [c.to_string(), b.to_string(), a.to_string()]);
        assert_eq!(all["next_cursor"], Value::Null);

        let (_, one_item) = call(
            &app,
            "GET",
            "/api/internal-work/runs?affected_kind=task&affected_id=T-1",
            false,
        )
        .await;
        let ids: Vec<&str> = one_item["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [c.to_string(), a.to_string()]);
        let (_, by_purpose) = call(
            &app,
            "GET",
            "/api/internal-work/runs?purpose=playbook_call",
            false,
        )
        .await;
        assert_eq!(by_purpose["runs"].as_array().unwrap().len(), 1);
        let (_, answered) = call(
            &app,
            "GET",
            "/api/internal-work/runs?status=answered",
            false,
        )
        .await;
        assert_eq!(answered["runs"][0]["id"], a.to_string());

        let (status, run) = call(&app, "GET", &format!("/api/internal-work/runs/{a}"), false).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run,
            json!({
                "id": a, "purpose": "routing_classification", "status": "answered",
                "affected": {"kind": "task", "id": "T-1", "project_id": null},
                "instructions": "Classify the task.", "input": "Task: fix the login page",
                "answer": answer,
                "attempts": [{"provider": "openai", "model": "openai-model", "way": "api",
                    "reason": null, "detail": "", "elapsed_ms": 100}],
                "provider": "openai", "model": "openai-model", "way": "api", "elapsed_ms": 125,
                "usage": {"input_tokens": 7, "output_tokens": 3},
                "created_at_ms": NOW, "finished_at_ms": NOW + 125, "escalation": "none"
            })
        );

        let (_, page) = call(&app, "GET", "/api/internal-work/runs?limit=2", false).await;
        assert_eq!(page["runs"].as_array().unwrap().len(), 2);
        assert_eq!(page["next_cursor"], format!("{}:{b}", NOW + 10));
        let cursor = page["next_cursor"].as_str().unwrap();
        let (_, rest) = call(
            &app,
            "GET",
            &format!("/api/internal-work/runs?limit=2&cursor={cursor}"),
            false,
        )
        .await;
        assert_eq!(rest["runs"].as_array().unwrap().len(), 1);
        assert_eq!(rest["runs"][0]["id"], a.to_string());
        assert_eq!(rest["next_cursor"], Value::Null);

        for bad in [
            "limit=0",
            "limit=101",
            "cursor=nope",
            "purpose=nope",
            "status=nope",
        ] {
            let (status, _) = call(
                &app,
                "GET",
                &format!("/api/internal-work/runs?{bad}"),
                false,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let missing = Uuid::new_v4();
        let (status, _) = call(
            &app,
            "GET",
            &format!("/api/internal-work/runs/{missing}"),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The run read shows what each candidate's agent did; the list does not.
    #[tokio::test]
    async fn the_run_read_includes_each_candidates_transcript() {
        let db = TestDb::create().await;
        let app = app(&db, "owner@localhost").await;
        let pool = db.pool().await;
        let candidates = [login("openai", "codex"), login("anthropic", "claude")];
        let id = store::submit(&pool, &agent_request(false), &candidates, NOW)
            .await
            .unwrap();
        let steps = json!([{"kind": "tool", "tool": "Bash", "input": "aq-playbooks check"}]);
        store::save_transcript(&pool, id, 1, &steps).await.unwrap();
        store::save_transcript(&pool, id, 0, &json!([]))
            .await
            .unwrap();
        let (status, run) =
            call(&app, "GET", &format!("/api/internal-work/runs/{id}"), false).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run["transcripts"],
            json!([{"candidate_index": 0, "events": []},
                {"candidate_index": 1, "events": steps}])
        );
        let (_, all) = call(&app, "GET", "/api/internal-work/runs", false).await;
        assert!(all["runs"][0].get("transcripts").is_none(), "{all}");
    }

    #[tokio::test]
    async fn retry_resubmits_a_failed_run_and_refuses_a_running_one() {
        let db = TestDb::create().await;
        let app = app(&db, "owner@localhost").await;
        let pool = db.pool().await;
        let original = about("task", "T-9", Purpose::DecisionRevision);
        let failed = store::submit(&pool, &original, &[], NOW).await.unwrap();
        let queued = store::submit(&pool, &original, &[api("openai")], NOW + 1)
            .await
            .unwrap();
        let running = store::submit(&pool, &original, &[api("openai")], NOW + 2)
            .await
            .unwrap();
        store::claim(&pool, Way::Api, &[], "r", NOW + 3)
            .await
            .unwrap();
        let held = store::get(&pool, queued).await.unwrap().unwrap();
        let busy = if held.status == "running" {
            queued
        } else {
            running
        };

        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/internal-work/runs/{failed}/retry"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let new: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        assert_ne!(new, failed);
        let old = store::get(&pool, failed).await.unwrap().unwrap();
        let row = store::get(&pool, new).await.unwrap().unwrap();
        assert_eq!(row.retry_of, Some(failed));
        assert_eq!(row.purpose, old.purpose);
        assert_eq!(row.request.input, old.request.input);
        assert_eq!(row.request.affected.id, "T-9");
        assert_eq!(old.retry_of, None);
        let (_, shown) = call(
            &app,
            "GET",
            &format!("/api/internal-work/runs/{new}"),
            false,
        )
        .await;
        assert_eq!(shown["retry_of"], failed.to_string());

        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/internal-work/runs/{busy}/retry"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = call(
            &app,
            "POST",
            &format!("/api/internal-work/runs/{}/retry", Uuid::new_v4()),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 4);
    }

    #[tokio::test]
    async fn runs_routes_require_owner() {
        let db = TestDb::create().await;
        let owner = app(&db, "owner@localhost").await;
        let pool = db.pool().await;
        let id = store::submit(&pool, &request(false), &[], NOW)
            .await
            .unwrap();
        let viewer = app(&db, "viewer@localhost").await;
        for (method, path) in [
            ("GET", "/api/internal-work/runs".to_string()),
            ("GET", format!("/api/internal-work/runs/{id}")),
            ("POST", format!("/api/internal-work/runs/{id}/retry")),
        ] {
            let (status, _) = call(&viewer, method, &path, true).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
        let (status, _) = call(
            &owner,
            "POST",
            &format!("/api/internal-work/runs/{id}/retry"),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "retry needs the CSRF header");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }
}

/// The failure path (spec 0012 §8): a supervisor first, the owner only when
/// the supervisor cannot fix it.
mod escalation {
    use super::{api, migrated, request};
    use aq_internal_work::{
        FAILURE_BURST_WINDOW_MS, InternalWorkConfig, Purpose, Request, SUPERVISOR_SILENCE_MS, Way,
        service::{NotifySupervisor, Service},
        store::{self, Lineage, Row},
    };
    use aq_server::{InternalWork, test_support::TestDb};
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicI64, Ordering},
        },
        time::Duration,
    };
    use uuid::Uuid;

    const T0: i64 = 1_000_000_000;
    const MINUTE: i64 = 60_000;

    type Told = Arc<Mutex<Vec<(Option<Uuid>, String)>>>;

    /// A service on a clock the test moves, telling a recorder instead of a
    /// supervisor. No profile is configured, so every retry fails at once.
    struct Fixture {
        _db: TestDb,
        pool: PgPool,
        clock: Arc<AtomicI64>,
        told: Told,
        service: Service,
    }

    async fn fixture() -> Fixture {
        let db = TestDb::create().await;
        let pool = migrated(&db).await;
        let clock = Arc::new(AtomicI64::new(T0));
        let told: Told = Arc::default();
        let sink = told.clone();
        let notify: NotifySupervisor = Arc::new(move |project, body| {
            sink.lock().unwrap().push((project, body));
            Box::pin(async { Ok(()) })
        });
        let now = clock.clone();
        let service = Service::new(
            pool.clone(),
            Arc::new(move || now.load(Ordering::SeqCst)),
            None,
            None,
            Arc::new(Vec::new),
        )
        .unwrap()
        .notify_supervisor(notify);
        Fixture {
            _db: db,
            pool,
            clock,
            told,
            service,
        }
    }

    impl Fixture {
        fn at(&self, ms: i64) {
            self.clock.store(ms, Ordering::SeqCst);
        }
        /// A run that failed now: no candidate.
        async fn fail(&self, request: &Request) -> Uuid {
            let now = self.clock.load(Ordering::SeqCst);
            store::submit(&self.pool, request, &[], now).await.unwrap()
        }
        async fn row(&self, id: Uuid) -> Row {
            store::get(&self.pool, id).await.unwrap().unwrap()
        }
        async fn escalation(&self, id: Uuid) -> String {
            self.row(id).await.escalation
        }
        fn told(&self) -> Vec<(Option<Uuid>, String)> {
            self.told.lock().unwrap().clone()
        }
    }

    fn quiet(purpose: Purpose) -> Request {
        Request {
            purpose,
            escalate: false,
            ..request(false)
        }
    }

    fn loud() -> Request {
        Request {
            escalate: true,
            ..request(false)
        }
    }

    #[tokio::test]
    async fn failed_run_with_escalate_messages_the_project_supervisor_once() {
        let db = TestDb::create().await;
        let pool = migrated(&db).await;
        let project = Uuid::new_v4();
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
            .bind(project)
            .bind(json!({"name": "Demo"}))
            .execute(&pool)
            .await
            .unwrap();
        let conversation = |project: Option<Uuid>| {
            let pool = pool.clone();
            async move {
                let id: Uuid = sqlx::query_scalar(
                    "SELECT conversation_id FROM supervisor_conversation_ids
                     WHERE project_id IS NOT DISTINCT FROM $1",
                )
                .bind(project)
                .fetch_one(&pool)
                .await
                .unwrap();
                id
            }
        };
        let (scoped, global) = (conversation(Some(project)).await, conversation(None).await);
        assert_ne!(scoped, global);
        let config = InternalWorkConfig {
            api: vec![],
            poll_ms: 20,
            enabled: true,
        };
        let work = InternalWork::new(
            pool.clone(),
            aq_server::system_clock(),
            Some(&config),
            None,
            None,
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let mut about_project = loud();
        about_project.affected.project_id = Some(project);
        let failed = store::submit(&pool, &about_project, &[], now)
            .await
            .unwrap();
        let unscoped = store::submit(&pool, &loud(), &[], now).await.unwrap();
        let (stop, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(work.clone().serve(rx));
        let mail = |thread: Uuid| {
            let pool = pool.clone();
            async move {
                let rows: Vec<(Value, String, bool)> = sqlx::query_as(
                    "SELECT sender,body,central_global_mail_source_valid(message_id)
                     FROM mailbox_messages WHERE thread_id=$1",
                )
                .bind(thread)
                .fetch_all(&pool)
                .await
                .unwrap();
                rows
            }
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while mail(scoped).await.is_empty() || mail(global).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("both supervisors are told");
        // Further passes of the loop tell nobody again.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(stop);
        let scoped_mail = mail(scoped).await;
        assert_eq!(scoped_mail.len(), 1, "{scoped_mail:?}");
        assert_eq!(mail(global).await.len(), 1);
        let (sender, body, deliverable) = &scoped_mail[0];
        assert_eq!(
            sender,
            &json!({"kind": "system", "component": "internal_work"})
        );
        assert!(deliverable, "a supervisor turn may take the message");
        for part in [
            &failed.to_string(),
            "Purpose: routing_classification",
            &format!("Affected item: task T-1 in project {project}"),
            "Reason: no_candidate: no candidate is configured for this purpose",
            &format!("/v1/supervisor/admin/api/internal-work/runs/{failed}/retry"),
            "Settings",
            &format!("/v1/supervisor/admin/api/internal-work/runs/{failed}/escalate"),
        ] {
            assert!(body.contains(part), "{part:?} in {body}");
        }
        for id in [failed, unscoped] {
            let row = store::get(&pool, id).await.unwrap().unwrap();
            assert_eq!(row.escalation, "supervisor");
        }
    }

    #[tokio::test]
    async fn failed_run_without_escalate_raises_nothing() {
        let f = fixture().await;
        let mut ids = vec![];
        for _ in 0..4 {
            ids.push(f.fail(&quiet(Purpose::RoutingClassification)).await);
        }
        ids.push(f.fail(&quiet(Purpose::PlaybookCall)).await);
        assert_eq!(f.service.escalate().await.unwrap(), 0);
        f.at(T0 + SUPERVISOR_SILENCE_MS + MINUTE);
        assert_eq!(f.service.escalate().await.unwrap(), 0);
        assert!(f.told().is_empty());
        for id in ids {
            let row = f.row(id).await;
            assert_eq!((row.escalation.as_str(), row.diagnosis), ("none", None));
        }
    }

    #[tokio::test]
    async fn five_failures_of_one_purpose_in_ten_minutes_escalate_once() {
        let f = fixture().await;
        let purpose = Purpose::RoutingClassification;
        let mut ids = vec![];
        for minute in 0..5 {
            f.at(T0 + minute * MINUTE);
            ids.push(f.fail(&quiet(purpose)).await);
            // Four failures of another purpose never add up to five.
            if minute < 4 {
                f.fail(&quiet(Purpose::PlaybookCall)).await;
            }
            let told = f.service.escalate().await.unwrap();
            assert_eq!(told, usize::from(minute == 4), "minute {minute}");
        }
        let newest = ids[4];
        assert_eq!(f.escalation(newest).await, "supervisor");
        for id in &ids[..4] {
            assert_eq!(f.escalation(*id).await, "none");
        }
        let told = f.told();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].0, None);
        assert!(told[0].1.contains(&newest.to_string()), "{}", told[0].1);
        assert!(
            told[0].1.contains("failed at least 5 times"),
            "{}",
            told[0].1
        );
        // A sixth failure within the window tells nobody.
        f.at(T0 + 5 * MINUTE);
        let sixth = f.fail(&quiet(purpose)).await;
        assert_eq!(f.service.escalate().await.unwrap(), 0);
        // Ten minutes after the escalation, five failures in the window do.
        f.at(T0 + 4 * MINUTE + FAILURE_BURST_WINDOW_MS + 1);
        for _ in 0..4 {
            f.fail(&quiet(purpose)).await;
        }
        assert_eq!(f.service.escalate().await.unwrap(), 1);
        assert_eq!(f.escalation(sixth).await, "none");
        assert_eq!(f.told().len(), 2);
    }

    #[tokio::test]
    async fn third_failed_supervisor_retry_reaches_the_owner() {
        let f = fixture().await;
        let first = f.fail(&loud()).await;
        assert_eq!(f.service.escalate().await.unwrap(), 1);
        let mut current = first;
        for n in 1..=3 {
            let next = f.service.retry(current, true).await.unwrap();
            let (earlier, retry) = (f.row(current).await, f.row(next).await);
            assert_eq!(earlier.supervisor_attempts, n);
            assert_eq!(
                (
                    retry.supervisor_attempts,
                    retry.retry_of,
                    retry.status.as_str()
                ),
                (n, Some(current), "failed")
            );
            let told = f.service.escalate().await.unwrap();
            let row = f.row(next).await;
            if n < 3 {
                assert_eq!((told, row.escalation.as_str()), (1, "supervisor"), "{n}");
                assert!(f.told().last().unwrap().1.contains(&format!("{n} of 3")));
            } else {
                assert_eq!((told, row.escalation.as_str()), (0, "owner"));
                assert_eq!(
                    row.diagnosis.as_deref(),
                    Some("supervisor attempts exhausted")
                );
            }
            current = next;
        }
        assert_eq!(f.told().len(), 3);
        // Runs a supervisor retried stay with it until the chain settles.
        assert_eq!(f.escalation(first).await, "supervisor");
        f.at(T0 + SUPERVISOR_SILENCE_MS);
        f.service.escalate().await.unwrap();
        assert_eq!(f.escalation(first).await, "supervisor");
        // The owner's own retry is not a supervisor attempt.
        let owners = f.service.retry(current, false).await.unwrap();
        assert_eq!(f.row(current).await.supervisor_attempts, 3);
        assert_eq!(f.row(owners).await.supervisor_attempts, 3);
    }

    #[tokio::test]
    async fn supervisor_escalate_records_its_diagnosis_and_reaches_the_owner() {
        let f = fixture().await;
        let failed = f.fail(&loud()).await;
        f.service.escalate().await.unwrap();
        f.at(T0 + MINUTE);
        let diagnosis = "profile internal lists only a revoked key; the owner must add one";
        f.service
            .escalate_to_owner(failed, diagnosis)
            .await
            .unwrap();
        let row = f.row(failed).await;
        assert_eq!(
            (row.escalation.as_str(), row.diagnosis.as_deref()),
            ("owner", Some(diagnosis))
        );
        assert_eq!(row.escalated_at_ms, Some(T0 + MINUTE));
        // The owner already has it: silence changes nothing.
        f.at(T0 + MINUTE + SUPERVISOR_SILENCE_MS);
        f.service.escalate().await.unwrap();
        assert_eq!(f.row(failed).await.diagnosis.as_deref(), Some(diagnosis));
        for bad in ["", "  ", &"x".repeat(4001)] {
            let refused = f.service.escalate_to_owner(failed, bad).await;
            assert!(
                matches!(refused, Err(aq_internal_work::Error::Invalid(_))),
                "{refused:?}"
            );
        }
        assert!(
            f.service
                .escalate_to_owner(failed, &"x".repeat(4000))
                .await
                .is_ok()
        );
        let missing = f.service.escalate_to_owner(Uuid::new_v4(), "gone").await;
        assert!(matches!(missing, Err(aq_internal_work::Error::NotFound(_))));
        let queued = store::submit(&f.pool, &loud(), &[api("openai")], T0)
            .await
            .unwrap();
        let busy = f.service.escalate_to_owner(queued, "not failed").await;
        assert!(matches!(busy, Err(aq_internal_work::Error::Conflict(_))));
    }

    #[tokio::test]
    async fn silent_supervisor_reaches_the_owner_after_thirty_minutes() {
        let f = fixture().await;
        let silent = f.fail(&loud()).await;
        let retried = f.fail(&loud()).await;
        assert_eq!(f.service.escalate().await.unwrap(), 2);
        f.at(T0 + 10 * MINUTE);
        let retry = f.service.retry(retried, true).await.unwrap();
        assert_eq!(f.service.escalate().await.unwrap(), 1);
        f.at(T0 + SUPERVISOR_SILENCE_MS - 1);
        f.service.escalate().await.unwrap();
        assert_eq!(f.escalation(silent).await, "supervisor");
        f.at(T0 + SUPERVISOR_SILENCE_MS);
        f.service.escalate().await.unwrap();
        let row = f.row(silent).await;
        assert_eq!(
            (row.escalation.as_str(), row.diagnosis.as_deref()),
            ("owner", Some("supervisor did not respond"))
        );
        // A retried run waits on its retry; the retry has its own 30 minutes.
        assert_eq!(f.escalation(retried).await, "supervisor");
        assert_eq!(f.escalation(retry).await, "supervisor");
        f.at(T0 + 10 * MINUTE + SUPERVISOR_SILENCE_MS);
        f.service.escalate().await.unwrap();
        assert_eq!(f.escalation(retried).await, "supervisor");
        assert_eq!(f.escalation(retry).await, "owner");
        assert_eq!(f.told().len(), 3);
    }

    #[tokio::test]
    async fn answered_retry_resolves_the_original_and_removes_the_attention_item() {
        let f = fixture().await;
        let original = f.fail(&loud()).await;
        f.service.escalate().await.unwrap();
        f.service
            .escalate_to_owner(original, "no working model")
            .await
            .unwrap();
        // The owner retries; that retry fails and goes to a supervisor.
        let first = f.service.retry(original, false).await.unwrap();
        assert_eq!(f.service.escalate().await.unwrap(), 1);
        assert_eq!(f.escalation(first).await, "supervisor");
        // The supervisor fixes the cause and retries; this one answers.
        let lineage = Lineage {
            retry_of: first,
            supervisor_attempts: 1,
        };
        let second = store::submit_retry(&f.pool, &loud(), Ok(&[api("openai")]), lineage, T0)
            .await
            .unwrap();
        let claimed = store::claim(&f.pool, Way::Api, &[], "r", T0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, second);
        let answer = json!({"choice": "small", "rationale": "one page"});
        store::complete(&f.pool, second, 0, &answer, None, T0 + 1)
            .await
            .unwrap();
        for id in [original, first] {
            assert_eq!(f.escalation(id).await, "resolved");
        }
        assert_eq!(f.escalation(second).await, "none");
        // The attention source lists runs with escalation 'owner': none is left.
        let owners: i64 =
            sqlx::query_scalar("SELECT count(*) FROM internal_tasks WHERE escalation='owner'")
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(owners, 0);
        // A resolved or never-escalated run cannot be resolved again.
        for id in [original, second] {
            let refused = store::resolve(&f.pool, id).await;
            assert!(matches!(refused, Err(aq_internal_work::Error::Conflict(_))));
        }
    }
}
