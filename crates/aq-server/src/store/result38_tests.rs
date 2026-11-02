use super::{MIGRATIONS, StoreError, migrate_known};
use crate::test_support::TestDb;
#[tokio::test]
async fn reviewed_result38_catalog_export() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/0038_profiled_result_origins.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let actual: serde_json::Value = sqlx::query_scalar(include_str!("result38_catalog.sql"))
        .fetch_one(&pool)
        .await
        .unwrap();
    if let Ok(path) = std::env::var("AQ_TEST_EXPORT_RESULT38_CATALOG") {
        std::fs::write(path, serde_json::to_vec_pretty(&actual).unwrap()).unwrap();
    } else {
        assert_eq!(
            actual,
            serde_json::from_str::<serde_json::Value>(include_str!("result38_catalog.json"))
                .unwrap()
        );
    }
}
#[tokio::test]
async fn result38_fresh_upgrade_preserves_prior_rows_and_old37_refuses() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..37], 37).await.unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t")
            .fetch_one(&pool)
            .await
            .unwrap();
    migrate_known(&pool, &MIGRATIONS[..38], 38).await.unwrap();
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
        migrate_known(&pool, &MIGRATIONS[..37], 37).await,
        Err(StoreError::IncompatibleState {
            found: 38,
            supported: 37
        })
    ));
    for mutation in [
        "ALTER TABLE profiled_result_reports DROP CONSTRAINT profiled_result_origin_check; ALTER TABLE profiled_result_reports ADD CONSTRAINT profiled_result_origin_check CHECK(TRUE)",
        "ALTER TABLE profiled_result_reports DROP CONSTRAINT profiled_result_stop_origin_fk",
        "ALTER TABLE profiled_result_reports ALTER COLUMN result_origin_kind DROP NOT NULL",
        "ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_phase_unqualified_result_guard",
        "ALTER TABLE profiled_result_reports DISABLE TRIGGER ALL",
        "CREATE OR REPLACE FUNCTION profiled_phase_unqualified_result_guard() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
    ] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(mutation).execute(&mut *tx).await.unwrap();
        assert!(
            super::result38::validate(&mut tx).await.is_err(),
            "accepted {mutation}"
        );
        tx.rollback().await.unwrap();
    }
}
