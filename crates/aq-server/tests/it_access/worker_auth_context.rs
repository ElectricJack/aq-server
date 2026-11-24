//! Actual PostgreSQL authentication-window fences, with synthetic enrolled nodes.
//! The fixture is Offered only; this file grants no execution/terminal authority.
use crate::common;
use aq_server::{
    error::AppError,
    worker_api::{authenticate_context, lock_current_node},
};
use axum::http::HeaderMap;
use common::profiled::fixture;
fn headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers
}
#[tokio::test]
async fn authenticated_context_is_redacted_and_rechecks_revocation_under_lock() {
    let f = fixture().await;
    let context = authenticate_context(&f.state, &headers(&f.credential))
        .await
        .unwrap();
    assert_eq!(context.id(), f.node.node_id);
    assert_eq!(context.node().id, context.id());
    let debug = format!("{context:?}");
    assert!(!debug.contains(&f.credential));
    assert!(!debug.contains(&aq_server::tokens::verifier(&f.credential)));
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        lock_current_node(&mut tx, &context).await.unwrap(),
        context.id()
    );
    tx.commit().await.unwrap();
    f.state
        .store
        .revoke_node(context.id(), (f.state.clock)())
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        lock_current_node(&mut tx, &context).await,
        Err(AppError::CredentialRevoked)
    ));
    tx.rollback().await.unwrap();
    assert!(matches!(
        authenticate_context(&f.state, &headers(&f.credential)).await,
        Err(AppError::CredentialRevoked)
    ));
}
#[tokio::test]
async fn credential_rotation_during_observed_node_lock_wait_refuses_old_context() {
    let f = fixture().await;
    let context = authenticate_context(&f.state, &headers(&f.credential))
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(context.id())
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let waiting_pool = f.db.pool().await;
    let waiting_context = context.clone();
    let (ready, started) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let mut tx = waiting_pool.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        ready.send(pid).unwrap();
        let result = lock_current_node(&mut tx, &waiting_context).await;
        tx.rollback().await.unwrap();
        result
    });
    let pid = started.await.unwrap();
    await_backend_lock(&pool, pid).await;
    let replacement = aq_server::tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(context.id())
        .bind(aq_server::tokens::verifier(&replacement))
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .unwrap()
            .unwrap(),
        Err(AppError::CredentialRevoked)
    ));
    assert!(
        authenticate_context(&f.state, &headers(&f.credential))
            .await
            .is_err()
    );
    let current = authenticate_context(&f.state, &headers(&replacement))
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        lock_current_node(&mut tx, &current).await.unwrap(),
        context.id()
    );
    tx.commit().await.unwrap();
}
async fn await_backend_lock(pool: &sqlx::PgPool, pid: i32) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no observed node lock wait"
        );
        tokio::task::yield_now().await;
    }
}
