//! Migration 0122 adds the playbook authoring tables (spec 0013 §4.1, §4.5) to a database at
//! 0121, and 0123 (internal agent runs) still applies after it.
use aq_server::test_support::{TestDb, migrate_profile};
use sqlx::PgPool;

async fn table(pool: &PgPool, name: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.tables
         WHERE table_schema=current_schema() AND table_name=$1)",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn migrations_0122_and_0123_apply_in_order_from_0121() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 121).await.unwrap();
    for name in [
        "playbook_sources",
        "playbook_authoring_runs",
        "playbook_source_grants",
        "internal_task_transcripts",
    ] {
        assert!(!table(&pool, name).await, "{name} before 0122");
    }
    let _store = db.store().await;
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(version.parse::<u32>().unwrap() >= 123, "{version}");
    let applied: Vec<i32> = sqlx::query_scalar(
        "SELECT version FROM schema_migrations WHERE version BETWEEN 121 AND 123 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(applied, [121, 122, 123]);
    for name in [
        "playbook_sources",
        "playbook_authoring_runs",
        "playbook_source_grants",
        "internal_task_transcripts",
    ] {
        assert!(table(&pool, name).await, "{name} after 0123");
    }

    // One active saved run per source; an active run must be saved.
    sqlx::query(
        "INSERT INTO playbook_sources(id,kind,binding,scope,purpose,judgment_purpose,commands,
            events,capabilities,facts,revision,updated_at_ms)
         VALUES('routing','builtin','aq.routing','installation','Route',
            'routing_classification','[]','[]','[]','[]',1,1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let run = |active: bool, status: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO playbook_authoring_runs(source_id,source_revision,source_digest,
                    facts_digest,record_digest,status,active,artifact_digest,artifact,
                    created_at_ms) VALUES('routing',1,'s','f','r',$1,$2,'a','{}',1)",
            )
            .bind(status)
            .bind(active)
            .execute(&pool)
            .await
        }
    };
    run(true, "saved").await.unwrap();
    assert!(run(true, "saved").await.is_err(), "a second active run");
    assert!(
        run(true, "running").await.is_err(),
        "an active run not saved"
    );
    run(false, "saved").await.unwrap();
}
