//! Migration 0126 (playbook authoring B1b, architecture 0006) drops the
//! playbook bridge's lifetime counters and their CHECKs: from 0125, a
//! namespace whose counters stand at their caps keeps its rows, and the
//! drive's rolling deletion has its indexes.
use aq_server::test_support::{TestDb, migrate_profile};
use uuid::Uuid;

const MIGRATION: &str = include_str!("../../migrations/0126_playbook_rolling_records.sql");
const COUNTERS: [&str; 6] = [
    "grant_count",
    "admission_count",
    "effect_count",
    "cancellation_count",
    "outbox_count",
    "retained_bytes",
];

#[tokio::test]
async fn migration_0126_drops_the_playbook_lifetime_counters() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 125).await.unwrap();
    let columns = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT column_name::text FROM information_schema.columns WHERE table_name='playbook_namespaces' AND column_name=ANY($1) ORDER BY 1",
        )
        .bind(COUNTERS)
        .fetch_all(&pool)
        .await
        .unwrap()
    };
    assert_eq!(columns().await.len(), 6);
    let (owner, namespace) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'b1b@localhost','owner',now())",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    // Every counter at its cap: before 0126 nothing more could be admitted.
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest,grant_count,admission_count,effect_count,cancellation_count,outbox_count,retained_bytes) VALUES($1,1,'b1b',$2,$2,$3,$4,$4,1024,4096,8192,8192,16384,67108864)")
        .bind(namespace)
        .bind(Uuid::new_v4())
        .bind(owner)
        .bind("a".repeat(64))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE playbook_namespaces SET admission_count=4097")
            .execute(&pool)
            .await
            .is_err(),
        "0023's CHECK holds the cap"
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(columns().await.is_empty());
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM playbook_namespaces WHERE namespace_id=$1")
            .bind(namespace)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kept, 1);
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes WHERE indexname IN ('playbook_effects_age','playbook_cancellations_age','playbook_host_operations_completed','playbook_health_observations_expiry','playbook_finding_notifications_submitted') ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(indexes.len(), 5, "{indexes:?}");
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, "126");
    assert_eq!(aq_server::STATE_FORMAT_VERSION, 126);
}
