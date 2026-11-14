//! Migration 0127 (architecture 0006): the worker heartbeat log
//! (`profiled_presence_operations`) becomes a rolling record. Its trigger
//! still refuses UPDATE and an ordinary DELETE; only row retention
//! (`aq.row_retention`) may delete. The behaviour on real rows is tested in
//! `it_native` (`scoped_presence_inventory`).
use aq_server::test_support::{TestDb, migrate_profile};

const MIGRATION: &str = include_str!("../../migrations/0127_presence_rolling_records.sql");

#[tokio::test]
async fn migration_0127_lets_row_retention_delete_the_heartbeat_log() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 126).await.unwrap();
    let body = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT prosrc FROM pg_proc WHERE proname='refuse_profiled_presence_history_change'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert!(!body().await.contains("aq.row_retention"));
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let after = body().await;
    assert!(after.contains("TG_OP='DELETE' AND current_setting('aq.row_retention',true)='on'"));
    assert!(after.contains("RAISE EXCEPTION 'immutable scoped presence operation'"));
    // The same trigger, still on UPDATE and DELETE (tgtype 27: row, before, both).
    let trigger: (String, i16) = sqlx::query_as(
        "SELECT p.proname::text,t.tgtype FROM pg_trigger t JOIN pg_proc p ON p.oid=t.tgfoid WHERE t.tgname='profiled_presence_operation_immutable'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        trigger,
        ("refuse_profiled_presence_history_change".into(), 27)
    );
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, "127");
    assert_eq!(aq_server::STATE_FORMAT_VERSION, 127);
}
