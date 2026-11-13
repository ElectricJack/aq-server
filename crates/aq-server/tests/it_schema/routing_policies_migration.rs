//! Migration 0121 retires the routing decision table on a database at 0120 that has compiles
//! (spec 0013 §8): `routing_policies` and `routing_state.active_policy_digest` go, a switch left
//! on is turned off, and task routes and route-needed emissions are kept.
use aq_server::test_support::{TestDb, migrate_profile};
use sqlx::PgPool;
use uuid::Uuid;

async fn exists(pool: &PgPool, table: &str, column: Option<&str>) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns
         WHERE table_schema=current_schema() AND table_name=$1 AND ($2::TEXT IS NULL OR column_name=$2))",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Seeds a 0120 database as the live one is (failed compiles, routing off, no active policy),
/// or, with `on`, with an active policy and routing on.
async fn at_0120(on: bool) -> (TestDb, PgPool, Uuid) {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 120).await.unwrap();
    for revision in [1_i64, 2] {
        sqlx::query(
            "INSERT INTO routing_policies(structure_revision,source_digest,status,error,
                created_at_ms,finished_at_ms) VALUES($1,'sha256:s','failed','No level',1,2)",
        )
        .bind(revision)
        .execute(&pool)
        .await
        .unwrap();
    }
    if on {
        sqlx::query(
            "INSERT INTO routing_policies(structure_revision,source_digest,status,digest,compiled,
                created_at_ms,finished_at_ms) VALUES(3,'sha256:s','active','d1','{}',1,2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE routing_state SET enabled=TRUE,active_policy_digest='d1'")
            .execute(&pool)
            .await
            .unwrap();
    }
    let task = Uuid::new_v4();
    sqlx::query("INSERT INTO task_routes(task_id,sequence,route,at_ms) VALUES($1,1,NULL,5)")
        .bind(task)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO route_needed_emissions(task_id,task_version,emitted_at_ms) VALUES($1,4,6)",
    )
    .bind(task)
    .execute(&pool)
    .await
    .unwrap();
    (db, pool, task)
}

#[tokio::test]
async fn migration_0121_drops_the_compiles_and_keeps_routes_and_emissions() {
    for on in [false, true] {
        let (db, pool, task) = at_0120(on).await;
        assert!(exists(&pool, "routing_policies", None).await);
        let _store = db.store().await;
        let version: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(version.parse::<u32>().unwrap() >= 121, "{version}");
        assert!(!exists(&pool, "routing_policies", None).await);
        assert!(!exists(&pool, "routing_state", Some("active_policy_digest")).await);
        let enabled: Vec<bool> = sqlx::query_scalar("SELECT enabled FROM routing_state")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(enabled, [false], "on before: {on}");
        let routes: i64 = sqlx::query_scalar("SELECT count(*) FROM task_routes WHERE task_id=$1")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
        let emitted: i64 =
            sqlx::query_scalar("SELECT task_version FROM route_needed_emissions WHERE task_id=$1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((routes, emitted), (1, 4));
    }
}
