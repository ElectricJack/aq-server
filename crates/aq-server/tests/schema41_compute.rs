//! Isolated PostgreSQL migration/guard qualification, not runtime stop proof.
use aq_server::{
    store::{Store, StoreError},
    test_support::{self, TestDb},
};
use serde_json::Value;

#[tokio::test]
async fn compute41_upgrade_preserves_prior_records_and_old40_refuses() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    test_support::migrate_profile(&pool, 40).await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t")
            .fetch_one(&pool)
            .await
            .unwrap();
    let _store = db.store().await;
    assert_eq!(
        before,
        sqlx::query_scalar::<_, Value>(
            "SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "41"
    );
    assert!(matches!(
        test_support::migrate_profile(&pool, 40).await,
        Err(StoreError::IncompatibleState {
            found: 41,
            supported: 40
        })
    ));
    Store::connect_with(db.options()).await.unwrap();
}

#[tokio::test]
async fn compute41_startup_refuses_predicate_or_incarnation_guard_drift() {
    for mutation in [
        "CREATE OR REPLACE FUNCTION profiled_compute_stopped_valid(attempt UUID,owner_node UUID) RETURNS BOOLEAN LANGUAGE SQL STABLE AS 'SELECT TRUE'",
        "CREATE OR REPLACE FUNCTION native_incarnation_guard() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "ALTER TABLE execution_nodes DISABLE TRIGGER native_incarnation_guard",
    ] {
        let db = TestDb::create().await;
        let _store = db.store().await;
        let pool = db.pool().await;
        sqlx::raw_sql(mutation).execute(&pool).await.unwrap();
        assert!(
            Store::connect_with(db.options()).await.is_err(),
            "guard drift accepted"
        );
    }
}
