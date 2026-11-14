//! The Host9 migration (playbook authoring B2, B5): from the format before
//! it, a namespace row of catalog profile 9 is admitted and
//! `route_needed_emissions` records why a task is not asked about again.
use aq_server::test_support::{TestDb, migrate_profile};
use uuid::Uuid;

const MIGRATION: &str = include_str!("../../migrations/0129_host9_routing.sql");

#[tokio::test]
async fn the_host9_migration_admits_profile_9_and_records_route_outcomes() {
    let number: u32 = MIGRATION
        .rsplit_once("SET value='")
        .and_then(|(_, tail)| tail.split('\'').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, number - 1).await.unwrap();
    let owner = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'b2@localhost','owner',now())",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    let namespace = |name: &'static str| {
        sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest,catalog_profile) VALUES($1,1,$2,$1,$1,$3,$4,$4,9)")
            .bind(Uuid::new_v4())
            .bind(name)
            .bind(owner)
            .bind("a".repeat(64))
    };
    assert!(namespace("before").execute(&pool).await.is_err());
    sqlx::query(
        "INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms) VALUES($1,1,1)",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    namespace("after").execute(&pool).await.unwrap();
    let kept: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT outcome,artifact_digest FROM route_needed_emissions WHERE task_id=$1",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, (None, None));
    let set = |outcome: &'static str| {
        sqlx::query("UPDATE route_needed_emissions SET outcome=$1,reasons='[]',artifact_digest='d',options_digest='o'")
            .bind(outcome)
    };
    set("not_offered").execute(&pool).await.unwrap();
    set("unroutable").execute(&pool).await.unwrap();
    assert!(set("routed").execute(&pool).await.is_err());
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(format, number.to_string());
}
