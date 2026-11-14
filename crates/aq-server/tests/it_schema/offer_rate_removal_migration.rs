//! Migration 0124 removes the spec 0008 offer rate limits (owner, 2026-11-13): the 0105
//! fence on `profiled_launches` and the `offers` / `project_offers` limit rows. It applies
//! from 0123 with recent offers present, and without ownership of what 0105 created it
//! leaves a fence that counts nothing instead of failing.
use crate::common::sealed;
use aq_server::test_support::{TestDb, migrate_profile};
use sqlx::PgPool;
use uuid::Uuid;

const NOW: i64 = 1_800_000_000_000;
const MIGRATION: &str = include_str!("../../migrations/0124_remove_offer_rate.sql");

async fn rate_holds(pool: &PgPool, nodes: &[Uuid], projects: &[Uuid]) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT hold_kind FROM sealed_history_holds($1,$2,$3) WHERE hold_code='history_rate_limited'",
    )
    .bind(nodes)
    .bind(projects)
    .bind(NOW)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn limit_kinds(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT DISTINCT kind FROM sealed_history_limits() ORDER BY kind")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn fence(pool: &PgPool) -> (bool, bool) {
    sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM pg_trigger WHERE tgrelid='profiled_launches'::regclass
           AND tgname='profiled_launch_offer_rate' AND NOT tgisinternal),
         to_regprocedure('profiled_launch_offer_rate()') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn launches(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM profiled_launches")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_0124_drops_the_offer_rate_fence_with_recent_offers_present() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 123).await.unwrap();
    // 0105's fence and function, and the table it guards, belong to the role that ran
    // the migrations: the same role applies 0124.
    let owned: bool = sqlx::query_scalar(
        "SELECT (SELECT relowner FROM pg_class WHERE oid='profiled_launches'::regclass)=current_user::regrole
         AND (SELECT proowner FROM pg_proc WHERE oid='profiled_launch_offer_rate()'::regprocedure)=current_user::regrole",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(owned, "the migrating role owns what 0105 created");
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 30).await;
    // Before: the limit rows hold the node and the fence refuses the 31st offer.
    assert!(limit_kinds(&pool).await.contains(&"offers".to_string()));
    assert_eq!(rate_holds(&pool, &[n], &[p]).await, ["offers"]);
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "54000");
    assert_eq!(fence(&pool).await, (true, true));

    let _store = db.store().await;
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(version.parse::<u32>().unwrap() >= 124, "{version}");
    let applied: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version>=123 ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(&applied[..2], [123, 124]);

    assert_eq!(
        fence(&pool).await,
        (false, false),
        "fence and function dropped"
    );
    let kinds = limit_kinds(&pool).await;
    assert!(
        !kinds.iter().any(|k| k == "offers" || k == "project_offers"),
        "{kinds:?}"
    );
    assert!(kinds.contains(&"presence_rate".to_string()), "{kinds:?}");
    assert!(rate_holds(&pool, &[n], &[p]).await.is_empty());
    // The 31st offer passes the removed fence and fails only on the decision FK.
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "23503");
    assert_eq!(launches(&pool).await, 30, "existing offers are kept");
    assert_eq!(
        sealed::usage(&pool, "offers", Some(n), NOW, None).await,
        (0, 0, 0, 0)
    );
}

/// A migrating role that owns neither `profiled_launches` nor 0105's function cannot drop
/// the fence. 0124 still applies, and the fence it leaves counts no offers.
#[tokio::test]
async fn migration_0124_without_ownership_of_the_fence_leaves_it_inert() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 123).await.unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let (migrator, other) = (format!("aq_t_mig_{suffix}"), format!("aq_t_own_{suffix}"));
    let setup = format!(
        "CREATE ROLE {migrator} NOLOGIN; CREATE ROLE {other} NOLOGIN;
         GRANT CREATE ON SCHEMA public TO {migrator}; GRANT SELECT,UPDATE ON meta TO {migrator};
         ALTER FUNCTION sealed_history_limits() OWNER TO {migrator};
         ALTER FUNCTION sealed_history_scoped(text,bigint,uuid,uuid[]) OWNER TO {migrator};
         ALTER TABLE profiled_launches OWNER TO {other};
         ALTER FUNCTION profiled_launch_offer_rate() OWNER TO {other};"
    );
    sqlx::raw_sql(&setup).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(&format!("SET LOCAL ROLE {migrator}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(fence(&pool).await, (true, true), "left in place");
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 30).await;
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "23503");
    sealed::spread_offers(&pool, Some(p), 240, NOW - 1_000).await;
    sealed::spread_offers(&pool, None, 960, NOW - 1_000).await;
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "23503");
    assert!(rate_holds(&pool, &[n], &[p]).await.is_empty());

    let cleanup = format!(
        "REASSIGN OWNED BY {migrator},{other} TO current_user;
         DROP OWNED BY {migrator},{other}; DROP ROLE {migrator}; DROP ROLE {other};"
    );
    sqlx::raw_sql(&cleanup).execute(&pool).await.unwrap();
}
