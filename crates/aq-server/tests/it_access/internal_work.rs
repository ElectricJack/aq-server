//! aq-internal-work queue (spec 0012 §3, §4): blocking work first, one claim
//! per task, failover across the candidate list, and late runners ignored.
use aq_internal_work::{
    Affected, AnswerShape, Candidate, Outcome, Purpose, Reason, Request, Usage, Way, store,
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
    }
}

fn api(provider: &str) -> Candidate {
    Candidate {
        provider: provider.into(),
        model: format!("{provider}-model"),
        reasoning: "medium".into(),
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
