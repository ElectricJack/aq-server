//! Actual server owners with explicitly synthetic qualified worker observations.
use super::*;
#[path = "../../common/presence.rs"]
mod presence_fixture;
use aq_admission::{
    scoped_presence::*,
    transport::{MaterialDelivery, MaterialFetch, Renew},
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use base64::Engine;
use tower::ServiceExt;
struct Installed {
    c: Prepared,
    initial: InitialGrantDelivery,
    report: PresenceReport,
    inventory_request: InventoryV2,
}
async fn json_post(c: &Prepared, path: &str, value: &impl serde::Serialize) -> (StatusCode, Value) {
    let response = crate::common::native_offer::post(
        &c.case.env.env.state,
        &c.case.env.env.credential,
        path,
        serde_json::to_vec(value).unwrap(),
    )
    .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn setup_presence() -> Installed {
    let mut ready = presence_fixture::setup().await;
    let response = crate::common::native_offer::post(
        &ready.c.env.env.state,
        &ready.c.env.env.credential,
        "/v1/worker/profiled/inventory-v2",
        serde_json::to_vec(&ready.request).unwrap(),
    )
    .await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let inventory: InventoryReceipt = serde_json::from_slice(&body).unwrap();
    inventory.validate_for(&ready.request).unwrap();
    ready.c.request.inventory_sequence = ready.request.sequence;
    let (status, body) = native_offer::poll(&ready.c).await;
    assert_eq!(status, 200, "{body}");
    let aq_admission::control::ControlReply::Poll(mut page) = serde_json::from_value(body).unwrap()
    else {
        panic!("expected actual offer")
    };
    assert_eq!(page.offers.len(), 1);
    let mut c = setup_from(ready.c, page.offers.remove(0)).await;
    let fetch = MaterialFetch {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.binding.clone(),
        lease_id: c.plan.materials[0].lease_id,
    };
    let response = crate::common::native_offer::post(
        &c.case.env.env.state,
        &c.case.env.env.credential,
        "/v1/worker/profiled/material",
        serde_json::to_vec(&fetch).unwrap(),
    )
    .await;
    assert_eq!(response.status(), 200);
    let header = response.headers().get("aq-material-delivery").unwrap();
    let delivery: MaterialDelivery = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header.as_bytes())
            .unwrap(),
    )
    .unwrap();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    let observed_digest = Digest::of(&body);
    let observed_bytes = body.len() as u64;
    let record = PrivateInstallationRecord {
        schema_version: 1,
        operation_id: fetch.operation_id,
        preparation_digest: aq_selection::digest(&c.plan).unwrap(),
        worker: c.request.identity.clone(),
        kernel_boot_id: Uuid::new_v4(),
        clock_source: ClockSource::Boottime,
        sent_ns: 1_000_000_000,
        received_ns: 1_001_000_000,
        material_synced_ns: 1_002_000_000,
        retrieval_deadline_ns: delivery
            .clock
            .client_deadline_ns(1_000_000_000, 1_001_000_000)
            .unwrap(),
        delivery,
    };
    c.request.materials = vec![record.receipt().unwrap()];
    let initial = grant(&c).await.unwrap();
    let grant_sent = 2_000_000_000;
    let grant_received = 2_001_000_000;
    let deadline = initial
        .delivery
        .clock
        .client_deadline_ns(grant_sent, grant_received)
        .unwrap();
    let report = PresenceReport {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        runtime: ready.request.runtime.clone(),
        runtime_qualification: ready.request.runtime_qualification.clone(),
        negotiation: ready.request.negotiation.clone(),
        binding: c.request.binding.clone(),
        grant_id: initial.original.grant.grant_id,
        guardian_id: c.request.guardian_id,
        launch_id: c.request.launch_id,
        reservation_id: c.request.reservation.reservation_id,
        lease_id: fetch.lease_id,
        installation_receipt_digest: aq_selection::digest(&c.request.materials[0]).unwrap(),
        private_binding: c.request.materials[0].private_binding.clone(),
        inventory: inventory.inventory.clone(),
        authority: AuthorityAnchor {
            kind: AuthorityKind::InitialGrant,
            operation_id: initial.operation_id,
            grant_revision: initial.versions.grant_revision,
            expires_at_ms: initial.original.grant.expires_at_ms,
        },
        sequence: 1,
        ttl_ms: 30_000,
        observation: Observation {
            kernel_boot_id: record.kernel_boot_id,
            clock_source: ClockSource::Boottime,
            permit_acquired_ns: grant_received,
            permit_deadline_ns: deadline,
            read_started_ns: grant_received + 1_000_000,
            read_completed_ns: grant_received + 2_000_000,
            sent_ns: grant_received + 3_000_000,
        },
        presence: Presence::Present {
            installation: Box::new(record),
            observed_material_digest: observed_digest,
            observed_material_bytes: observed_bytes,
        },
    };
    report
        .validate_against(&c.plan.materials[0], &c.request.materials[0])
        .unwrap();
    Installed {
        c,
        initial,
        report,
        inventory_request: ready.request,
    }
}
async fn send_presence(c: &Prepared, r: &PresenceReport) -> (StatusCode, Value) {
    json_post(c, "/v1/worker/profiled/material-presence", r).await
}
async fn renew(
    c: &Prepared,
    initial: &InitialGrantDelivery,
) -> (StatusCode, aq_admission::control::ControlReply) {
    renew_at(c, initial, c.request.journal_revision + 1).await
}
async fn renew_at(
    c: &Prepared,
    initial: &InitialGrantDelivery,
    journal_revision: u64,
) -> (StatusCode, aq_admission::control::ControlReply) {
    let r = Renew {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.binding.clone(),
        grant_id: initial.original.grant.grant_id,
        reservation_id: c.request.reservation.reservation_id,
        guardian_id: c.request.guardian_id,
        launch_id: c.request.launch_id,
        journal_revision,
    };
    let response = aq_server::build_worker_router(c.case.env.env.state.clone())
        .oneshot(
            Request::post("/v1/worker/profiled/renew")
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header("content-type", "application/json")
                .header("aq-delivery-nonce", Uuid::new_v4().to_string())
                .body(Body::from(serde_json::to_vec(&r).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
#[tokio::test]
async fn scoped_record_is_not_a_grant_and_compatible_new_inventory_preserves_renewal_fact() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, 200, "{body}");
    let receipt: PresenceReceipt = serde_json::from_value(body).unwrap();
    receipt.validate_for(&f.report).unwrap();
    assert!(receipt.expires_at_ms <= f.initial.original.clock.server_time_ms + 30_000);
    let hot: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(f.report.binding.attempt_id)
            .fetch_one(&f.c.case.env.env.pool)
            .await
            .unwrap();
    assert_eq!(hot["stop"]["kind"], "unknown");
    assert_eq!(
        hot["grant"]["expires_at_ms"],
        f.initial.original.grant.expires_at_ms
    );
    let mut inventory = f.inventory_request.clone();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence += 1;
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, 200, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let (status, replay) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(serde_json::to_value(&receipt).unwrap(), replay);
}
fn next_observation(report: &mut PresenceReport) {
    report.operation_id = Uuid::new_v4();
    report.sequence += 1;
    report.observation.read_started_ns = report.observation.sent_ns + 1_000_000;
    report.observation.read_completed_ns = report.observation.read_started_ns + 1_000_000;
    report.observation.sent_ns = report.observation.read_completed_ns + 1_000_000;
}
#[tokio::test]
async fn newer_scoped_negative_survives_renew_race_and_expiry_never_falls_back_to_present() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, 200, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, 200, "{reply:?}");
    let mut negative = f.report.clone();
    next_observation(&mut negative);
    negative.ttl_ms = 1000;
    negative.presence = Presence::Missing {
        reason: MissingReason::Changed,
    };
    // This genuine newer negative still refers to initial revision1, while the
    // same immutable grant has just advanced to revision2.
    let (status, body) = send_presence(&f.c, &negative).await;
    assert_eq!(status, 200, "{body}");
    let receipt: PresenceReceipt = serde_json::from_value(body).unwrap();
    let (status, reply) = renew_at(&f.c, &f.initial, f.c.request.journal_revision + 2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    f.c.clock
        .store(receipt.expires_at_ms as i64 + 1, Ordering::SeqCst);
    let (status, reply) = renew_at(&f.c, &f.initial, f.c.request.journal_revision + 2).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{reply:?}");
    let seq: i64 = sqlx::query_scalar(
        "SELECT sequence FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(f.report.binding.attempt_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(seq, 2);
    let (status, old) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, 200, "{old}");
    let seq: i64 = sqlx::query_scalar(
        "SELECT sequence FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(f.report.binding.attempt_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(seq, 2);
}
#[tokio::test]
async fn legacy_barrier_then_v2_restoration_requires_new_private_observation() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, 200, "{body}");
    let legacy = json!({"operation_id":Uuid::new_v4(),"boot_id":f.report.identity.boot_id,"sequence":3,"ttl_ms":30_000,"qualifications":f.inventory_request.qualifications,"credentials":[]});
    let (status, body) = json_post(&f.c, "/v1/worker/intelligence/inventory", &legacy).await;
    assert_eq!(status, 200, "{body}");
    let mut inventory = f.inventory_request.clone();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence = 4;
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, 200, "{body}");
    let accepted: InventoryReceipt = serde_json::from_value(body).unwrap();
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    let mut fresh = f.report.clone();
    next_observation(&mut fresh);
    fresh.inventory = accepted.inventory;
    let (status, body) = send_presence(&f.c, &fresh).await;
    assert_eq!(status, 200, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, 200, "{reply:?}");
}
#[tokio::test]
async fn scoped_receipt_and_authority_forgery_refuse_without_retaining_a_head() {
    let f = setup_presence().await;
    for mutation in 0..4 {
        let mut report = f.report.clone();
        report.operation_id = Uuid::new_v4();
        match mutation {
            0 => report.installation_receipt_digest = Digest::of(b"not retained"),
            1 => report.authority.operation_id = Uuid::new_v4(),
            2 => report.inventory.snapshot_digest = Digest::of(b"not current"),
            _ => report.lease_id = Uuid::new_v4(),
        }
        let (status, body) = send_presence(&f.c, &report).await;
        assert_ne!(status, StatusCode::OK, "{body}");
    }
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(f.report.binding.attempt_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
#[tokio::test]
async fn expired_permit_cannot_make_a_delayed_positive_fresh_on_receipt() {
    let f = setup_presence().await;
    f.c.clock.store(
        f.initial.original.grant.expires_at_ms as i64,
        Ordering::SeqCst,
    );
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(f.report.binding.attempt_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn scoped_head_failure_rolls_back_receipt_and_exact_retry_can_succeed() {
    let f = setup_presence().await;
    sqlx::raw_sql("CREATE FUNCTION fail_presence_head() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test head rollback'; END $$; CREATE TRIGGER fail_presence_head BEFORE INSERT ON profiled_material_presence_heads FOR EACH ROW EXECUTE FUNCTION fail_presence_head();")
        .execute(&f.c.case.env.env.pool).await.unwrap();
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_ne!(status, StatusCode::OK, "{body}");
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_presence_operations WHERE node_id=$1 AND operation_id=$2",
    )
    .bind(f.report.identity.node_id)
    .bind(f.report.operation_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_presence_head ON profiled_material_presence_heads")
        .execute(&f.c.case.env.env.pool)
        .await
        .unwrap();
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
#[tokio::test]
async fn present_fact_never_bypasses_current_private_backing_or_agent_revocation() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let registry = f.c.case.env.env.state.config.intelligence.as_ref().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|r| r.credential.profile == f.c.plan.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .as_ref()
        .unwrap();
    let original = std::fs::read(path).unwrap();
    std::fs::write(path, b"changed synthetic private material").unwrap();
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    std::fs::write(path, original).unwrap();
    sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE WHERE attempt_id=$1")
        .bind(f.report.binding.attempt_id)
        .execute(&f.c.case.env.env.pool)
        .await
        .unwrap();
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
}
#[tokio::test]
async fn normalized_binding_removal_then_restore_requires_new_scoped_read() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let registry = f.c.case.env.env.state.config.intelligence.as_ref().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|r| r.credential.profile == f.c.plan.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .as_ref()
        .unwrap();
    let original = std::fs::read(path).unwrap();
    std::fs::write(path, b"changed actual backing").unwrap();
    let mut inventory = f.inventory_request.clone();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence += 1;
    inventory.provisionable_only.clear();
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    std::fs::write(path, original).unwrap();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence += 1;
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ack: InventoryReceipt = serde_json::from_value(body).unwrap();
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    let mut report = f.report.clone();
    next_observation(&mut report);
    report.inventory = ack.inventory;
    let (status, body) = send_presence(&f.c, &report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
}

#[path = "presence/isolation.rs"]
mod isolation;
