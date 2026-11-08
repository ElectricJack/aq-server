//! Ordered real SQL profiles. No metadata-only downgrade or runtime activation.
use aq_server::{
    store::{Store, StoreError},
    test_support::{TestDb, migrate_profile},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;
async fn old25() -> (TestDb, PgPool) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 25).await.unwrap();
    assert_eq!(format(&pool).await, "25");
    (db, pool)
}
async fn format(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn seed(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'schema-fixture@example.invalid','owner','fixture','schema-upgrade',now())").bind(id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'schema-fixture-node',$2,'0.3.3',2,now())").bind(id).bind("a".repeat(64)).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO execution_receipts(node_id,operation_id,kind,payload_digest,response) VALUES($1,$1,'result',$2,$3)").bind(id).bind("b".repeat(64)).bind(json!({"synthetic":"retained old result","retention_release":false})).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,'schema-fixture-namespace',$1,$1,$1,$2,$3)").bind(id).bind("c".repeat(64)).bind("d".repeat(64)).execute(pool).await.unwrap();
    id
}
async fn snapshot(pool: &PgPool, id: Uuid) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('user',(SELECT to_jsonb(u) FROM users u WHERE id=$1),'node',(SELECT to_jsonb(n) - 'machine_key' - 'slot' FROM nodes n WHERE id=$1),'receipt',(SELECT to_jsonb(r) FROM execution_receipts r WHERE node_id=$1),'namespace',(SELECT to_jsonb(p)-'catalog_profile' FROM playbook_namespaces p WHERE namespace_id=$1))").bind(id).fetch_one(pool).await.unwrap()
}
async fn migrations(pool: &PgPool) -> Vec<(i32, DateTime<Utc>)> {
    sqlx::query_as("SELECT version,applied_at FROM schema_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .unwrap()
}
async fn installed(pool: &PgPool, table: &str) -> bool {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("public.{table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}
#[tokio::test]
async fn fresh_database_installs_ordered_current_without_activating_optional_owners() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(
        migrations(&pool)
            .await
            .iter()
            .map(|r| r.0)
            .collect::<Vec<_>>(),
        (1..=aq_server::STATE_FORMAT_VERSION as i32).collect::<Vec<_>>()
    );
    for table in [
        "review_owner_budget",
        "review_lineage_budget",
        "review_subjects",
        "human_decision_requests",
        "human_decision_finality",
        "review_owner_outbox",
    ] {
        assert!(installed(&pool, table).await, "{table}");
    }
    let row = sqlx::query(
        "SELECT retained_operations,retained_bytes FROM review_owner_budget WHERE singleton",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>(0), 0);
    assert_eq!(row.get::<i64, _>(1), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM review_subjects")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let constraints:Vec<String>=sqlx::query_scalar("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid='admission_artifacts'::regclass").fetch_all(&pool).await.unwrap();
    assert!(constraints.iter().any(|s| s.contains("'startup'::text")));
    let config = toml::from_str(
        r#"
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="nobody@example.invalid"
[bootstrap]
enabled=false
"#,
    )
    .unwrap();
    let state = aq_server::AppState {
        agent_access: None,
        source_retention: None,
        identity: std::sync::Arc::new(aq_server::identity::DevIdentity::new(
            "nobody@example.invalid".into(),
        )),
        config: std::sync::Arc::new(config),
        store,
        clock: aq_server::system_clock(),
    };
    use tower::ServiceExt;
    for path in [
        "/api/playbooks/status",
        "/api/reviews/decisions/00000000-0000-0000-0000-000000000001",
    ] {
        let response = aq_server::build_admin_router(state.clone())
            .oneshot(
                axum::http::Request::get(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}
#[tokio::test]
async fn populated25_upgrades_to_current_preserving_rows_and_reopening_idempotently() {
    let (db, pool) = old25().await;
    let id = seed(&pool).await;
    let before = snapshot(&pool, id).await;
    let prior = migrations(&pool).await;
    assert!(!installed(&pool, "review_owner_budget").await);
    for _ in 0..2 {
        drop(Store::connect_with(db.options()).await.unwrap());
        assert_eq!(
            format(&pool).await,
            aq_server::STATE_FORMAT_VERSION.to_string()
        );
        assert_eq!(snapshot(&pool, id).await, before);
        let current = migrations(&pool).await;
        assert_eq!(&current[..25], prior.as_slice());
        assert_eq!(current.len(), aq_server::STATE_FORMAT_VERSION as usize);
        assert_eq!(
            sqlx::query_scalar::<_, i16>(
                "SELECT catalog_profile FROM playbook_namespaces WHERE namespace_id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        assert!(sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='admission_definition_identity')").fetch_one(&pool).await.unwrap());
    }
}
#[tokio::test]
async fn failure_in27_rolls_back26_and_all_metadata_then_explicit_retry_succeeds() {
    let (db, pool) = old25().await;
    let id = seed(&pool).await;
    let before = snapshot(&pool, id).await;
    let prior = migrations(&pool).await;
    // Deliberate local catalog collision, not a migration body changed to fail.
    sqlx::query("CREATE INDEX admission_definition_identity ON admission_artifacts(artifact_id)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(format(&pool).await, "25");
    assert_eq!(migrations(&pool).await, prior);
    assert_eq!(snapshot(&pool, id).await, before);
    assert!(!installed(&pool, "review_owner_budget").await);
    assert!(!sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='playbook_namespaces' AND column_name='catalog_profile')").fetch_one(&pool).await.unwrap());
    sqlx::query("DROP INDEX admission_definition_identity")
        .execute(&pool)
        .await
        .unwrap();
    drop(Store::connect_with(db.options()).await.unwrap());
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(snapshot(&pool, id).await, before);
}
#[tokio::test]
async fn old25_reader_profile_refuses_current_without_touching_rows_or_migration_metadata() {
    let db = TestDb::create().await;
    drop(db.store().await);
    let pool = db.pool().await;
    let id = seed(&pool).await;
    let before = snapshot(&pool, id).await;
    let prior = migrations(&pool).await;
    assert!(matches!(
        migrate_profile(&pool, 25).await,
        Err(StoreError::IncompatibleState {
            found: aq_server::STATE_FORMAT_VERSION,
            supported: 25
        })
    ));
    assert_eq!(
        format(&pool).await,
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(migrations(&pool).await, prior);
    assert_eq!(snapshot(&pool, id).await, before);
}
#[test]
fn format_change_requires_backup_restore_before_rollback() {
    let mut target=aq_server::updates::build_manifest("ghcr.io/fixture/server@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","ghcr.io/fixture/worker@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",env!("CARGO_PKG_VERSION"),"synthetic-schema-check").unwrap();
    assert_eq!(target.state_format, aq_server::STATE_FORMAT_VERSION);
    assert!(target.readable_state_formats.contains(&25));
    assert!(target.readable_state_formats.contains(&26));
    let mut old = target.clone();
    old.state_format = 25;
    old.readable_state_formats = (1..=25).collect();
    // Fixture versions test compatibility policy only; no release is published.
    target.version = "0.2.0".parse().unwrap();
    assert!(
        aq_release::check_upgrade(Some(&old), Some(25), &target)
            .unwrap()
            .needs_restore_on_rollback
    );
    assert!(
        !aq_release::check_upgrade(None, Some(aq_server::STATE_FORMAT_VERSION), &target)
            .unwrap()
            .needs_restore_on_rollback
    );
}

#[tokio::test]
async fn inconsistent_recorded_format_refuses_without_repair_or_partial_changes() {
    let db = TestDb::create().await;
    drop(db.store().await);
    let pool = db.pool().await;
    let id = seed(&pool).await;
    let before = snapshot(&pool, id).await;
    let prior = migrations(&pool).await;
    sqlx::query("UPDATE meta SET value='25' WHERE key='state_format_version'")
        .execute(&pool)
        .await
        .unwrap();
    let error = Store::connect_with(db.options()).await.err().unwrap();
    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
    assert_eq!(format(&pool).await, "25");
    assert_eq!(migrations(&pool).await, prior);
    assert_eq!(snapshot(&pool, id).await, before);
    sqlx::query("DELETE FROM meta WHERE key='state_format_version'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        Store::connect_with(db.options()).await.err().unwrap(),
        StoreError::Corrupt(_)
    ));
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key='state_format_version')"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert_eq!(migrations(&pool).await, prior);
    assert_eq!(snapshot(&pool, id).await, before);
}
