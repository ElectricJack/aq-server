//! Actual isolated PostgreSQL upgrade and cancellation proof guard checks.
use aq_server::{
    store::{Store, StoreError},
    test_support::{self, TestDb},
};
use serde_json::{Value, json};
#[tokio::test]
async fn cancellation43_upgrade_preserves_prior_records_and_old41_refuses() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    test_support::migrate_profile(&pool, 41).await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t")
            .fetch_one(&pool)
            .await
            .unwrap();
    let _store = db.store().await;
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM global_supervisor_conversation t")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert!(matches!(
        test_support::migrate_profile(&pool, 41).await,
        Err(StoreError::IncompatibleState {
            found,
            supported: 41
        }) if found == aq_server::STATE_FORMAT_VERSION
    ));
    Store::connect_with(db.options()).await.unwrap();
}
#[tokio::test]
async fn cancellation43_upgrade_preserves_dashboard42_operation_and_old42_refuses() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    test_support::migrate_profile(&pool, 42).await.unwrap();
    let principal = uuid::Uuid::new_v4();
    let operation = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'upgrade@localhost','owner',now())",
    )
    .bind(principal)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO dashboard_operations(operation_id,principal_id,kind,scope,request_digest,disposition,response,created_at,updated_at) VALUES($1,$2,'test.upgrade','{}',$3,'pending',jsonb_build_object('operation_id',$1::text,'request_digest',$3::text,'disposition','pending','scope','{}'::jsonb),now(),now())")
        .bind(operation).bind(principal).bind(format!("sha256:{}", "0".repeat(64))).execute(&pool).await.unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(t) FROM dashboard_operations t WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    let _store = db.store().await;
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(t) FROM dashboard_operations t WHERE operation_id=$1")
            .bind(operation)
            .fetch_one(&pool)
            .await
            .unwrap();
    // Later migrations may add columns (0061 adds owner_id/supervisor_session_id);
    // every schema-42 column must survive the upgrade unchanged.
    let after_42: serde_json::Map<String, Value> = before
        .as_object()
        .unwrap()
        .keys()
        .map(|key| (key.clone(), after[key].clone()))
        .collect();
    assert_eq!(before, Value::Object(after_42));
    assert_eq!(after["owner_id"], json!(principal.to_string()));
    assert_eq!(after["supervisor_session_id"], Value::Null);
    assert!(matches!(
        test_support::migrate_profile(&pool, 42).await,
        Err(StoreError::IncompatibleState {
            found,
            supported: 42
        }) if found == aq_server::STATE_FORMAT_VERSION
    ));
    Store::connect_with(db.options()).await.unwrap();
}
#[tokio::test]
async fn cancellation43_startup_refuses_missing_or_changed_proof_owners() {
    for mutation in [
        "ALTER TABLE profiled_cancellations DISABLE TRIGGER profiled_cancellations_immutable",
        "ALTER TABLE profiled_cancelled_settlements DISABLE TRIGGER profiled_cancelled_settlement_guard",
        "ALTER TABLE profiled_never_granted_closures DISABLE TRIGGER profiled_cancelled_absence_guard",
        "CREATE OR REPLACE FUNCTION profiled_cancelled_settlement_guard() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "CREATE OR REPLACE FUNCTION profiled_close_expired_ungranted(p_node UUID,p_attempt UUID,p_now BIGINT,p_closure UUID,p_operation UUID) RETURNS TABLE(body_bytes BYTEA,body_digest TEXT,changed BOOLEAN) LANGUAGE SQL AS 'SELECT NULL::BYTEA,NULL::TEXT,FALSE'",
    ] {
        let db = TestDb::create().await;
        let _store = db.store().await;
        let pool = db.pool().await;
        sqlx::raw_sql(mutation).execute(&pool).await.unwrap();
        assert!(
            Store::connect_with(db.options()).await.is_err(),
            "accepted drift: {mutation}"
        );
    }
}
