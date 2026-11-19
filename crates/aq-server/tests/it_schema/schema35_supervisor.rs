//! Ordered upgrade and destructive-drift qualification for the sole conversation identity.
use aq_server::store::StoreError;
use aq_server::test_support::{TestDb, migrate_profile};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
// The exact state35 reader remains qualified after later state versions exist.
async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
    migrate_profile(pool, 35).await
}

async fn database(version: u32) -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, version).await.unwrap();
    (db, pool)
}
async fn rows(pool: &PgPool, table: &str) -> Value {
    sqlx::query_scalar(&format!("SELECT COALESCE(jsonb_agg(row ORDER BY row::text),'[]'::jsonb) FROM (SELECT to_jsonb(t) AS row FROM {table} t) s")).fetch_one(pool).await.unwrap()
}
#[tokio::test]
async fn fresh35_and_actual34_upgrade_preserve_mail_and_refuse_old_reader() {
    let (_db, pool) = database(34).await;
    let message = Uuid::new_v4();
    let operation = Uuid::new_v4();
    sqlx::query("INSERT INTO mailbox_heads(mailbox_key,next_cursor) VALUES('retained-legacy',2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mailbox_messages(message_id,mailbox_key,cursor,thread_id,sender,recipient,payload_digest,body,created_at_ms) VALUES($1,'retained-legacy',1,$2,'{\"kind\":\"owner\"}','{\"kind\":\"task\"}','retained-digest','Exact legacy body ☃',1000)").bind(message).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) VALUES('retained-sender',$1,'retained-digest',$2)").bind(operation).bind(json!({"disposition":"queued","message_id":message})).execute(&pool).await.unwrap();
    let tables = ["mailbox_heads", "mailbox_messages", "mailbox_operations"];
    let mut before = vec![];
    for table in tables {
        before.push(rows(&pool, table).await);
    }
    migrate(&pool).await.unwrap();
    for (table, old) in tables.into_iter().zip(before) {
        assert_eq!(rows(&pool, table).await, old, "upgrade changed {table}");
    }
    let singleton = rows(&pool, "global_supervisor_conversation").await;
    assert_eq!(singleton.as_array().unwrap().len(), 1);
    assert_eq!(singleton[0]["revision"], 1);
    assert_ne!(singleton[0]["conversation_id"], json!(Uuid::nil()));
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert_eq!(ledger.as_array().unwrap().len(), 35);
    assert!(matches!(
        migrate_profile(&pool, 34).await,
        Err(StoreError::IncompatibleState {
            found: 35,
            supported: 34
        })
    ));
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
    assert_eq!(
        rows(&pool, "global_supervisor_conversation").await,
        singleton
    );
    migrate(&pool).await.unwrap();
    assert_eq!(
        rows(&pool, "global_supervisor_conversation").await,
        singleton
    );
    let (_fresh, fresh) = database(35).await;
    assert_eq!(
        rows(&fresh, "global_supervisor_conversation")
            .await
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn identity_and_retention_are_durable_and_cannot_be_reset() {
    let (_db, pool) = database(35).await;
    for sql in [
        "UPDATE global_supervisor_conversation SET conversation_id=gen_random_uuid()",
        "UPDATE global_supervisor_conversation SET revision=2",
        "DELETE FROM global_supervisor_conversation",
        "TRUNCATE global_supervisor_conversation",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
    sqlx::query(
        "UPDATE global_supervisor_conversation SET retained_operations=1,retained_bytes=4096",
    )
    .execute(&pool)
    .await
    .unwrap();
    for sql in [
        "UPDATE global_supervisor_conversation SET retained_operations=0",
        "UPDATE global_supervisor_conversation SET retained_bytes=0",
        "UPDATE global_supervisor_conversation SET retained_operations=4097",
        "UPDATE global_supervisor_conversation SET retained_bytes=67108865",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
}
async fn corrupt(sql: &str) {
    let (_db, pool) = database(35).await;
    sqlx::query(sql).execute(&pool).await.unwrap();
    let ledger = rows(&pool, "schema_migrations").await;
    let meta = rows(&pool, "meta").await;
    assert!(
        matches!(migrate(&pool).await,Err(StoreError::Corrupt(message))if message.contains("global supervisor conversation")),
        "{sql}"
    );
    assert_eq!(rows(&pool, "schema_migrations").await, ledger);
    assert_eq!(rows(&pool, "meta").await, meta);
}
#[tokio::test]
async fn missing_or_disabled_guards_and_missing_singleton_are_not_silently_repaired() {
    for guard in ["identity", "delete", "truncate"] {
        corrupt(&format!("DROP TRIGGER global_supervisor_conversation_{guard}_guard ON global_supervisor_conversation")).await;
        corrupt(&format!("ALTER TABLE global_supervisor_conversation DISABLE TRIGGER global_supervisor_conversation_{guard}_guard")).await;
    }
    corrupt("DROP FUNCTION guard_global_supervisor_conversation() CASCADE").await;
    corrupt("DROP TABLE global_supervisor_conversation").await;
    let (_db, pool) = database(35).await;
    sqlx::query("ALTER TABLE global_supervisor_conversation DISABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM global_supervisor_conversation")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE global_supervisor_conversation ENABLE TRIGGER USER")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        matches!(migrate(&pool).await,Err(StoreError::Corrupt(message))if message.contains("singleton"))
    );
}
