//! Migration012/shared inspection only. Offered synthetic native fixtures never
//! grant terminal execution or qualify a provider. Runtime remains separate.
mod common;
use aq_server::{
    error::AppError,
    execution::authority::{self, Policy},
    session_access,
    test_support::TestDb,
};
use aq_session_protocol::SessionIdentity;
use common::profiled::{NOW, fixture, insert};
use serde_json::json;
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use uuid::Uuid;
async fn access(
    f: &common::profiled::Fixture,
    pool: &PgPool,
    actor: Uuid,
) -> Result<session_access::InspectionAccess, AppError> {
    let mut tx = pool.begin().await.unwrap();
    let view = authority::load_locked(
        &f.state,
        &mut tx,
        &f.profiled.offer.binding.session,
        Policy::TerminalHistorical,
    )
    .await
    .unwrap();
    let result = session_access::require_inspect_locked(&f.state, &mut tx, &view, actor).await;
    tx.commit().await.unwrap();
    result
}
async fn viewer(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,$2,'viewer',now())")
        .bind(id)
        .bind(format!("{id}@fixture.test"))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn grant(pool: &PgPool, session: &SessionIdentity, actor: Uuid, expires: u64) {
    let key = session_access::session_key(session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2, $3,1,TRUE,$4)").bind(key).bind(json!(session)).bind(actor).bind(expires as i64).execute(pool).await.unwrap();
}
#[tokio::test]
async fn migration012_registers_only_schema_and_exact_state_format() {
    let db = TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(version, "12");
    let migrations: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(migrations, (1..=12).collect::<Vec<_>>());
    for table in [
        "session_inspection_acl",
        "terminal_control_acl",
        "terminal_sessions",
        "terminal_operations",
        "terminal_input_receipts",
        "terminal_subscriptions",
        "terminal_outbox",
    ] {
        let found: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(found.is_some(), "{table}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM terminal_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn current_roles_exact_session_revocation_expiry_and_deletion_are_rechecked() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let who = viewer(&pool).await;
    assert!(access(&f, &pool, f.actor).await.unwrap().owner);
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Forbidden(_))
    ));
    grant(&pool, &f.profiled.offer.binding.session, who, NOW + 1000).await;
    let allowed = access(&f, &pool, who).await.unwrap();
    assert!(!allowed.owner);
    assert_eq!(allowed.session, f.profiled.offer.binding.session);
    assert_eq!(allowed.valid_until_ms, NOW + 1000);
    let key = session_access::session_key(&f.profiled.offer.binding.session).unwrap();
    sqlx::query("UPDATE session_inspection_acl SET can_inspect=FALSE,revision=revision+1 WHERE session_key=$1 AND principal_id=$2").bind(&key).bind(who).execute(&pool).await.unwrap();
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Forbidden(_))
    ));
    sqlx::query("UPDATE session_inspection_acl SET can_inspect=TRUE,revision=revision+1 WHERE session_key=$1 AND principal_id=$2").bind(&key).bind(who).execute(&pool).await.unwrap();
    f.time.store((NOW + 1000) as i64, Ordering::SeqCst);
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Forbidden(_))
    ));
    f.time.store(NOW as i64, Ordering::SeqCst);
    sqlx::query("DELETE FROM session_inspection_acl WHERE principal_id=$1")
        .bind(who)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(who)
        .execute(&pool)
        .await
        .unwrap();
    assert!(access(&f, &pool, who).await.unwrap().owner);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(who)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Forbidden(_))
    ));
    grant(&pool, &f.profiled.offer.binding.session, who, NOW + 1000).await;
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(who)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Forbidden(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM session_inspection_acl WHERE principal_id=$1"
        )
        .bind(who)
        .fetch_one(&pool)
        .await
        .unwrap(),
        0
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        access(&f, &pool, f.actor).await,
        Err(AppError::Forbidden(_))
    ));
}
#[tokio::test]
async fn every_remote_tuple_field_is_in_scope_and_corrupt_acl_body_cannot_authorize() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let who = viewer(&pool).await;
    let exact = f.profiled.offer.binding.session.clone();
    let key = session_access::session_key(&exact).unwrap();
    for field in 0..5 {
        let mut other = exact.clone();
        if let SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } = &mut other
        {
            match field {
                0 => *node_id = Uuid::new_v4(),
                1 => *boot_id = Uuid::new_v4(),
                2 => *attempt_id = Uuid::new_v4(),
                3 => *assignment_generation += 1,
                4 => *session_id = Uuid::new_v4(),
                _ => unreachable!(),
            }
        }
        assert_ne!(key, session_access::session_key(&other).unwrap());
        grant(&pool, &other, who, NOW + 1000).await;
        assert!(matches!(
            access(&f, &pool, who).await,
            Err(AppError::Forbidden(_))
        ));
    }
    let mut foreign = exact.clone();
    if let SessionIdentity::Remote { session_id, .. } = &mut foreign {
        *session_id = Uuid::new_v4();
    }
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,TRUE,$4)").bind(&key).bind(json!(foreign)).bind(who).bind((NOW+1000)as i64).execute(&pool).await.unwrap();
    assert!(matches!(
        access(&f, &pool, who).await,
        Err(AppError::Internal)
    ));
    assert!(sqlx::query("UPDATE session_inspection_acl SET session_identity=$3,revision=revision+1 WHERE session_key=$1 AND principal_id=$2").bind(&key).bind(who).bind(json!(exact)).execute(&pool).await.is_err());
    assert!(sqlx::query("UPDATE session_inspection_acl SET revision=revision WHERE session_key=$1 AND principal_id=$2").bind(&key).bind(who).execute(&pool).await.is_err());
}
#[tokio::test]
async fn inspection_expiry_is_sampled_after_an_observed_acl_lock_wait() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let observer = f.db.pool().await;
    let who = viewer(&pool).await;
    grant(&pool, &f.profiled.offer.binding.session, who, NOW + 100).await;
    let key = session_access::session_key(&f.profiled.offer.binding.session).unwrap();
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("SELECT revision FROM session_inspection_acl WHERE session_key=$1 AND principal_id=$2 FOR UPDATE").bind(key).bind(who).fetch_one(&mut *blocker).await.unwrap();
    let state = f.state.clone();
    let session = f.profiled.offer.binding.session.clone();
    let work = pool.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut tx = work.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        send.send(pid).unwrap();
        let view = authority::load_locked(&state, &mut tx, &session, Policy::TerminalHistorical)
            .await
            .unwrap();
        let access = session_access::require_inspect_locked(&state, &mut tx, &view, who).await;
        tx.commit().await.unwrap();
        access
    });
    let pid = receive.await.unwrap();
    let mut observed = false;
    for _ in 0..100 {
        let waiting:bool=sqlx::query_scalar("SELECT coalesce((SELECT wait_event_type='Lock' FROM pg_stat_activity WHERE pid=$1),FALSE)").bind(pid).fetch_one(&observer).await.unwrap();
        if waiting {
            observed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(observed, "ACL query never reached an actual SQL lock wait");
    f.time.store((NOW + 100) as i64, Ordering::SeqCst);
    blocker.rollback().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(AppError::Forbidden(_))));
}
