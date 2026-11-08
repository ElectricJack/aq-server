//! Actual synthetic-owner native publication and refusal cases. No provider calls.
use crate::common;
use aq_admission::{
    Digest,
    control::{ControlReply, decode_reply},
};
use common::native_offer::{poll, setup};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn offered_window_preserves_five_seconds_of_fresh_facts_and_allows_delayed_accept() {
    let c = setup().await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    let native = &page.offers[0];
    let clock = &native.offered.clock;
    // This catalog permits five-second-old availability observations. A fresh
    // poll must preserve that window instead of truncating it to two seconds.
    assert!(clock.expires_at_ms - clock.server_time_ms >= 5_000);
    c.env.env.time.fetch_add(3_000, Ordering::SeqCst);
    let acceptance = common::native_offer::acceptance(&c, native);
    common::native_offer::accept(&c, &acceptance).await;
}

#[tokio::test]
async fn actual_native_offer_seals_real_owner_artifacts_source_and_graph_without_grant() {
    let c = setup().await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    let native = &page.offers[0];
    let pool = &c.env.env.pool;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM execution_attempts WHERE kind='profiled_v1' AND holds_task"
        )
        .fetch_one(pool)
        .await
        .unwrap(),
        1
    );
    let hot: Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "offered");
    assert!(hot["grant"].is_null());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM admission_material_leases")
            .fetch_one(pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM agent_launch_bindings WHERE credential_verifier IS NULL AND credential_version=0").fetch_one(pool).await.unwrap(),1);
    // Spec 0006 phase 2: the offer seals the default coding grants.
    let (grants, digest): (Value, String) =
        sqlx::query_as("SELECT grants,grants_digest FROM agent_launch_grants")
            .fetch_one(pool)
            .await
            .unwrap();
    let default = aq_agent_grants::resolve::default_coding();
    assert_eq!(grants, serde_json::to_value(&default).unwrap());
    assert_eq!(digest, default.digest());
    let original: Value =
        sqlx::query_scalar("SELECT graph_task_observation FROM profiled_launches")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        original["native_offer"],
        serde_json::to_value(native).unwrap()
    );
    let (status, replayed) = poll(&c).await;
    assert_eq!(status, 200, "{replayed}");
    let ControlReply::Poll(replayed) =
        decode_reply(&serde_json::to_vec(&replayed).unwrap()).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        replayed.offers[0].offered.offer.binding,
        native.offered.offer.binding
    );
    assert_eq!(
        replayed.offers[0].offered.clock.expires_at_ms,
        native.offered.clock.expires_at_ms
    );
    let accept = aq_admission::transport::Accept {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        acceptance: aq_session_protocol::ProfiledAcceptance {
            binding: native.offered.offer.binding.clone(),
            advertised_capabilities: native.offered.offer.required_capabilities.clone(),
        },
        reservation: aq_admission::transport::NoLaunchReservation {
            reservation_id: Uuid::new_v4(),
            reservation_operation_id: Uuid::new_v4(),
            reservation_journal_digest: Digest::of(b"synthetic durable reservation"),
        },
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        journal_revision: 1,
        journal_digest: Digest::of(b"synthetic reserved journal"),
        reservation_remaining_ms: 10000,
    };
    let nonce = Uuid::new_v4();
    let response = aq_server::build_worker_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/accept")
                .header("authorization", format!("Bearer {}", c.env.env.credential))
                .header("content-type", "application/json")
                .header("aq-delivery-nonce", nonce.to_string())
                .body(axum::body::Body::from(serde_json::to_vec(&accept).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let ControlReply::Preparation(preparation) = decode_reply(&bytes).unwrap() else {
        panic!()
    };
    preparation.validate_for(&accept, nonce).unwrap();
    assert_eq!(preparation.original.materials.len(), 1);
    let hot: Value = sqlx::query_scalar("SELECT state FROM profiled_authority_hot")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(hot["status"], "preparing");
    assert!(hot["grant"].is_null());
}
#[tokio::test]
async fn task_cas_and_same_cas_content_mismatch_never_publish_an_offer() {
    let c = setup().await;
    for mode in [1, 2] {
        c.mode.store(mode, Ordering::SeqCst);
        let (status, _) = poll(&c).await;
        assert_eq!(status, 409);
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_attempts")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn missing_qualified_capacity_never_invents_zero_or_exports_source() {
    let mut c = setup().await;
    let mut config = (*c.env.env.state.config).clone();
    let mut native = (*config.profiled_execution.as_ref().unwrap().as_ref()).clone();
    native.revision += 1;
    native.context_qualifications.clear();
    config.profiled_execution = Some(std::sync::Arc::new(native));
    c.env.env.state.config = std::sync::Arc::new(config);
    aq_server::profiled_execution::policies::install(&c.env.env.state)
        .await
        .unwrap();
    let (status, _) = poll(&c).await;
    assert_eq!(status, 409);
    for table in [
        "execution_attempts",
        "profiled_source_exports",
        "admission_material_leases",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}
#[tokio::test]
async fn poll_rejects_chosen_scope_duplicate_fields_and_oversized_body_before_admission() {
    let c = setup().await;
    let original = serde_json::to_value(&c.request).unwrap();
    let mut unknown = original.clone();
    unknown["selected_task_id"] = serde_json::json!(Uuid::new_v4());
    let text = original.to_string();
    let duplicate = text.replacen(
        "\"operation_id\":",
        &format!("\"operation_id\":\"{}\",\"operation_id\":", Uuid::new_v4()),
        1,
    );
    for (bytes, status) in [
        (serde_json::to_vec(&unknown).unwrap(), 400),
        (duplicate.into_bytes(), 400),
        (vec![b' '; 16385], 413),
    ] {
        let response = common::native_offer::post(
            &c.env.env.state,
            &c.env.env.credential,
            "/v1/worker/profiled/poll",
            bytes,
        )
        .await;
        assert_eq!(response.status(), status);
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_attempts")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap(),
        0
    );
    let admin = aq_server::build_admin_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/poll")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin.status(), 404);
}

/// Spec 0006 §7: the task's thread follows the assignment in the startup
/// context, as an optional input under the startup budget.
#[tokio::test]
async fn startup_context_carries_the_task_thread() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let thread = aq_task_comments::Thread {
        project_id: c.env.lineage.project_id,
        task_id: c.env.lineage.task_id,
    };
    let author = aq_task_comments::Author::System {
        component: "test".into(),
    };
    let now = chrono::Utc::now();
    aq_task_comments::add(
        pool,
        Uuid::new_v4(),
        thread,
        &author,
        "Found the release checklist.",
        now,
    )
    .await
    .unwrap();
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let manifest: Value = sqlx::query_scalar("SELECT launch_manifest FROM profiled_launches")
        .fetch_one(pool)
        .await
        .unwrap();
    let refs = manifest["context"]["references"].as_array().unwrap();
    let thread = refs
        .iter()
        .find(|r| {
            r["provenance"]
                .as_str()
                .unwrap()
                .starts_with("aq.task-thread/")
        })
        .unwrap_or_else(|| panic!("no task thread in {refs:?}"));
    assert_eq!(thread["required"], false);
    let bytes = thread["bytes"].as_u64().unwrap() as usize;
    assert!(bytes > 0 && bytes <= aq_task_thread::BUDGET_BYTES);
    // The assignment comes first.
    let order: Vec<&str> = refs
        .iter()
        .map(|r| r["provenance"].as_str().unwrap())
        .collect();
    let task = order
        .iter()
        .position(|p| p.starts_with("aq.authorized-task-context/"))
        .unwrap();
    let thread = order
        .iter()
        .position(|p| p.starts_with("aq.task-thread/"))
        .unwrap();
    assert!(task < thread, "{order:?}");
}

#[tokio::test]
async fn stopped_machine_gets_no_new_offer_but_keeps_cancellations() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let who: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    aq_machine_control::claim_key(&mut conn, c.env.env.node, "desk")
        .await
        .unwrap();
    let now = chrono::Utc::now();
    aq_machine_control::set_desired(
        &mut conn,
        "desk",
        aq_machine_control::DesiredState::Stopped,
        1,
        who,
        Uuid::new_v4(),
        now,
    )
    .await
    .unwrap();
    let offers = |body: Value| {
        let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
        else {
            panic!("{body}")
        };
        page.offers.len()
    };
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offers(body), 0);
    aq_machine_control::set_desired(
        &mut conn,
        "desk",
        aq_machine_control::DesiredState::Running,
        2,
        who,
        Uuid::new_v4(),
        now,
    )
    .await
    .unwrap();
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offers(body), 1);
}

