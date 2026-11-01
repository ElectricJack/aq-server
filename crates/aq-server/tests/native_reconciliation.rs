//! Mounted bounded recovery reads; no provider or physical host is invoked.
mod common;
use aq_admission::{
    LaunchBinding, SessionIdentity,
    control::{ControlReply, CurrentPhase, ReconcileEntry},
    transport::{JournalEvidence, Reconcile, ReconcileAttempt},
};
use aq_server::{
    profiled_execution::{no_grant, reconciliation, started, stopped},
    worker_api,
};
use common::native_offer;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
fn request(binding: &LaunchBinding) -> Reconcile {
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        panic!()
    };
    Reconcile {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity { node_id, boot_id },
        after_attempt: None,
        limit: 32,
        attempts: vec![ReconcileAttempt {
            binding: binding.clone(),
            guardian_id: Uuid::new_v4(),
            launch_id: Uuid::new_v4(),
            last_grant_id: None,
            journal: JournalEvidence::Unavailable {
                reason: aq_admission::transport::JournalUnavailable::Missing,
            },
        }],
    }
}
async fn call(
    state: &aq_server::AppState,
    credential: &str,
    request: &Reconcile,
) -> (axum::http::StatusCode, Value) {
    let nonce = Uuid::new_v4();
    let response = aq_server::build_worker_router(state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/reconcile")
                .header("authorization", format!("Bearer {credential}"))
                .header("aq-delivery-nonce", nonce.to_string())
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::to_vec(request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    if status.is_success() {
        let ControlReply::Reconciled(page) = aq_admission::control::decode_reply(&bytes).unwrap()
        else {
            panic!()
        };
        page.validate_for(request, nonce).unwrap();
    }
    (status, value)
}
fn phase(value: &Value) -> &Value {
    &value["body"]["attempts"][0]["authority"]["phase"]
}
async fn snapshot(pool: &sqlx::PgPool, attempt: Uuid) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',to_jsonb(h),'attempt',to_jsonb(a),'journal',to_jsonb(j),'receipts',(SELECT count(*) FROM profiled_execution_receipts)) FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN profiled_owner_journals j USING(attempt_id) WHERE a.attempt_id=$1").bind(attempt).fetch_one(pool).await.unwrap()
}
#[tokio::test]
async fn actual_publication_and_preparation_are_read_without_adopting_input_journal() {
    let (c, offer) = native_offer::published().await;
    let mut q = request(&offer.offered.offer.binding);
    let before = snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await;
    let (status, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(phase(&page)["kind"], "offered");
    assert_eq!(
        before,
        snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await
    );
    let accept = native_offer::acceptance(&c, &offer);
    let plan = native_offer::accept(&c, &accept).await.original;
    q.attempts[0].guardian_id = accept.guardian_id;
    q.attempts[0].launch_id = accept.launch_id;
    q.attempts[0].journal = JournalEvidence::Recorded {
        revision: 9999,
        digest: aq_admission::Digest::of(b"caller history is not adopted"),
    };
    let before = snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await;
    let (status, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(phase(&page)["kind"], "preparing");
    assert_eq!(
        phase(&page)["preparation_operation_id"],
        accept.operation_id.to_string()
    );
    assert_eq!(
        phase(&page)["preparation_digest"],
        aq_selection::digest(&plan).unwrap().as_str()
    );
    assert_eq!(
        before,
        snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await
    );
    assert!(!page.to_string().contains("delivery_clock"));
}
#[tokio::test]
async fn explicit_scope_owner_refusals_and_cursor_progress_do_not_leak_other_bindings() {
    let (c, offer) = native_offer::published().await;
    let q = request(&offer.offered.offer.binding);
    let mut unknown = q.clone();
    let id = Uuid::new_v4();
    unknown.attempts[0].binding.attempt_id = id;
    if let SessionIdentity::Remote { attempt_id, .. } = &mut unknown.attempts[0].binding.session {
        *attempt_id = id;
    }
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &unknown).await;
    assert_eq!(
        page["body"]["attempts"][0],
        json!({"kind":"refused","attempt_id":id,"code":"unknown_or_not_owned"})
    );
    let accept = native_offer::acceptance(&c, &offer);
    native_offer::accept(&c, &accept).await;
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(page["body"]["attempts"][0]["kind"], "refused");
    assert!(page["body"]["attempts"][0].get("authority").is_none());
    let mut scan = q.clone();
    scan.attempts.clear();
    scan.limit = 1;
    let (status, page) = call(&c.env.env.state, &c.env.env.credential, &scan).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["body"]["attempts"].as_array().unwrap().len(), 1);
    scan.after_attempt = Some(serde_json::from_value(page["body"]["next_after"].clone()).unwrap());
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &scan).await;
    assert_eq!(page["body"]["attempts"], json!([]));
    assert_eq!(page["body"]["next_after"], Value::Null);
    scan.identity.boot_id = Uuid::new_v4();
    scan.after_attempt = None;
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &scan).await;
    assert_eq!(page["body"]["attempts"], json!([]));
    scan.identity.node_id = Uuid::new_v4();
    assert_eq!(
        call(&c.env.env.state, &c.env.env.credential, &scan).await.0,
        403
    );
}
#[tokio::test]
async fn expired_snapshot_does_not_close_and_actual_never_granted_receipt_is_required() {
    let (mut c, offer) = native_offer::published().await;
    let q = request(&offer.offered.offer.binding);
    let at = offer.offered.clock.expires_at_ms;
    c.env.env.state.clock =
        Arc::new(move || chrono::DateTime::from_timestamp_millis(at as i64).unwrap());
    let before = snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await;
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(phase(&page)["kind"], "stop_required");
    assert_eq!(phase(&page)["reason"], "expired");
    assert_eq!(
        before,
        snapshot(&c.env.env.pool, q.attempts[0].binding.attempt_id).await
    );
    let closed = no_grant::close_expired(
        &c.env.env.state,
        c.request.identity.node_id,
        q.attempts[0].binding.attempt_id,
    )
    .await
    .unwrap()
    .unwrap();
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(phase(&page)["kind"], "never_granted");
    assert_eq!(
        phase(&page)["closure"],
        serde_json::to_value(closed.artifact).unwrap()
    );
    let current: Value = sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
        .bind(c.request.identity.node_id)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    let mut registration: aq_admission::transport::Register =
        serde_json::from_value(current["native"]["request"].clone()).unwrap();
    registration.operation_id = Uuid::new_v4();
    registration.identity.boot_id = Uuid::new_v4();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    let auth = worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap();
    aq_server::profiled_execution::registration::register(&c.env.env.state, &auth, &registration)
        .await
        .unwrap();
    let (status, historical) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(status, 200, "{historical}");
    assert_eq!(phase(&historical)["kind"], "never_granted");
    assert_eq!(
        historical["body"]["identity"]["boot_id"],
        q.identity.boot_id.to_string()
    );
}
#[tokio::test]
async fn actual_started_and_stopped_receipts_remain_readable_after_launch_configuration_retires() {
    let (mut f, start) = common::native_started::setup(true).await;
    let auth = common::native_started::context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let mut q = request(&start.binding);
    q.attempts[0].guardian_id = start.guardian_id;
    q.attempts[0].launch_id = start.launch_id;
    q.attempts[0].last_grant_id = Some(start.grant_id);
    let page = reconciliation::reconcile(&f.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    let ReconcileEntry::Current { authority } = &page.attempts[0] else {
        panic!()
    };
    assert!(
        matches!(authority.phase,CurrentPhase::Granted {started_operation_id:Some(op),..} if op==start.operation_id)
    );
    let stop = common::native_started::stop_request(&start);
    let receipt = stopped::record(&f.state, &auth, &stop).await.unwrap();
    let mut config = (*f.state.config).clone();
    config.profiled_execution = None;
    f.state.config = Arc::new(config);
    let pool = f.db.pool().await;
    let before = snapshot(&pool, start.binding.attempt_id).await;
    let page = reconciliation::reconcile(&f.state, &auth, &q, Uuid::new_v4())
        .await
        .unwrap();
    let ReconcileEntry::Current { authority } = &page.attempts[0] else {
        panic!()
    };
    assert!(
        matches!(&authority.phase,CurrentPhase::Stopped {stopped_receipt,..} if *stopped_receipt==receipt.artifact().unwrap())
    );
    assert_eq!(before, snapshot(&pool, start.binding.attempt_id).await);
}
#[tokio::test]
async fn mounted_contract_requires_current_auth_exact_nonce_and_bounded_strict_body() {
    let (c, offer) = native_offer::published().await;
    let q = request(&offer.offered.offer.binding);
    let valid = serde_json::to_string(&q).unwrap();
    let unknown = valid.replacen("{", "{\"unexpected_scope\":true,", 1);
    let duplicate = format!("{{\"schema_version\":1,{}", &valid[1..]);
    for body in [
        duplicate,
        unknown,
        "x".repeat(aq_admission::transport::MAX_FRAME_BYTES + 1),
    ] {
        let response = aq_server::build_worker_router(c.env.env.state.clone())
            .oneshot(
                axum::http::Request::post("/v1/worker/profiled/reconcile")
                    .header("authorization", format!("Bearer {}", c.env.env.credential))
                    .header("aq-delivery-nonce", Uuid::new_v4().to_string())
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
    }
    let no_nonce = native_offer::post(
        &c.env.env.state,
        &c.env.env.credential,
        "/v1/worker/profiled/reconcile",
        valid.into_bytes(),
    )
    .await;
    assert_eq!(no_nonce.status(), 400);
    assert_eq!(call(&c.env.env.state, "aqn_invalid", &q).await.0, 401);
    let response = aq_server::build_admin_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/reconcile")
                .header("x-aq-request", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(
        aq_server::admission::classify("/v1/worker/profiled/reconcile"),
        aq_server::admission::Class::Control
    );
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    let auth = worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(c.request.identity.node_id)
        .bind(aq_server::tokens::verifier(
            &aq_server::tokens::new_node_credential(),
        ))
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    assert!(
        reconciliation::reconcile(&c.env.env.state, &auth, &q, Uuid::new_v4())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unsupported_history_advances_cursor_without_synthesizing_missing_receipts() {
    let (c, offer) = native_offer::published().await;
    let accept = native_offer::acceptance(&c, &offer);
    native_offer::accept(&c, &accept).await;
    let mut q = request(&offer.offered.offer.binding);
    q.attempts[0].guardian_id = accept.guardian_id;
    q.attempts[0].launch_id = accept.launch_id;
    // Trusted test fault: lost owner receipt is unknown, never synthesized from
    // the hot pointer. Cursor advancement must not repeatedly scan this row.
    sqlx::query("DELETE FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2")
        .bind(c.request.identity.node_id)
        .bind(accept.operation_id)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    let (_, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(page["body"]["attempts"][0]["kind"], "refused");
    q.attempts.clear();
    q.limit = 1;
    let (status, page) = call(&c.env.env.state, &c.env.env.credential, &q).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["body"]["attempts"], json!([]));
    assert_eq!(
        page["body"]["next_after"],
        offer.offered.offer.binding.attempt_id.to_string()
    );
}
#[tokio::test]
async fn credential_rotation_during_observed_node_lock_wait_refuses_old_authentication() {
    let (c, offer) = native_offer::published().await;
    let q = request(&offer.offered.offer.binding);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    let auth = worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap();
    let mut held = c.env.env.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(c.request.identity.node_id)
        .execute(&mut *held)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT pg_current_xact_id()::text")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let read = reconciliation::reconcile(&c.env.env.state, &auth, &q, Uuid::new_v4());
    let rotate = async {
        let until = tokio::time::Instant::now() + std::time::Duration::from_millis(400);
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)").bind(&xid).fetch_one(&c.env.env.pool).await.unwrap();
            if waiting {
                break;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "read did not reach held node lock"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
            .bind(c.request.identity.node_id)
            .bind(aq_server::tokens::verifier(
                &aq_server::tokens::new_node_credential(),
            ))
            .execute(&mut *held)
            .await
            .unwrap();
        held.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(read, rotate);
    assert!(result.is_err());
}
