//! Actual native cancellation/SQL owners with explicitly fake worker stop evidence.
use crate::common;
use aq_server::profiled_execution::{started, stopped};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use common::{native_cancellation::cancel, native_offer, native_started};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn slow_offer_keeps_export_but_waits_for_new_inventory_instead_of_publishing_near_expiry() {
    use std::sync::atomic::Ordering;
    let c = native_offer::setup().await;
    // The synthetic Git export advances the daemon clock after selection.
    c.mode.store(9, Ordering::SeqCst);
    let (status, page) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{page}");
    assert!(page["body"]["offers"].as_array().unwrap().is_empty());
    let verified: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_source_exports WHERE state='verified'")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        verified, 1,
        "the completed export is kept for a fresh inventory"
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE node_id=$1")
            .bind(c.request.identity.node_id)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(
        attempts, 0,
        "waiting for fresh inventory must not spend an attempt"
    );
}

#[tokio::test]
async fn ordinary_slow_offer_survives_poll_return_and_blocks_duplicate_assembly() {
    use std::{sync::atomic::Ordering, time::Duration};
    let mut c = native_offer::setup().await;
    c.mode.store(6, Ordering::SeqCst);
    let (status, page) = tokio::time::timeout(Duration::from_secs(2), native_offer::poll(&c))
        .await
        .expect("ordinary poll must return while source observation is paused");
    assert_eq!(status, 200, "{page}");
    assert!(page["body"]["offers"].as_array().unwrap().is_empty());
    c.observation_entered.acquire().await.unwrap().forget();
    c.request.operation_id = Uuid::new_v4();
    let (status, pending) = tokio::time::timeout(Duration::from_secs(1), native_offer::poll(&c))
        .await
        .expect("another poll must not start another assembly for this node");
    assert_eq!(status, 200, "{pending}");
    assert!(pending["body"]["offers"].as_array().unwrap().is_empty());
    assert_eq!(c.observation_entered.available_permits(), 0);
    c.mode.store(0, Ordering::SeqCst);
    c.observation_resume.add_permits(1);
    let attempt: Uuid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(id) = sqlx::query_scalar(
                "SELECT attempt_id FROM execution_attempts WHERE node_id=$1 AND holds_task",
            )
            .bind(c.request.identity.node_id)
            .fetch_optional(&c.env.env.pool)
            .await
            .unwrap()
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("detached assembler must publish the retained offer");
    let (status, replayed) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(
        replayed["body"]["offers"][0]["offered"]["offer"]["binding"]["attempt_id"],
        json!(attempt)
    );
}

#[tokio::test]
async fn native_cancel_slow_offer_continues_after_prompt_directive() {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    let (mut c, offer) = native_offer::published().await;
    let old = offer.offered.offer.binding.attempt_id;
    assert_eq!(
        cancel(&c.env.env.state, old, Uuid::new_v4(), "reschedule")
            .await
            .0,
        200
    );
    c.mode.store(6, Ordering::SeqCst);
    c.request.operation_id = Uuid::new_v4();
    let (status, page) = tokio::time::timeout(Duration::from_secs(1), native_offer::poll(&c))
        .await
        .expect("cancellation must not wait for the paused Graph source");
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["body"]["cancellations"].as_array().unwrap().len(), 1);
    assert!(page["body"]["offers"].as_array().unwrap().is_empty());
    c.observation_entered.acquire().await.unwrap().forget();
    c.mode.store(0, Ordering::SeqCst);
    c.observation_resume.add_permits(1);
    let replacement: Uuid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(id) = sqlx::query_scalar("SELECT attempt_id FROM execution_attempts WHERE node_id=$1 AND holds_task AND attempt_id<>$2")
                .bind(c.request.identity.node_id).bind(old)
                .fetch_optional(&c.env.env.pool).await.unwrap() { break id; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("slow healthy scheduling must finish after cancellation delivery");
    let (status, next) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{next}");
    assert_eq!(
        next["body"]["offers"][0]["offered"]["offer"]["binding"]["attempt_id"],
        json!(replacement)
    );
}

