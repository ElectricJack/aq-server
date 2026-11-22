//! Migration 0164 (spec 0017 §6.3, §6.7): environment versions and builder
//! jobs move into the database, and `environment_limits` is a Settings value.
use aq_server::store::Store;
use aq_server::test_support::TestDb;
use uuid::Uuid;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

async fn version(pool: &sqlx::PgPool, id: &str, project: Option<Uuid>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO environment_versions(environment_id,version,project_id,template_digest,image,provides,recipe_sha256,base_digest) \
         VALUES($1,'1',$2,$3,'registry.example/aq@'||$3,'[\"rust\"]',$4,$3)",
    )
    .bind(id)
    .bind(project)
    .bind(DIGEST)
    .bind("b".repeat(64))
    .execute(pool)
    .await
    .map(drop)
}

#[tokio::test]
async fn environment_versions_migration_applies_and_is_idempotent() {
    let db = TestDb::create().await;
    drop(db.store().await);
    let pool = db.pool().await;
    let project = Uuid::new_v4();
    version(&pool, "example.web", Some(project)).await.unwrap();
    // A base environment belongs to no project.
    version(&pool, "aq-base", None).await.unwrap();
    // One row per environment version.
    assert!(version(&pool, "example.web", Some(project)).await.is_err());
    sqlx::query(
        "INSERT INTO environment_jobs(job_id,kind,project_id,commit,recipes,state,base_template,target_repository) \
         VALUES($1,'build',$2,$3,'[\"web\"]','queued','registry.example/aq@'||$4,'registry.example/aq')",
    )
    .bind(Uuid::new_v4())
    .bind(project)
    .bind("c".repeat(40))
    .bind(DIGEST)
    .execute(&pool)
    .await
    .unwrap();
    let limits: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT environment_limits FROM role_settings WHERE singleton")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(limits, None, "never saved: the defaults apply");

    // Connecting again applies nothing twice and keeps the rows.
    drop(Store::connect_with(db.options()).await.unwrap());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM schema_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, i64::from(aq_server::STATE_FORMAT_VERSION));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM environment_versions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
}
