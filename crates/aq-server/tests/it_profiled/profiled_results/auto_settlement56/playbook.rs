use super::*;
use aq_server::{
    playbooks::domain::delivery_decisions::Settled,
    profiled_execution::settlement::{complete_by_playbook, return_by_playbook},
};

async fn auto_env(delivery: &str, status: axum::http::StatusCode) -> AutoEnv {
    let (f, _, report) = Box::pin(prepare_with_policy_checks(
        true,
        true,
        None,
        false,
        Some(default_policy(delivery)),
        true,
    ))
    .await;
    finish_auto_env(f, report, status, ReportSource::Pushed { changed: true }).await
}

async fn complete(env: &AutoEnv) -> Settled {
    complete_by_playbook(&env.f.state, env.report.report_id)
        .await
        .unwrap()
}
async fn send_back(env: &AutoEnv, reason: &str) -> Settled {
    return_by_playbook(&env.f.state, env.report.report_id, reason)
        .await
        .unwrap()
}

#[tokio::test]
async fn work_complete_completes_on_the_graph_receipt_and_releases_the_claim_once() {
    // No check or delivery evidence: only the playbook's decision completes.
    // The release guards (migrations 0059, 0066) accept its receipt.
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    assert!(env.holds_task().await);
    assert_eq!(complete(&env).await, Settled::Completed);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    check_graph_completion(&env);
    assert!(
        !env.holds_task().await,
        "the validated receipt releases the claim"
    );
    assert_eq!(
        settlement_row(&env.f, env.report.report_id).await.0,
        "confirmed"
    );
    // A replay answers the same and writes nothing; a return is refused.
    assert_eq!(complete(&env).await, Settled::Completed);
    assert_eq!(send_back(&env, "policy").await, Settled::Stale);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_graph_version_conflict_is_stale_and_never_dispatched_again() {
    let env = auto_env("not_required", axum::http::StatusCode::CONFLICT).await;
    assert_eq!(complete(&env).await, Settled::Stale);
    check_version_conflict(&env).await;
    assert_eq!(complete(&env).await, Settled::Stale);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert!(env.holds_task().await);
}

#[tokio::test]
async fn a_superseded_report_is_never_completed_or_returned() {
    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    let auth = context(&env.f).await;
    let mut correction = env.report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(env.report.report_id);
    results::record(&env.f.state, &auth, &correction)
        .await
        .unwrap();
    check_stale_decision(&env).await;
    // Recording decided nothing and a stale decision writes no row.
    assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
}

#[tokio::test]
async fn a_cancelled_attempt_is_never_completed_or_returned() {
    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    aq_server::profiled_execution::cancellation::cancel(
        &env.f.state,
        env.report.binding.attempt_id,
        Uuid::new_v4(),
        "owner cancel before the playbook decides",
    )
    .await
    .unwrap();
    check_stale_decision(&env).await;
}

#[tokio::test]
async fn work_return_retries_a_contended_settlement_for_the_same_result() {
    use std::time::Duration;

    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    let pool = env.f.db.pool().await;
    let report_id = env.report.report_id;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report_id}"))
        .execute(&mut *lock)
        .await
        .unwrap();
    let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let unlock_after_timeout = async {
        let waiting = || {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted AND locktype='advisory' AND $1=ANY(pg_blocking_pids(pid)))")
                .bind(backend_pid)
                .fetch_one(&pool)
        };
        tokio::time::timeout(Duration::from_secs(3), async {
            while !waiting().await.unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Keep the lock through the first timeout. The return will
            // have to open a new transaction once the lock is released.
            tokio::time::sleep(Duration::from_millis(520)).await;
        })
        .await
        .unwrap();
        lock.commit().await.unwrap();
    };
    let (_, outcome) = tokio::join!(unlock_after_timeout, send_back(&env, "policy"));
    assert_eq!(outcome, Settled::Returned);
    assert_eq!(settlement_row(&env.f, report_id).await.0, "rework_required");
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
}

#[tokio::test]
async fn work_return_reworks_the_result_once_and_leaves_the_claim_to_the_replacement() {
    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    assert_eq!(
        send_back(&env, "delivery_unpublished").await,
        Settled::Returned
    );
    let row = settlement_row(&env.f, env.report.report_id).await;
    assert_eq!(
        (row.0.as_str(), row.1.as_deref()),
        ("rework_required", Some("delivery_unpublished"))
    );
    let evidence: Value =
        sqlx::query_scalar("SELECT evidence FROM profiled_auto_settlements WHERE report_id=$1")
            .bind(env.report.report_id)
            .fetch_one(&env.f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(
        (&evidence["decided_by"], &evidence["decision"]),
        (&json!("playbook"), &json!("work.return"))
    );
    // No Graph write, and the claim stays until the replacement owner releases it.
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
    let mut db = env.f.db.pool().await.acquire().await.unwrap();
    let task = env.f.selection.lineage.task_id;
    assert!(
        !aq_profiled_state::replacement_sql::delivery_unpublished(&mut db, task)
            .await
            .unwrap(),
        "a playbook return does not trigger the legacy D1 hold"
    );

    // The replacement facts the first call could not prove are swept later.
    let swept = settlement::reworked_without_facts(&env.f.state, None, 8).await;
    assert!(swept.unwrap().contains(&env.report.report_id));
    // A replay answers the same; the result is no longer completed, and a
    // duplicate record of the report keeps the rework.
    assert_eq!(send_back(&env, "policy").await, Settled::Returned);
    assert_eq!(complete(&env).await, Settled::Stale);
    let auth = context(&env.f).await;
    results::record(&env.f.state, &auth, &env.report)
        .await
        .unwrap();
    let row = settlement_row(&env.f, env.report.report_id).await;
    assert_eq!(
        (row.0.as_str(), row.1.as_deref()),
        ("rework_required", Some("delivery_unpublished"))
    );
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
}

#[path = "playbook/enrollment.rs"]
mod enrollment;
#[path = "playbook/owner_retry.rs"]
mod owner_retry;
#[path = "playbook/result_facts.rs"]
mod result_facts;
pub(super) use result_facts::work_reasons;
#[path = "playbook/scheduling.rs"]
mod scheduling;

fn check_graph_completion(env: &AutoEnv) {
    let bodies = env.bodies.lock().unwrap();
    let operation = &bodies[0]["operations"][0];
    assert_eq!(operation["patch"], json!({"task_state":"completed"}));
    assert_eq!(operation["expected_version"], json!(env.expected_version));
    assert_eq!(
        bodies[0]["reason"],
        format!("Playbook accepted retained result {}", env.report.report_id)
    );
}

async fn check_stale_decision(env: &AutoEnv) {
    assert_eq!(complete(env).await, Settled::Stale);
    assert_eq!(send_back(env, "policy").await, Settled::Stale);
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
}

async fn check_version_conflict(env: &AutoEnv) {
    let row = settlement_row(&env.f, env.report.report_id).await;
    assert_eq!(
        (row.0.as_str(), row.1.as_deref()),
        ("held", Some("graph_version_conflict"))
    );
}
#[path = "playbook/publication_recovery.rs"]
mod publication_recovery;
