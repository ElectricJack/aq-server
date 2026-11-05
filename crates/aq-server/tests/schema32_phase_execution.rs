//! Actual ordered31→32 migration and refusal of missing immutable authority.
//! Empty schema installation is not execution-owner activation.
use aq_server::{
    store::{Store, StoreError},
    test_support::{TestDb, migrate_profile},
};
async fn format(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn present(pool: &sqlx::PgPool) -> bool {
    sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn applied(pool: &sqlx::PgPool) -> (i64, i32) {
    sqlx::query_as("SELECT count(*),max(version) FROM schema_migrations")
        .fetch_one(pool)
        .await
        .unwrap()
}
#[tokio::test]
async fn ordered32_preserves_prior_metadata_and_creates_no_execution_authority() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 31).await.unwrap();
    sqlx::query("INSERT INTO meta(key,value) VALUES('phase32-fixture','unchanged original bytes')")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!present(&pool).await);
    let store = Store::connect_with(db.options()).await.unwrap();
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(
        applied(&pool).await,
        (
            i64::from(aq_server::STATE_FORMAT_VERSION),
            aq_server::STATE_FORMAT_VERSION as i32
        )
    );
    let retained: String = sqlx::query_scalar("SELECT value FROM meta WHERE key='phase32-fixture'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(retained, "unchanged original bytes");
    for table in [
        "profiled_phase_execution_grants",
        "profiled_v2_proposals",
        "profiled_v2_accepts",
        "profiled_execution_receipts",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count, 0,
            "schema installation cannot fabricate authority in {table}"
        );
    }
    store.close().await;
    let reopened = Store::connect_with(db.options()).await.unwrap();
    reopened.close().await;
    assert!(matches!(migrate_profile(&pool, 31).await,
        Err(StoreError::IncompatibleState { found, supported: 31 }) if found == aq_server::STATE_FORMAT_VERSION));
}
#[tokio::test]
async fn real31_table_collision_rolls_back32_and_explicit_fixture_repair_allows_retry() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 31).await.unwrap();
    sqlx::raw_sql("CREATE TABLE profiled_phase_execution_grants(fixture TEXT); INSERT INTO profiled_phase_execution_grants VALUES('preserve collision')")
        .execute(&pool).await.unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(format(&pool).await, "31");
    assert_eq!(applied(&pool).await, (31, 31));
    let collision: String =
        sqlx::query_scalar("SELECT fixture FROM profiled_phase_execution_grants")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(collision, "preserve collision");
    sqlx::query("DROP TABLE profiled_phase_execution_grants")
        .execute(&pool)
        .await
        .unwrap();
    let store = Store::connect_with(db.options()).await.unwrap();
    store.close().await;
    assert!(present(&pool).await);
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
}
#[tokio::test]
async fn failure_after32_sql_meta_transition_rolls_back_table_and_version_together() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 31).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION test_refuse_migration32() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.version=32 THEN RAISE EXCEPTION 'synthetic post-meta32 failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_refuse32 BEFORE INSERT ON schema_migrations FOR EACH ROW EXECUTE FUNCTION test_refuse_migration32();")
        .execute(&pool).await.unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(format(&pool).await, "31");
    assert_eq!(applied(&pool).await, (31, 31));
    assert!(!present(&pool).await);
    sqlx::raw_sql(
        "DROP TRIGGER test_refuse32 ON schema_migrations; DROP FUNCTION test_refuse_migration32();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = Store::connect_with(db.options()).await.unwrap();
    store.close().await;
    assert!(present(&pool).await);
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
}
#[tokio::test]
async fn current32_missing_execution_history_refuses_reopen_without_repair() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    let store = db.store().await;
    store.close().await;
    assert!(present(&pool).await);
    // Newer terminal/result tables reference this immutable history.
    sqlx::query("DROP TABLE profiled_phase_execution_grants CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        Store::connect_with(db.options()).await,
        Err(StoreError::Corrupt(_))
    ));
    assert!(!present(&pool).await);
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(
        applied(&pool).await,
        (
            i64::from(aq_server::STATE_FORMAT_VERSION),
            aq_server::STATE_FORMAT_VERSION as i32
        )
    );
}
