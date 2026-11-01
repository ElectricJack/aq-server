//! Fixture integration only; synthetic runtime proof is explicitly nonphysical.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod terminal;
#[tokio::test]
async fn qualified_terminal_fixture_preserves_actual_phase_origin_without_legacy_grant() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let execution = terminal::issue(&f).await;
    let request = terminal::started_request(&f, &execution);
    request
        .validate_for(&terminal::context(
            &f,
            &execution,
            &terminal::checkpoint(&f),
        ))
        .unwrap();
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_phase_execution_grants),(SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare'),(SELECT count(*) FROM profiled_phase_terminal_capacity)").fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(counts, (1, 0, 1));
}
