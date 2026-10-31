//! Real SQL/router ownership checks with explicitly synthetic host/runtime data.
//! Actual kernel/tmux journal production is qualified separately in aq-worker.
mod common;
use aq_admission::Digest;
use aq_server::{
    execution::{
        authority::{
            self, Policy,
            hot::{HotState, LaunchOwner},
        },
        profiled::{Status, Stop},
    },
    profiled_execution::{journal, started},
};
use common::{
    native_started::{context, setup, setup_native, state, stop_request},
    profiled::{NOW, artifact},
};
use serde_json::json;
use std::{sync::atomic::Ordering, time::Duration};
#[tokio::test]
async fn original_started_is_atomic_and_historical_replay_never_revives_stopped_host() {
    let (f, r) = setup(true).await;
    let auth = context(&f).await;
    let before = state(&f, &r).await;
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    assert!(
        authority::load_locked(
            &f.state,
            &mut tx,
            &r.binding.session,
            Policy::TerminalActive
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    let first = started::record(&f.state, &auth, &r).await.unwrap();
    first.validate_for(&r).unwrap();
    let after = state(&f, &r).await;
    assert_eq!(after["stop"], json!({"kind":"running"}));
    assert_eq!(after["grant"], before["grant"]);
    assert_eq!(
        after["execution_deadline_ms"],
        before["execution_deadline_ms"]
    );
    let mut tx = pool.begin().await.unwrap();
    authority::load_locked(
        &f.state,
        &mut tx,
        &r.binding.session,
        Policy::TerminalActive,
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let replay = started::record(&f.state, &auth, &r).await.unwrap();
    assert_eq!(json!(first), json!(replay));
    assert_eq!(state(&f, &r).await, after);
    let mut changed = r.clone();
    changed.journal_revision += 1;
    assert!(started::record(&f.state, &auth, &changed).await.is_err());
    changed = r.clone();
    changed.operation_id = uuid::Uuid::new_v4();
    assert!(started::record(&f.state, &auth, &changed).await.is_err());
    let mut stopped: HotState = serde_json::from_value(after).unwrap();
    stopped.status = Status::Finished;
    stopped.stop = Stop::Stopped {
        receipt: artifact("synthetic-stop-owner", b"not-a-live-worker-proof"),
    };
    assert!(
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .bind(json!(stopped))
            .execute(&pool)
            .await
            .is_err()
    );
    aq_server::profiled_execution::stopped::record(&f.state, &auth, &stop_request(&r))
        .await
        .unwrap();
    f.time.store((NOW + 40_000) as i64, Ordering::SeqCst);
    assert_eq!(
        json!(started::record(&f.state, &auth, &r).await.unwrap()),
        json!(first)
    );
    assert_eq!(state(&f, &r).await["stop"]["kind"], "stopped");
    assert!(
        sqlx::query("DELETE FROM profiled_started_receipts WHERE attempt_id=$1")
            .bind(r.binding.attempt_id)
            .execute(&pool)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn missing_prepared_head_foreign_owner_and_unregistered_allowlist_never_make_running() {
    let (f, r) = setup(false).await;
    let auth = context(&f).await;
    assert!(started::record(&f.state, &auth, &r).await.is_err());
    let pool = f.db.pool().await;
    assert!(sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{stop}','{\"kind\":\"running\"}') WHERE attempt_id=$1").bind(r.binding.attempt_id).execute(&pool).await.is_err());
    assert_eq!(state(&f, &r).await["stop"]["kind"], "unknown");
    let (g, r) = setup(true).await;
    let auth = context(&g).await;
    let mut changed = r.clone();
    changed.guardian_id = uuid::Uuid::new_v4();
    assert!(started::record(&g.state, &auth, &changed).await.is_err());
    changed = r.clone();
    changed.qualification = artifact("other-qualification", b"other");
    assert!(started::record(&g.state, &auth, &changed).await.is_err());
    let (g, r) = setup_native(true, false).await;
    let auth = context(&g).await;
    assert!(started::record(&g.state, &auth, &r).await.is_err());
    assert_eq!(state(&g, &r).await["stop"]["kind"], "unknown");
}
#[tokio::test]
async fn receipt_journal_and_running_rollback_together_and_rotated_auth_cannot_replay() {
    let (f, r) = setup(true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    sqlx::raw_sql("CREATE FUNCTION fail_started() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.phase='started' THEN RAISE EXCEPTION 'synthetic failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_started BEFORE UPDATE ON profiled_owner_journals FOR EACH ROW EXECUTE FUNCTION fail_started();").execute(&pool).await.unwrap();
    assert!(started::record(&f.state, &auth, &r).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_started_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(state(&f, &r).await["stop"]["kind"], "unknown");
    sqlx::query("DROP TRIGGER fail_started ON profiled_owner_journals")
        .execute(&pool)
        .await
        .unwrap();
    started::record(&f.state, &auth, &r).await.unwrap();
    let credential = aq_server::tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier(&credential))
        .execute(&pool)
        .await
        .unwrap();
    assert!(started::record(&f.state, &auth, &r).await.is_err());
}
#[tokio::test]
async fn grant_expiring_while_waiting_for_owner_lock_cannot_become_running() {
    let (f, r) = setup(true).await;
    let auth = context(&f).await;
    let pool = f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT attempt_id FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE")
        .bind(r.binding.attempt_id)
        .execute(&mut *held)
        .await
        .unwrap();
    let app = f.state.clone();
    let request = r.clone();
    let call = tokio::spawn(async move { started::record(&app, &auth, &request).await });
    let mut observed = false;
    for _ in 0..200 {
        observed=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT phase,revision,digest FROM profiled_owner_journals%')").fetch_one(&pool).await.unwrap();
        if observed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(observed, "actual owner lock wait must be observed");
    f.time.store((NOW + 1_000) as i64, Ordering::SeqCst);
    held.commit().await.unwrap();
    assert!(call.await.unwrap().is_err());
    assert_eq!(state(&f, &r).await["stop"]["kind"], "unknown");
}
#[tokio::test]
async fn worker_http_start_report_uses_current_node_auth_and_returns_only_historical_ack() {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use tower::ServiceExt;
    let (f, r) = setup(true).await;
    let router = aq_server::build_worker_router(f.state.clone());
    let request = |token: &str, body: Vec<u8>| {
        HttpRequest::builder()
            .method("POST")
            .uri("/v1/worker/profiled/started")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let bytes = serde_json::to_vec(&r).unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(request("aqn_wrong", bytes.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = router
        .clone()
        .oneshot(request(&f.credential, bytes.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let response = axum::body::to_bytes(response.into_body(), 16384)
        .await
        .unwrap();
    aq_admission::started::decode_receipt(&response, &r).unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(request(&f.credential, vec![b' '; 16385]))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_ne!(
        router
            .oneshot(request(&f.credential, bytes))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}
#[tokio::test]
async fn journal_heads_preserve_exact_phase_scope_and_require_original_start_record() {
    let (f, r) = setup(true).await;
    let pool = f.db.pool().await;
    let auth = context(&f).await;
    let mut tx = pool.begin().await.unwrap();
    aq_server::worker_api::lock_current_node(&mut tx, &auth)
        .await
        .unwrap();
    authority::load_locked(&f.state, &mut tx, &r.binding.session, Policy::IdentityOnly)
        .await
        .unwrap();
    let owner = LaunchOwner {
        guardian_id: r.guardian_id,
        launch_id: r.launch_id,
        reservation_id: r.reservation_id,
    };
    journal::record_prepared(
        &mut tx,
        &r.binding,
        &owner,
        r.grant_id,
        2,
        &Digest::of(b"prepared"),
    )
    .await
    .unwrap();
    assert!(
        journal::record_prepared(
            &mut tx,
            &r.binding,
            &owner,
            r.grant_id,
            2,
            &Digest::of(b"changed")
        )
        .await
        .is_err()
    );
    assert!(
        journal::record_reserved(&mut tx, &r.binding, &owner, 1, &Digest::of(b"reserved"))
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    assert!(sqlx::query("UPDATE profiled_owner_journals SET phase='started',revision=3,digest=$2 WHERE attempt_id=$1").bind(r.binding.attempt_id).bind(r.journal_digest.as_str()).execute(&pool).await.is_err());
    assert_eq!(state(&f, &r).await["stop"]["kind"], "unknown");
}
