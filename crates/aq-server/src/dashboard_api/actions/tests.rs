use super::*;
use crate::{config::Config, identity::verifier_from_config, store::Role, test_support::TestDb};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

mod fixture;
mod http;
mod schema;
use fixture::*;
#[tokio::test]
async fn conflict_outcome_carries_current_versions() {
    let (_db, state, user, registry, _) = setup().await;
    let request = request("conflict");
    let op = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(op.disposition, Disposition::Conflict);
    assert_eq!(op.recovery, Recovery::RefreshAndReview);
    assert_eq!(
        op.current_versions,
        std::collections::BTreeMap::from([("test".into(), "3".into())])
    );
    let lookup = journal::lookup(&state.store.pool, user.id, request.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup, op);
    assert_eq!(json!(lookup)["current_versions"], json!({"test":"3"}));
    // Current versions are only conflict evidence; anything else is unproven.
    let bad = submit(
        &state,
        &registry,
        &user,
        self::request("confirmed_with_versions"),
    )
    .await
    .unwrap();
    assert_eq!(bad.disposition, Disposition::Unknown);
    assert!(bad.current_versions.is_empty());
    assert!(json!(bad).get("current_versions").is_none());
}
#[tokio::test]
async fn unknown_effect_operation_is_recoverable_by_lookup() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("lost");
    let original = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(original.disposition, Disposition::Unknown);
    assert_eq!(original.recovery, Recovery::LookupOriginal);
    let lookup = journal::lookup(&state.store.pool, user.id, request.operation_id)
        .await
        .unwrap();
    assert_eq!(lookup, original);
    assert_eq!(
        submit(&state, &registry, &user, request).await.unwrap(),
        original
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn concurrent_retries_and_changed_bytes_do_not_redispatch() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let (one, two) = tokio::join!(
        submit(&state, &registry, &user, request.clone()),
        submit(&state, &registry, &user, request.clone())
    );
    assert!(one.is_ok() && two.is_ok());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let mut changed = request.clone();
    changed.payload = json!({"value":"changed"});
    assert_eq!(
        submit(&state, &registry, &user, changed)
            .await
            .unwrap_err()
            .status(),
        StatusCode::CONFLICT
    );
    let mut revoked = user.clone();
    revoked.role = Role::Viewer;
    assert_eq!(
        submit(&state, &registry, &revoked, request.clone())
            .await
            .unwrap_err()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        journal::lookup(&state.store.pool, Uuid::new_v4(), request.operation_id)
            .await
            .unwrap_err()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn terminal_replay_rechecks_current_lookup_authority() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let original = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    // Model an authenticated/authorized context becoming stale during replay IO.
    // Lookup authorization reads the current row rather than the captured User.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(user.id)
        .execute(&state.store.pool)
        .await
        .unwrap();
    let error = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        original
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_payloads_and_retention_preserve_unresolved_and_tombstones() {
    let (_db, state, user, registry, count) = setup().await;
    let mut bad = request("ok");
    bad.payload["command"] = json!("forbidden");
    assert_eq!(
        submit(&state, &registry, &user, bad)
            .await
            .unwrap_err()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let resolved = request("ok");
    submit(&state, &registry, &user, resolved.clone())
        .await
        .unwrap();
    let unknown = request("lost");
    submit(&state, &registry, &user, unknown.clone())
        .await
        .unwrap();
    let now = (state.clock)();
    assert_eq!(journal::retire(&state.store.pool, now).await.unwrap(), 0);
    assert_eq!(
        journal::retire(&state.store.pool, now + chrono::Duration::days(91))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, unknown.operation_id)
            .await
            .unwrap()
            .disposition,
        Disposition::Unknown
    );
    assert_eq!(
        submit(&state, &registry, &user, resolved)
            .await
            .unwrap_err()
            .status(),
        StatusCode::GONE
    );
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert!(
        sqlx::query("DELETE FROM dashboard_operations")
            .execute(&state.store.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn admitted_but_interrupted_operation_remains_pending_without_redispatch() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("ok");
    let digest = canonical_digest(&request, &Scope::AllAuthorized).unwrap();
    let (original, dispatch) =
        journal::NewEntry::pending(&request, Scope::AllAuthorized, digest, (state.clock)())
            .admit(&state, user.id, None)
            .await
            .unwrap();
    assert!(dispatch);
    // Simulate losing the admitting request after commit, before any effect.
    let replay = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(replay, original);
    assert_eq!(replay.disposition, Disposition::Pending);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal::retire(
            &state.store.pool,
            (state.clock)() + chrono::Duration::days(365)
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn database_guards_prevent_identity_changes_early_retirement_and_schema_loss() {
    let (_db, state, user, registry, _) = setup().await;
    let request = request("ok");
    submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    for sql in [
        "UPDATE dashboard_operations SET request_digest='sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'",
        "UPDATE dashboard_operations SET principal_id='11111111-1111-4111-8111-111111111111'",
        "UPDATE dashboard_operations SET scope='{\"kind\":\"project\"}'::jsonb",
        "UPDATE dashboard_operations SET response=NULL,retired_at=updated_at+interval '89 days'",
        "TRUNCATE dashboard_operations",
    ] {
        assert!(
            sqlx::query(sql).execute(&state.store.pool).await.is_err(),
            "{sql}"
        );
    }
    sqlx::query("DROP TRIGGER dashboard_operation_guard ON dashboard_operations")
        .execute(&state.store.pool)
        .await
        .unwrap();
    assert!(
        crate::store::Store::connect_with(_db.options())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn admission_failure_never_dispatches_and_lost_completion_stays_recoverable() {
    let (_db, state, user, registry, count) = setup().await;
    sqlx::query("CREATE FUNCTION reject_test_operation() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test failure'; END $$") .execute(&state.store.pool).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_test_admission BEFORE INSERT ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION reject_test_operation()") .execute(&state.store.pool).await.unwrap();
    let request = request("ok");
    assert_eq!(
        submit(&state, &registry, &user, request.clone())
            .await
            .unwrap_err()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap_err()
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("DROP TRIGGER reject_test_admission ON dashboard_operations")
        .execute(&state.store.pool)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_test_completion BEFORE UPDATE ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION reject_test_operation()") .execute(&state.store.pool).await.unwrap();
    let operation = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(operation.disposition, Disposition::Pending);
    assert_eq!(operation.recovery, Recovery::LookupOriginal);
    assert_eq!(
        journal::lookup(&state.store.pool, user.id, request.operation_id)
            .await
            .unwrap(),
        operation
    );
    assert_eq!(
        submit(&state, &registry, &user, request).await.unwrap(),
        operation
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn timed_out_effect_is_unknown_and_never_reexecuted() {
    let (_db, state, user, registry, count) = setup().await;
    let request = request("timeout");
    let operation = submit(&state, &registry, &user, request.clone())
        .await
        .unwrap();
    assert_eq!(
        (operation.disposition, operation.recovery),
        (Disposition::Unknown, Recovery::LookupOriginal)
    );
    assert_eq!(
        submit(&state, &registry, &user, request.clone())
            .await
            .unwrap(),
        operation
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
