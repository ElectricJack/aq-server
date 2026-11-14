//! Migration 0124 removes the spec 0008 rate limits (owner, 2026-11-13): the offer rate
//! (the `offers` / `project_offers` limit rows and the 0105 fence on `profiled_launches`)
//! and the worker report rate (the `presence_rate` rows and the rate branch of
//! `profiled_presence_operation_guard`). It applies from 0123 with recent offers and
//! reports present. Without ownership of what 0105 created it warns and leaves a fence
//! that counts nothing instead of failing.
use crate::common::sealed;
use aq_server::test_support::{TestDb, migrate_profile};
use sqlx::PgPool;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const NOW: i64 = 1_800_000_000_000;
const MIGRATION: &str = include_str!("../../migrations/0124_remove_rate_limits.sql");

async fn rate_holds(pool: &PgPool, nodes: &[Uuid], projects: &[Uuid]) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT hold_kind FROM sealed_history_holds($1,$2,$3) WHERE hold_code='history_rate_limited' ORDER BY 1",
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

async fn format(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn presence_guard(pool: &PgPool) -> String {
    sqlx::query_scalar(
        "SELECT prosrc FROM pg_proc WHERE proname='profiled_presence_operation_guard'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// sqlx reports PostgreSQL notices as `tracing` events; this collects them.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Applies 0124 as `role` (SET LOCAL ROLE) and returns the warnings PostgreSQL sent.
async fn migrate_as(pool: &PgPool, role: &str) -> String {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .finish();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(&format!("SET LOCAL ROLE {role}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::raw_sql(MIGRATION).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
}

#[tokio::test]
async fn migration_0124_removes_both_rates_with_recent_offers_and_reports_present() {
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
    // 720 reports from node r a minute ago: expired, so only the hour's rate counts them.
    let r = sealed::node(&pool).await;
    sealed::inventories(&pool, r, 720, NOW - 60_000).await;
    // Before: the limit rows hold both nodes, the fence refuses the 31st offer and the
    // presence guard the 721st report.
    let kinds = limit_kinds(&pool).await;
    assert!(kinds.contains(&"offers".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"presence_rate".to_string()), "{kinds:?}");
    assert_eq!(
        rate_holds(&pool, &[n, r], &[p]).await,
        ["offers", "presence_rate"]
    );
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "54000");
    assert_eq!(
        sealed::report(&pool, r, 10_001, NOW).await,
        Err("54000".to_string())
    );
    assert_eq!(fence(&pool).await, (true, true));
    assert!(presence_guard(&pool).await.contains("presence_rate"));

    let _store = db.store().await;
    assert!(format(&pool).await.parse::<u32>().unwrap() >= 124);
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
    for gone in ["offers", "project_offers", "presence_rate"] {
        assert!(!kinds.iter().any(|k| k == gone), "{gone}: {kinds:?}");
    }
    assert!(kinds.contains(&"presence_operations".to_string()));
    assert!(rate_holds(&pool, &[n, r], &[p]).await.is_empty());
    // The 31st offer passes the removed fence and fails only on the decision FK.
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "23503");
    assert_eq!(launches(&pool).await, 30, "existing offers are kept");
    // The 721st and 722nd reports are accepted; the guard keeps its live-row cap.
    assert_eq!(sealed::report(&pool, r, 10_001, NOW).await, Ok(()));
    assert_eq!(sealed::report(&pool, r, 10_002, NOW).await, Ok(()));
    let guard = presence_guard(&pool).await;
    assert!(!guard.contains("presence_rate"), "{guard}");
    assert!(
        guard.contains("sealed_history_usage('presence_operations'"),
        "{guard}"
    );
    for kind in ["offers", "presence_rate"] {
        assert_eq!(
            sealed::usage(&pool, kind, Some(n), NOW, None).await,
            (0, 0, 0, 0),
            "{kind}"
        );
    }
}

/// A migrating role that owns neither `profiled_launches` nor 0105's function cannot drop
/// the fence. 0124 still applies (the state format moves to 124) and warns that the fence
/// it leaves counts no offers but still serialises launch inserts.
#[tokio::test]
async fn migration_0124_without_ownership_of_the_fence_warns_and_leaves_it_inert() {
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
         ALTER FUNCTION profiled_presence_operation_guard() OWNER TO {migrator};
         ALTER TABLE profiled_launches OWNER TO {other};
         ALTER FUNCTION profiled_launch_offer_rate() OWNER TO {other};"
    );
    sqlx::raw_sql(&setup).execute(&pool).await.unwrap();
    let warnings = migrate_as(&pool, &migrator).await;
    assert!(
        warnings.contains("offer rate fence left on profiled_launches")
            && warnings.contains("owns neither")
            && warnings.contains("still serialises launch inserts"),
        "{warnings}"
    );
    assert_eq!(format(&pool).await, "124");

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

/// A migrating role that owns 0105's function but not `profiled_launches` cannot drop the
/// trigger, so it empties the function: no advisory lock serialises launch inserts.
#[tokio::test]
async fn migration_0124_owning_only_the_fence_function_empties_it() {
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
         ALTER FUNCTION profiled_presence_operation_guard() OWNER TO {migrator};
         ALTER FUNCTION profiled_launch_offer_rate() OWNER TO {migrator};
         ALTER TABLE profiled_launches OWNER TO {other};"
    );
    sqlx::raw_sql(&setup).execute(&pool).await.unwrap();
    let warnings = migrate_as(&pool, &migrator).await;
    assert!(
        warnings.contains("its function was emptied") && !warnings.contains("serialises"),
        "{warnings}"
    );
    assert_eq!(format(&pool).await, "124");
    assert_eq!(
        fence(&pool).await,
        (true, true),
        "trigger left, function kept"
    );
    let body: String =
        sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname='profiled_launch_offer_rate'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(body.trim(), "BEGIN RETURN NEW; END", "{body}");
    let (n, p) = (Uuid::new_v4(), Uuid::new_v4());
    sealed::offers(&pool, n, p, NOW - 1_000, 30).await;
    assert_eq!(sealed::try_offer(&pool, n, p, NOW).await, "23503");

    let cleanup = format!(
        "REASSIGN OWNED BY {migrator},{other} TO current_user;
         DROP OWNED BY {migrator},{other}; DROP ROLE {migrator}; DROP ROLE {other};"
    );
    sqlx::raw_sql(&cleanup).execute(&pool).await.unwrap();
}
