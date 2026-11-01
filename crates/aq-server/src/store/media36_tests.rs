//! Actual frozen-prefix upgrade/refusal and destructive storage-drift checks.
use super::{MIGRATIONS, StoreError, migrate_known};
use crate::test_support::TestDb;
use serde_json::Value;
use sqlx::PgPool;

async fn database(version: u32) -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_known(&pool, &MIGRATIONS[..version as usize], version)
        .await
        .unwrap();
    (db, pool)
}
async fn rows(pool: &PgPool, table: &str) -> Value {
    sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(row ORDER BY row::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) AS row FROM {table} t) s")).fetch_one(pool).await.unwrap()
}
#[tokio::test]
async fn fresh36_and_actual35_upgrade_preserve_conversation_and_refuse_old_reader() {
    let (_db, pool) = database(35).await;
    let conversation = rows(&pool, "global_supervisor_conversation").await;
    migrate_known(&pool, &MIGRATIONS[..36], 36).await.unwrap();
    assert_eq!(
        rows(&pool, "global_supervisor_conversation").await,
        conversation
    );
    assert_eq!(
        rows(&pool, "result_media_intents").await,
        serde_json::json!([])
    );
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert!(matches!(
        migrate_known(&pool, &MIGRATIONS[..35], 35).await,
        Err(StoreError::IncompatibleState {
            found: 36,
            supported: 35
        })
    ));
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
    migrate_known(&pool, &MIGRATIONS[..36], 36).await.unwrap();
    let (_fresh, fresh) = database(36).await;
    assert_eq!(
        rows(&fresh, "result_media_intents").await,
        serde_json::json!([])
    );
}
async fn corrupt(sql: &str) {
    let (_db, pool) = database(36).await;
    sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert!(
        matches!(migrate_known(&pool,&MIGRATIONS[..36],36).await,Err(StoreError::Corrupt(message))if message.contains("result media")),
        "{sql}"
    );
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
}
#[tokio::test]
async fn missing_disabled_or_wrong_owner_guards_are_refused_without_repair() {
    for (table, guard) in [
        ("result_media_storage", "result_media_storage_update_guard"),
        ("result_media_budgets", "result_media_budget_update_guard"),
        ("result_media_intents", "result_media_intent_insert_guard"),
        ("result_media_intents", "result_media_intent_update_guard"),
        ("result_media_intents", "result_media_intent_delete_guard"),
        ("result_media_intents", "result_media_intent_truncate_guard"),
    ] {
        corrupt(&format!("DROP TRIGGER {guard} ON {table}")).await;
        corrupt(&format!("ALTER TABLE {table} DISABLE TRIGGER {guard}")).await;
    }
    corrupt("DROP INDEX result_media_exact_report").await;
    corrupt("ALTER TABLE result_media_intents DROP CONSTRAINT result_media_receipt_pair_check")
        .await;
    corrupt("ALTER TABLE result_media_intents DROP COLUMN receipt_bytes CASCADE").await;
    corrupt("DROP TABLE result_media_intents CASCADE").await;
    corrupt("CREATE OR REPLACE FUNCTION guard_result_media_intent() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$").await;
}
#[tokio::test]
async fn budget_storage_identity_and_history_cannot_be_erased_or_reset() {
    let (_db, pool) = database(36).await;
    for sql in [
        "UPDATE result_media_budgets SET reserved_bytes=1 WHERE scope_key='installation'",
        "UPDATE result_media_budgets SET historical_intents=1 WHERE scope_key='installation'",
        "DELETE FROM result_media_budgets",
        "TRUNCATE result_media_budgets",
        "TRUNCATE result_media_intents",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
    sqlx::query("INSERT INTO result_media_storage(storage_id,root_fingerprint,created_at_ms) VALUES(gen_random_uuid(),repeat('a',64),1000)").execute(&pool).await.unwrap();
    for sql in [
        "UPDATE result_media_storage SET storage_id=gen_random_uuid()",
        "DELETE FROM result_media_storage",
        "TRUNCATE result_media_storage",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
    corrupt("ALTER TABLE result_media_budgets DISABLE TRIGGER USER; UPDATE result_media_budgets SET historical_intents=1; ALTER TABLE result_media_budgets ENABLE TRIGGER USER").await;
}
