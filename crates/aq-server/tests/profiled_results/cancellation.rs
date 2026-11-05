//! Public cancellation before a first retained result, using fake stopped evidence.
use super::*;
use common::native_cancellation::cancel;

#[tokio::test]
async fn late_result_after_cancel_is_recorded_not_applied() {
    let (f, start, report) = prepare(true, false).await;
    let (status, receipt) = cancel(
        &f.state,
        start.binding.attempt_id,
        Uuid::new_v4(),
        "late result cannot complete",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    let auth = context(&f).await;
    stopped::record(&f.state, &auth, &native_started::stop_request(&start))
        .await
        .unwrap();
    let recorded = results::record(&f.state, &auth, &report).await.unwrap();
    assert_eq!(json!(recorded)["disposition"], "recorded");
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    let loaded = results::load_locked(&f.state, &mut tx, report.report_id)
        .await
        .unwrap();
    assert_eq!(json!(loaded.report), json!(report));
    tx.rollback().await.unwrap();
    let hot = native_started::state(&f, &start).await;
    assert_eq!(hot["cancellation_id"], receipt["cancellation_id"]);
    let (held, evaluations, completions): (bool, i64, i64) = sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_settlement_evaluations WHERE attempt_id=$1 AND disposition='accepted'),(SELECT count(*) FROM profiled_task_completions WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1")
        .bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(held);
    assert_eq!((evaluations, completions), (0, 0));
}
