//! Registered migration and current-schema validation; no metadata relabeling.
use super::*;
use aq_server::{
    store::{Store, StoreError},
    test_support::TestDb,
};
#[tokio::test]
async fn actual32_open_owner_history_survives_registered33_upgrade() {
    let f = aq_server::test_support::with_state32_store(Box::pin(prepared(true))).await;
    issue(&f).await;
    let pool = &f.c.env.env.pool;
    let before = history(pool).await;
    let mut old = pool.acquire().await.unwrap();
    let backend: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    let upgraded = Store::connect_with(f.c.env.env._db.options())
        .await
        .unwrap();
    assert_eq!(history(pool).await, before);
    let (version,applied,reserved):(String,i64,i64)=sqlx::query_as("SELECT (SELECT value FROM meta WHERE key='state_format_version'),(SELECT count(*) FROM schema_migrations WHERE version=33),(SELECT reserved_bytes FROM profiled_phase_terminal_capacity)").fetch_one(&mut *old).await.unwrap();
    assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
    assert_eq!((applied, reserved), (1, 1048576));
    let after: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *old)
        .await
        .unwrap();
    assert_eq!(backend, after);
    upgraded.close().await;
}
#[tokio::test]
async fn current33_missing_mandatory_objects_refuses_reopen_without_repair() {
    for (remove, absent) in [
        (
            "DROP FUNCTION profiled_phase_unqualified_result_guard() CASCADE",
            "SELECT to_regprocedure('profiled_phase_unqualified_result_guard()') IS NULL",
        ),
        (
            "ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_phase_unqualified_result_guard",
            "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_unqualified_result_guard' AND tgrelid='profiled_result_reports'::regclass AND tgenabled='D')",
        ),
        (
            // Migration39 replaces the unqualified guard with the stricter
            // settlement owner. Corrupt the actual current mandatory guard.
            "ALTER TABLE profiled_verified_releases DISABLE TRIGGER profiled_phase_qualified_release_guard",
            "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_qualified_release_guard' AND tgrelid='profiled_verified_releases'::regclass AND tgenabled='D')",
        ),
        (
            "ALTER TABLE profiled_started_receipts DISABLE TRIGGER profiled_phase_terminal_origin_guard",
            "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_terminal_origin_guard' AND tgrelid='profiled_started_receipts'::regclass AND tgenabled='D')",
        ),
        (
            "ALTER TABLE profiled_launch_closures DROP COLUMN legacy_origin_operation_id CASCADE",
            "SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_launch_closures' AND column_name='legacy_origin_operation_id')",
        ),
        (
            "ALTER TABLE profiled_launch_closures DROP COLUMN phase_origin_operation_id CASCADE",
            "SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_launch_closures' AND column_name='phase_origin_operation_id')",
        ),
        (
            "ALTER TABLE profiled_launch_closures DROP CONSTRAINT profiled_launch_closures_legacy_origin_fk",
            "SELECT NOT EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid='profiled_launch_closures'::regclass AND conname='profiled_launch_closures_legacy_origin_fk')",
        ),
        (
            "ALTER TABLE profiled_launch_closures DROP CONSTRAINT profiled_launch_closures_phase_origin_fk",
            "SELECT NOT EXISTS(SELECT 1 FROM pg_constraint WHERE conrelid='profiled_launch_closures'::regclass AND conname='profiled_launch_closures_phase_origin_fk')",
        ),
        (
            "DROP TABLE profiled_phase_terminal_capacity CASCADE",
            "SELECT to_regclass('profiled_phase_terminal_capacity') IS NULL",
        ),
        (
            "DROP FUNCTION profiled_phase_terminal_origin_guard() CASCADE",
            "SELECT to_regprocedure('profiled_phase_terminal_origin_guard()') IS NULL",
        ),
        (
            "DROP FUNCTION profiled_phase_terminal_capacity_guard() CASCADE",
            "SELECT to_regprocedure('profiled_phase_terminal_capacity_guard()') IS NULL",
        ),
        (
            "DROP TRIGGER profiled_phase_terminal_reserve ON profiled_phase_execution_grants",
            "SELECT NOT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_terminal_reserve' AND tgrelid='profiled_phase_execution_grants'::regclass)",
        ),
        (
            "ALTER TABLE profiled_phase_execution_grants DISABLE TRIGGER profiled_phase_terminal_reserve",
            "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_terminal_reserve' AND tgrelid='profiled_phase_execution_grants'::regclass AND tgenabled='D')",
        ),
        (
            "DROP TRIGGER profiled_phase_terminal_origin_guard ON profiled_stopped_receipts",
            "SELECT NOT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='profiled_phase_terminal_origin_guard' AND tgrelid='profiled_stopped_receipts'::regclass)",
        ),
        (
            "ALTER TABLE profiled_launch_closures DROP COLUMN phase_ack_digest",
            "SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_launch_closures' AND column_name='phase_ack_digest')",
        ),
        (
            "ALTER TABLE profiled_started_receipts DROP COLUMN phase_ack_bytes CASCADE",
            "SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='profiled_started_receipts' AND column_name='phase_ack_bytes')",
        ),
    ] {
        let db = TestDb::create().await;
        let store = db.store().await;
        store.close().await;
        let pool = db.pool().await;
        sqlx::query(remove).execute(&pool).await.unwrap();
        let result = Store::connect_with(db.options()).await;
        assert!(
            matches!(result, Err(StoreError::Corrupt(_))),
            "mandatory object loss must refuse: {remove}"
        );
        let remains_missing: bool = sqlx::query_scalar(absent).fetch_one(&pool).await.unwrap();
        assert!(
            remains_missing,
            "reopen must never silently repair: {remove}"
        );
        let (version,n):(String,i64)=sqlx::query_as("SELECT (SELECT value FROM meta WHERE key='state_format_version'),(SELECT count(*) FROM schema_migrations WHERE version=33)").fetch_one(&pool).await.unwrap();
        assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
        assert_eq!(n, 1);
    }
}