fn poll_offers(body: &Value) -> Vec<aq_admission::startup::NativeOffer> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    page.offers
}

/// Spec 0007 Q1 (Review Focus 7): a successor the predecessor's pinned policy
/// refuses is skipped; the poll answers 200 and offers the next ready task.
#[tokio::test]
async fn refused_successor_is_skipped_and_other_work_is_offered() {
    let mut c = setup().await;
    common::native_offer::pin_policy(&mut c, |p| p.max_attempts = 1).await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let first = poll_offers(&body).remove(0);
    let t1 = c.env.lineage.task_id;
    assert_eq!(first.offered.offer.manifest.bindings.task_id, t1);
    common::native_offer::fail_and_release(&c, first.offered.offer.binding.attempt_id).await;
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = poll(&c).await;
    assert_eq!(
        status, 200,
        "a refused successor never fails the poll: {body}"
    );
    let offers = poll_offers(&body);
    assert_eq!(offers.len(), 1, "{body}");
    assert_eq!(
        offers[0].offered.offer.manifest.bindings.task_id,
        c.second_task
    );
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        poll_offers(&body)
            .iter()
            .all(|o| o.offered.offer.manifest.bindings.task_id != t1)
    );
}

/// A refused poll selection (no fresh availability observation) writes no
/// decision row: the table is capped and polls must not fill it.
#[tokio::test]
async fn refused_poll_selection_records_no_decision() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let decisions = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM intelligence_decisions")
            .fetch_one(pool)
            .await
            .unwrap()
    };
    let before = decisions().await;
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(pool)
        .await
        .unwrap();
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert!(poll_offers_or_none(status, &body), "{status} {body}");
    }
    assert_eq!(decisions().await, before);
}
fn poll_offers_or_none(status: axum::http::StatusCode, body: &Value) -> bool {
    status == 409 || (status == 200 && poll_offers(body).is_empty())
}