#[tokio::test]
async fn native_cancel_before_grant_releases_claim() {
    for preparing in [false, true] {
        let (mut c, offer) = native_offer::published().await;
        if preparing {
            native_offer::accept(&c, &native_offer::acceptance(&c, &offer)).await;
        }
        let binding = &offer.offered.offer.binding;
        let operation = Uuid::new_v4();
        let (status, first) = cancel(
            &c.env.env.state,
            binding.attempt_id,
            operation,
            "owner changed plan",
        )
        .await;
        assert_eq!(status, 200, "{first}");
        assert_eq!(first["disposition"], "never_granted");
        assert_eq!(first["cancellation_id"], json!(operation));
        let (held, hot): (bool, Value) = sqlx::query_as("SELECT a.holds_task,h.state FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE attempt_id=$1")
            .bind(binding.attempt_id).fetch_one(&c.env.env.pool).await.unwrap();
        assert!(!held);
        assert_eq!(hot["status"], "never_granted");
        assert_eq!(hot["stop"]["kind"], "unknown");
        assert_eq!(hot["grant"], Value::Null);
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT profiled_never_granted_valid($1)")
                .bind(binding.attempt_id)
                .fetch_one(&c.env.env.pool)
                .await
                .unwrap()
        );
        assert_eq!(
            cancel(
                &c.env.env.state,
                binding.attempt_id,
                operation,
                "owner changed plan"
            )
            .await
            .1,
            first
        );
        assert_eq!(
            cancel(&c.env.env.state, binding.attempt_id, operation, "different")
                .await
                .0,
            409
        );
        c.request.available.slots = 0;
        c.request.operation_id = Uuid::new_v4();
        let (status, page) = native_offer::poll(&c).await;
        assert_eq!(status, 200, "{page}");
        assert_eq!(page["body"]["cancellations"][0]["binding"], json!(binding));
        assert_eq!(page["body"]["cancellations"][0]["reason"], "owner_request");
    }
}

#[tokio::test]
async fn native_cancel_after_start_stops_and_settles_cancelled() {
    let (f, start) = native_started::setup(true).await;
    let auth = native_started::context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let operation = Uuid::new_v4();
    let (status, receipt) = cancel(
        &f.state,
        start.binding.attempt_id,
        operation,
        "stop fake worker",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["disposition"], "requested");
    let pool = f.db.pool().await;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_cancelled_settlements WHERE attempt_id=$1",
    )
    .bind(start.binding.attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0, "cancellation is not stop proof");
    assert!(sqlx::query("INSERT INTO profiled_cancelled_settlements(attempt_id,cancellation_id,stop_receipt_digest,recorded_at_ms) VALUES($1,$2,$3,1)").bind(start.binding.attempt_id).bind(operation).bind("0".repeat(64)).execute(&pool).await.is_err(),"SQL must reject a fabricated stopped disposition");

    let hot = native_started::state(&f, &start).await;
    assert_eq!(hot["stop"]["kind"], "running");
    let renew = aq_admission::transport::Renew {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        binding: start.binding.clone(),
        grant_id: start.grant_id,
        reservation_id: start.reservation_id,
        guardian_id: start.guardian_id,
        launch_id: start.launch_id,
        journal_revision: start.journal_revision + 1,
    };
    let response = aq_server::build_worker_router(f.state.clone())
        .oneshot(
            Request::post("/v1/worker/profiled/renew")
                .header("authorization", format!("Bearer {}", f.credential))
                .header("aq-delivery-nonce", Uuid::new_v4().to_string())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&renew).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    let reply: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(reply["body"]["code"], "cancelled");
    let poll = aq_admission::transport::Poll {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: renew.identity.clone(),
        inventory_sequence: 1,
        available: aq_admission::Resources {
            slots: 0,
            memory_bytes: 0,
            cpu_millis: 0,
            disk_bytes: 0,
        },
        limit: 1,
        after_offer: None,
    };
    let page = aq_server::profiled_execution::producer::poll(&f.state, &auth, &poll)
        .await
        .unwrap();
    assert!(page.offers.is_empty());
    assert_eq!(page.cancellations.len(), 1);
    assert_eq!(page.cancellations[0].cancellation_id, operation);
    assert_eq!(page.cancellations[0].binding, start.binding);
    assert_eq!(
        page.cancellations[0].authority_revision,
        receipt["authority_revision"].as_u64().unwrap()
    );
    let mut stale = poll.clone();
    stale.identity.boot_id = Uuid::new_v4();
    assert!(
        aq_server::profiled_execution::producer::poll(&f.state, &auth, &stale)
            .await
            .is_err()
    );
    let mut stop = native_started::stop_request(&start);
    stop.evidence.reason = aq_admission::transport::StopReason::Cancellation;
    let stopped_receipt = stopped::record(&f.state, &auth, &stop).await.unwrap();
    let (id, proof): (Uuid, String) = sqlx::query_as("SELECT cancellation_id,stop_receipt_digest FROM profiled_cancelled_settlements WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&pool).await.unwrap();
    assert_eq!(id, operation);
    assert_eq!(proof, stopped_receipt.artifact().unwrap().digest.as_str());

    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(start.binding.attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        "stop alone does not release source/effect claims"
    );
    assert_eq!(
        cancel(
            &f.state,
            start.binding.attempt_id,
            operation,
            "stop fake worker"
        )
        .await
        .1,
        receipt
    );
}

