//! Ordered state31 upgrade/rollback and deterministic semantic projection.
use aq_server::{
    store::Store,
    test_support::{TestDb, migrate_profile},
};
use serde_json::json;
#[tokio::test]
async fn actual30_collision_rolls_back31_and_retry_upgrades() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 30).await.unwrap();
    sqlx::query("CREATE FUNCTION profiled_provisionable_binding_set(JSONB) RETURNS JSONB LANGUAGE SQL AS 'SELECT $1'")
 .execute(&pool).await.unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, "30");
    let column:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_inventory_semantic_heads' AND column_name='provisionable_binding_set')").fetch_one(&pool).await.unwrap();
    assert!(!column);
    sqlx::query("DROP FUNCTION profiled_provisionable_binding_set(JSONB)")
        .execute(&pool)
        .await
        .unwrap();
    drop(Store::connect_with(db.options()).await.unwrap());
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
}
#[tokio::test]
async fn semantic_projection_ignores_order_and_observation_clocks_but_not_bindings() {
    let db = TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let a = json!({"credential_profile":{"id":"a"},"credential_revision":1,"available":false,"observed_at_ms":1,"expires_at_ms":2,"preparation":{"owner":"owner","observed_at_ms":1,"expires_at_ms":2}});
    let b = json!({"credential_profile":{"id":"b"},"credential_revision":2,"available":false,"observed_at_ms":1,"expires_at_ms":2});
    let mut changed = a.clone();
    changed["observed_at_ms"] = json!(10);
    changed["expires_at_ms"] = json!(20);
    changed["preparation"]["observed_at_ms"] = json!(10);
    changed["preparation"]["expires_at_ms"] = json!(20);
    let same: bool = sqlx::query_scalar(
        "SELECT profiled_provisionable_binding_set($1)=profiled_provisionable_binding_set($2)",
    )
    .bind(json!({"credential_bindings":[a,b]}))
    .bind(json!({"credential_bindings":[b,changed]}))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(same);
    let removed: bool = sqlx::query_scalar(
        "SELECT profiled_provisionable_binding_set($1)<>profiled_provisionable_binding_set($2)",
    )
    .bind(json!({"credential_bindings":[a,b]}))
    .bind(json!({"credential_bindings":[a]}))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(removed);
}

#[tokio::test]
async fn current31_missing_binding_column_or_function_refuses_reopen_without_repair() {
    for missing in ["column", "function"] {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        store.close().await;
        let ddl = if missing == "column" {
            "ALTER TABLE profiled_inventory_semantic_heads DROP COLUMN provisionable_binding_set"
        } else {
            "DROP FUNCTION profiled_provisionable_binding_set(JSONB)"
        };
        sqlx::query(ddl).execute(&pool).await.unwrap();
        assert!(matches!(
            Store::connect_with(db.options()).await,
            Err(aq_server::store::StoreError::Corrupt(_))
        ));
        let version: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
        let restored: bool = if missing == "column" {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_inventory_semantic_heads' AND column_name='provisionable_binding_set')")
                .fetch_one(&pool).await.unwrap()
        } else {
            sqlx::query_scalar(
                "SELECT to_regprocedure('profiled_provisionable_binding_set(jsonb)') IS NOT NULL",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert!(!restored);
    }
}
