use super::*;
use uuid::Uuid;

struct QueueTask<'a> {
    pool: &'a PgPool,
    id: Uuid,
}

impl<'a> QueueTask<'a> {
    async fn submit(pool: &'a PgPool, blocking: bool) -> Self {
        let candidates = [api("openai"), api("anthropic")];
        let id = store::submit(pool, &request(blocking), &candidates, NOW)
            .await
            .unwrap();
        Self { pool, id }
    }

    fn candidate(&self, index: u32) -> CandidateRun<'_> {
        CandidateRun {
            pool: self.pool,
            key: store::CandidateAttempt::new(self.id, index),
        }
    }

    async fn claim(&self, elapsed: i64) -> Claimed {
        store::Queue::at(self.pool, NOW + elapsed)
            .claim(Way::Api, &[], "r")
            .await
            .unwrap()
            .expect("the queued candidate")
    }

    async fn row(&self) -> store::Row {
        store::get(self.pool, self.id).await.unwrap().unwrap()
    }
}

struct CandidateRun<'a> {
    pool: &'a PgPool,
    key: store::CandidateAttempt,
}

impl CandidateRun<'_> {
    async fn fail(&self, elapsed: i64, reason: Reason, detail: &str) {
        store::Queue::at(self.pool, NOW + elapsed)
            .fail_candidate(self.key, reason, detail)
            .await
            .unwrap();
    }

    async fn answer(&self, elapsed: i64, choice: &str, usage: Option<&Usage>) {
        store::Queue::at(self.pool, NOW + elapsed)
            .complete(self.key, &json!({"choice": choice}), usage)
            .await
            .unwrap();
    }
}

fn assert_requeued(row: store::Row) {
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
}

fn assert_answered(row: store::Row) {
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
async fn failed_candidate_requeues_on_the_next_and_records_the_attempt() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let task = QueueTask::submit(&pool, false).await;
    let claimed = task.claim(5).await;
    assert_eq!(claimed.candidate.provider, "openai");
    task.candidate(0)
        .fail(105, Reason::ProviderError, "HTTP 500")
        .await;
    assert_requeued(task.row().await);

    let next = task.claim(110).await;
    assert_eq!((next.id, next.candidate_index), (task.id, 1));
    assert_eq!(next.candidate.provider, "anthropic");
    task.candidate(1)
        .answer(
            150,
            "small",
            Some(&Usage {
                input_tokens: 10,
                output_tokens: 2,
            }),
        )
        .await;
    assert_answered(task.row().await);
}

#[tokio::test]
async fn last_candidate_failure_fails_the_task_with_all_attempts() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let task = QueueTask::submit(&pool, true).await;
    task.claim(0).await;
    task.candidate(0)
        .fail(1, Reason::ProviderUnavailable, "login expired")
        .await;
    task.claim(2).await;
    task.candidate(1)
        .fail(3, Reason::InvalidOutput, "not JSON")
        .await;
    let row = task.row().await;
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
    let task = QueueTask::submit(&pool, true).await;
    task.claim(0).await;
    task.candidate(0)
        .fail(1, Reason::BudgetExceeded, "over budget")
        .await;
    let row = task.row().await;
    assert_eq!(
        (row.status.as_str(), row.reason),
        ("failed", Some(Reason::BudgetExceeded))
    );

    // A candidate left but the task deadline passed: fail with that reason.
    let task = QueueTask::submit(&pool, true).await;
    task.claim(0).await;
    task.candidate(0)
        .fail(60_000, Reason::DeadlineExceeded, "slow")
        .await;
    let row = task.row().await;
    assert_eq!(
        (row.status.as_str(), row.reason, row.candidate_index),
        ("failed", Some(Reason::DeadlineExceeded), 0)
    );
}

#[tokio::test]
async fn late_completion_from_a_superseded_candidate_is_ignored() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let task = QueueTask::submit(&pool, false).await;
    task.claim(0).await;
    task.candidate(0)
        .fail(10, Reason::DeadlineExceeded, "slow")
        .await;
    // The superseded runner answers late, while the row is queued again.
    task.candidate(0).answer(11, "large", None).await;
    task.candidate(0)
        .fail(11, Reason::ProviderError, "late")
        .await;
    let row = task.row().await;
    assert_eq!((row.status.as_str(), row.candidate_index), ("queued", 1));
    assert_eq!(row.answer, None);
    assert_eq!(row.attempts.len(), 1);

    // Still ignored once the next candidate is running.
    task.claim(12).await;
    task.candidate(0).answer(13, "large", None).await;
    let row = task.row().await;
    assert_eq!((row.status.as_str(), row.answer), ("running", None));

    task.candidate(1).answer(14, "small", None).await;
    // A finished task cannot be overwritten either.
    task.candidate(1).answer(15, "large", None).await;
    task.candidate(1)
        .fail(15, Reason::ProviderError, "late")
        .await;
    let row = task.row().await;
    assert_eq!(row.status, "answered");
    assert_eq!(row.answer, Some(json!({"choice": "small"})));
    assert_eq!(row.finished_at_ms, Some(NOW + 14));
}