#[tokio::test]
async fn native_cancel_granted_without_started_retains_unknown_writer_and_claim() {
    let (f, start) = native_started::setup(true).await;
    let auth = native_started::context(&f).await;
    let (status, receipt) = cancel(
        &f.state,
        start.binding.attempt_id,
        Uuid::new_v4(),
        "grant may have escaped",
    )
    .await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["disposition"], "requested");
    assert!(started::record(&f.state, &auth, &start).await.is_err());
    let hot = native_started::state(&f, &start).await;
    assert_eq!(hot["status"], "cancelling");
    assert_eq!(hot["stop"]["kind"], "unknown");
    assert_eq!(hot["grant"]["grant_id"], json!(start.grant_id));
    let (held,settlements,closures):(bool,i64,i64)=sqlx::query_as("SELECT holds_task,(SELECT count(*) FROM profiled_cancelled_settlements WHERE attempt_id=$1),(SELECT count(*) FROM profiled_never_granted_closures WHERE attempt_id=$1) FROM execution_attempts WHERE attempt_id=$1").bind(start.binding.attempt_id).fetch_one(&f.db.pool().await).await.unwrap();
    assert!(held);
    assert_eq!((settlements, closures), (0, 0));
}

#[tokio::test]
async fn native_cancel_owner_validation_attempt_replay_and_concurrency() {
    let (f, start) = native_started::setup(true).await;
    let attempt = start.binding.attempt_id;
    assert_eq!(
        common::native_cancellation::cancel_as(
            &f.state,
            attempt,
            Uuid::new_v4(),
            "unauthorized",
            "viewer@localhost"
        )
        .await
        .0,
        403
    );
    assert_eq!(
        cancel(&f.state, attempt, Uuid::nil(), "invalid operation")
            .await
            .0,
        400
    );
    assert_eq!(cancel(&f.state, attempt, Uuid::new_v4(), "  ").await.0, 400);
    let operation = Uuid::new_v4();
    let (first, second) = tokio::join!(
        cancel(&f.state, attempt, operation, "same"),
        cancel(&f.state, attempt, operation, "same")
    );
    assert_eq!(first.0, 200, "{:?}", first.1);
    assert_eq!(first, second);
    // A second actual native attempt in this database.
    // The operation payload contains the attempt id and is globally fenced.
    let (mut c, offer) = native_offer::published().await;
    let op = Uuid::new_v4();
    assert_eq!(
        cancel(
            &c.env.env.state,
            offer.offered.offer.binding.attempt_id,
            op,
            "first"
        )
        .await
        .0,
        200
    );
    c.request.operation_id = Uuid::new_v4();
    let (status, page) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{page}");
    let offers = page["body"]["offers"].as_array().unwrap();
    assert_eq!(
        offers.len(),
        1,
        "retained cancellation must not starve the released task"
    );
    let replacement: Uuid =
        serde_json::from_value(offers[0]["offered"]["offer"]["binding"]["attempt_id"].clone())
            .unwrap();
    assert_ne!(replacement, offer.offered.offer.binding.attempt_id);
    assert_eq!(
        cancel(&c.env.env.state, replacement, op, "first").await.0,
        409
    );
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(replacement)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert!(held, "old operation must not release replacement");
}
