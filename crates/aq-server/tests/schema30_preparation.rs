//! Real ordered29->30 migration, rollback, reopen and older-profile refusal.
use aq_server::{
    store::Store,
    test_support::{TestDb, migrate_profile},
};
async fn format(pool: &sqlx::PgPool) -> String {
    sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn present(pool: &sqlx::PgPool, name: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}
#[tokio::test]
async fn fresh_ordered30_is_empty_and_reopens_without_activating_any_owner() {
    let db = TestDb::create().await;
    let first = db.store().await;
    let pool = db.pool().await;
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    let records: (i64, i32) = sqlx::query_as("SELECT count(*),max(version) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        records,
        (
            i64::from(aq_server::STATE_FORMAT_VERSION),
            aq_server::STATE_FORMAT_VERSION as i32
        )
    );
    for name in ["profiled_v2_proposals", "profiled_v2_accepts"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {name}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0)
    }
    let triggers:i64=sqlx::query_scalar("SELECT count(*) FROM pg_trigger WHERE tgname IN ('profiled_v2_proposal_immutable','profiled_v2_accept_immutable') AND NOT tgisinternal").fetch_one(&pool).await.unwrap();
    assert_eq!(triggers, 2);
    first.close().await;
    let second = Store::connect_with(db.options()).await.unwrap();
    second.close().await;
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert!(matches!(
        migrate_profile(&pool, 29).await,
        Err(aq_server::store::StoreError::IncompatibleState {
            found,
            supported: 29
        }) if found == aq_server::STATE_FORMAT_VERSION
    ));
}
#[tokio::test]
async fn real29_collision_rolls_back_all30_and_explicit_repair_allows_exact_upgrade() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 29).await.unwrap();
    sqlx::query("CREATE TABLE profiled_v2_accepts(fixture INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(format(&pool).await, "29");
    assert!(!present(&pool, "profiled_v2_proposals").await);
    let records: (i64, i32) = sqlx::query_as("SELECT count(*),max(version) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(records, (29, 29));
    sqlx::query("DROP TABLE profiled_v2_accepts")
        .execute(&pool)
        .await
        .unwrap();
    let migrated = Store::connect_with(db.options()).await.unwrap();
    migrated.close().await;
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert!(present(&pool, "profiled_v2_proposals").await);
}
#[tokio::test]
async fn failure_after_sql_metadata_transition_rolls_back_tables_and_meta_before_commit() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 29).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION test_refuse_migration30() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.version=30 THEN RAISE EXCEPTION 'synthetic failure after SQL metadata transition'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_refuse30 BEFORE INSERT ON schema_migrations FOR EACH ROW EXECUTE FUNCTION test_refuse_migration30();").execute(&pool).await.unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(format(&pool).await, "29");
    assert!(!present(&pool, "profiled_v2_proposals").await);
    assert!(!present(&pool, "profiled_v2_accepts").await);
    sqlx::raw_sql(
        "DROP TRIGGER test_refuse30 ON schema_migrations; DROP FUNCTION test_refuse_migration30();",
    )
    .execute(&pool)
    .await
    .unwrap();
    let current = Store::connect_with(db.options()).await.unwrap();
    current.close().await;
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
}
#[tokio::test]
async fn current30_missing_one_or_both_tables_refuses_reopen_without_repair() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    store.close().await;
    // Remove newer foreign-key dependencies as part of deliberate corruption.
    // Reopening must refuse the damage without silently repairing it.
    sqlx::query("DROP TABLE profiled_v2_accepts CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        Store::connect_with(db.options()).await,
        Err(aq_server::store::StoreError::Corrupt(_))
    ));
    assert!(!present(&pool, "profiled_v2_accepts").await);
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    sqlx::query("DROP TABLE profiled_v2_proposals CASCADE")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        Store::connect_with(db.options()).await,
        Err(aq_server::store::StoreError::Corrupt(_))
    ));
    assert!(!present(&pool, "profiled_v2_proposals").await);
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
}