/// `swarm.availability.set` through the mounted action route.
async fn set_availability(c: &common::native_offer::Case, available: bool, generation: &str) {
    let id = Uuid::new_v4();
    let body = serde_json::json!({"operation_id":id,"kind":"swarm.availability.set",
        "subject":{"type":"swarm","id":"owner-availability"},"expected_versions":{},
        "payload":{"operation_id":id,"available":available,"expected_generation":generation}});
    let (status, v) = common::selection::call(
        &c.env.env.app,
        "/api/v1/actions",
        Some(body),
        "owner@localhost",
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["data"]["disposition"], "confirmed", "{v}");
}

/// Spec 0007 Q2 (Review Focus 9): with the owner's switch off the poll makes no
/// offer and runs no selection; switching it on restores offers without a
/// manual observation.
#[tokio::test]
async fn owner_availability_off_makes_no_offer_and_no_decision() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let decisions = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM intelligence_decisions")
            .fetch_one(pool)
            .await
            .unwrap()
    };
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(pool)
        .await
        .unwrap();
    set_availability(&c, false, "1").await;
    let before = decisions().await;
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(poll_offers(&body).is_empty(), "{body}");
    }
    assert_eq!(decisions().await, before);
    set_availability(&c, true, "2").await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(poll_offers(&body).len(), 1, "{body}");
}

/// Spec 0007 Q2 (owner request; fix round 1, C1): the switch alone keeps the
/// owner available, with no attestation row at all and no keeper.
#[tokio::test]
async fn owner_availability_on_outlives_the_attestation_window() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    sqlx::query("DELETE FROM intelligence_observations")
        .execute(pool)
        .await
        .unwrap();
    let (status, body) = poll(&c).await;
    assert!(
        poll_offers_or_none(status, &body),
        "unset and no attestation: no offer: {status} {body}"
    );
    set_availability(&c, true, "1").await;
    // The switch was turned on an hour ago and nothing was posted since.
    sqlx::query("UPDATE owner_availability SET updated_at = updated_at - interval '1 hour'")
        .execute(pool)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM intelligence_observations")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows, 0, "the switch writes no observation row");
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(poll_offers(&body).len(), 1, "{body}");
}
