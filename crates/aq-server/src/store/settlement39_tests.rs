use super::{MIGRATIONS, StoreError, migrate_known};
use crate::test_support::TestDb;
#[tokio::test]
async fn reviewed_settlement39_catalog_export() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..38], 38).await.unwrap();
    sqlx::raw_sql(include_str!("../../migrations/0039_pilot_settlement.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let actual: serde_json::Value = sqlx::query_scalar(include_str!("settlement39_catalog.sql"))
        .fetch_one(&pool)
        .await
        .unwrap();
    if let Ok(path) = std::env::var("AQ_TEST_EXPORT_SETTLEMENT39_CATALOG") {
        std::fs::write(path, serde_json::to_vec_pretty(&actual).unwrap()).unwrap()
    } else {
        assert_eq!(
            actual,
            serde_json::from_str::<serde_json::Value>(include_str!("settlement39_catalog.json"))
                .unwrap()
        )
    }
}
#[tokio::test]
async fn settlement39_registered_upgrade_old_reader_and_critical_drift() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..38], 38).await.unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t")
            .fetch_one(&pool)
            .await
            .unwrap();
    migrate_known(&pool, &MIGRATIONS[..39], 39).await.unwrap();
    assert_eq!(
        before,
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert!(matches!(
        migrate_known(&pool, &MIGRATIONS[..38], 38).await,
        Err(StoreError::IncompatibleState {
            found: 39,
            supported: 38
        })
    ));
    for mutation in [
        "ALTER TABLE profiled_settlement_evaluations DROP CONSTRAINT profiled_settlement_evaluations_request_digest_check; ALTER TABLE profiled_settlement_evaluations ADD CONSTRAINT profiled_settlement_evaluations_request_digest_check CHECK(TRUE)",
        "ALTER TABLE profiled_task_completions DISABLE TRIGGER settlement_completion_guard",
        "ALTER TABLE profiled_verified_releases DROP CONSTRAINT settlement_release_evaluation_fk",
        "ALTER TABLE review_subjects ALTER COLUMN subject_profile DROP NOT NULL",
        "DROP INDEX settlement_one_accepted",
        "CREATE OR REPLACE FUNCTION settlement_claim_guard() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "ALTER TABLE profiled_task_completions DISABLE TRIGGER ALL",
    ] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(mutation).execute(&mut *tx).await.unwrap();
        assert!(
            super::settlement39::validate(&mut tx).await.is_err(),
            "{mutation}"
        );
        tx.rollback().await.unwrap();
    }
}
