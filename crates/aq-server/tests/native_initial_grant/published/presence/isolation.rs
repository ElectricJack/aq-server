//! Controlled capacity checks, not throughput or fleet qualification.
use super::*;
use http_body_util::{BodyExt, Full};
use std::{convert::Infallible, time::Duration};
use tokio::sync::oneshot;
fn held_body(bytes: Vec<u8>) -> (Body, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let body = Full::new(axum::body::Bytes::from(bytes)).with_trailers(async move {
        let _ = entered_tx.send(());
        let _ = release_rx.await;
        None::<Result<axum::http::HeaderMap, Infallible>>
    });
    (Body::new(body), entered, release)
}
async fn occupy(
    c: &Prepared,
    path: &str,
    bytes: Vec<u8>,
) -> (
    tokio::task::JoinHandle<axum::response::Response>,
    oneshot::Sender<()>,
) {
    let (body, entered, release) = held_body(bytes);
    let router = aq_server::build_worker_router(c.case.env.env.state.clone());
    let req = Request::post(path)
        .header(
            "authorization",
            format!("Bearer {}", c.case.env.env.credential),
        )
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let job = tokio::spawn(async move { router.oneshot(req).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    (job, release)
}
#[tokio::test]
async fn held_inventory_admission_does_not_consume_report_or_renewal_capacity() {
    let f = setup_presence().await;
    let bytes = serde_json::to_vec(&f.inventory_request).unwrap();
    let (a, ra) = occupy(&f.c, "/v1/worker/profiled/inventory-v2", bytes.clone()).await;
    let (b, rb) = occupy(&f.c, "/v1/worker/profiled/inventory-v2", bytes).await;
    let (status, body) = json_post(
        &f.c,
        "/v1/worker/profiled/inventory-v2",
        &f.inventory_request,
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    ra.send(()).unwrap();
    rb.send(()).unwrap();
    assert_eq!(a.await.unwrap().status(), StatusCode::OK);
    assert_eq!(b.await.unwrap().status(), StatusCode::OK);
}
#[tokio::test]
async fn held_report_admission_preserves_inventory_and_renewal_progress() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bytes = serde_json::to_vec(&f.report).unwrap();
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(occupy(&f.c, "/v1/worker/profiled/material-presence", bytes.clone()).await);
    }
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let mut inventory = f.inventory_request.clone();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence += 1;
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, reply) = renew(&f.c, &f.initial).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    for (job, release) in held {
        release.send(()).unwrap();
        assert_eq!(job.await.unwrap().status(), StatusCode::OK);
    }
    let seq: i64 = sqlx::query_scalar(
        "SELECT sequence FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(f.report.binding.attempt_id)
    .fetch_one(&f.c.case.env.env.pool)
    .await
    .unwrap();
    assert_eq!(seq, 1);
}
#[tokio::test]
async fn occupied_presence_pg_connections_do_not_consume_renewal_pool() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pool = f.c.case.env.env.state.store.presence_pool_for_testing();
    let a = pool.acquire().await.unwrap();
    let b = pool.acquire().await.unwrap();
    let (status, reply) = tokio::time::timeout(Duration::from_secs(2), renew(&f.c, &f.initial))
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    drop((a, b));
}

fn canonical(value: &impl serde::Serialize) -> Vec<u8> {
    let mut value = serde_json::to_value(value).unwrap();
    value.sort_all_objects();
    serde_json::to_vec(&value).unwrap()
}
/// A second relational scope, intentionally NOT a published/runnable offer.
/// Its typed Offered compatibility snapshot has no task claim, sealed launch,
/// hot authority, installation acknowledgment or owner journal. Only storage
/// independence is under test; public placement remains single-slot.
async fn storage_scope(f: &Installed) -> PresenceReport {
    use aq_admission::SessionIdentity;
    let pool = &f.c.case.env.env.pool;
    let state: Value =
        sqlx::query_scalar("SELECT state FROM execution_attempts WHERE attempt_id=$1")
            .bind(f.report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    let mut state: aq_server::execution::profiled::ProfiledState =
        serde_json::from_value(state).unwrap();
    let attempt = Uuid::new_v4();
    let session = Uuid::new_v4();
    let offer = Uuid::new_v4();
    let task = Uuid::new_v4();
    state.offer.binding.attempt_id = attempt;
    state.offer.binding.offer_id = offer;
    let SessionIdentity::Remote {
        attempt_id,
        session_id,
        ..
    } = &mut state.offer.binding.session
    else {
        panic!("remote fixture")
    };
    *attempt_id = attempt;
    *session_id = session;
    state.offer.manifest.bindings.session = state.offer.binding.session.clone();
    state.offer.manifest.bindings.attempt_id = attempt;
    state.offer.manifest.bindings.task_id = task;
    state.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&state.offer.manifest).unwrap();
    state.validate().unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,holds_task,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',FALSE,$6)")
 .bind(attempt).bind(task).bind(f.report.identity.node_id).bind(offer).bind(session).bind(json!(state)).execute(pool).await.unwrap();
    let mut lease = f.c.plan.materials[0].clone();
    lease.lease_id = Uuid::new_v4();
    lease.binding = state.offer.binding.clone();
    lease.validate().unwrap();
    // Pending storage ownership only: no admitted assignment/grant is synthesized.
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,'pending',$13,$14)")
 .bind(lease.lease_id).bind(Uuid::new_v4()).bind(Digest::of(b"storage fixture request").as_str())
 .bind(f.report.identity.node_id).bind(f.report.identity.boot_id).bind(attempt).bind(offer)
 .bind(lease.credential_profile.id.as_str()).bind(lease.credential_revision as i64).bind(lease.purpose.as_str())
 .bind(json!(lease)).bind(Digest::of(Uuid::new_v4().as_bytes()).as_str()).bind(lease.issued_at_ms as i64).bind(lease.expires_at_ms as i64)
 .execute(pool).await.unwrap();
    let mut report = f.report.clone();
    report.operation_id = Uuid::new_v4();
    report.binding = lease.binding.clone();
    report.grant_id = Uuid::new_v4();
    report.guardian_id = Uuid::new_v4();
    report.launch_id = Uuid::new_v4();
    report.reservation_id = Uuid::new_v4();
    report.lease_id = lease.lease_id;
    let Presence::Present { installation, .. } = &mut report.presence else {
        panic!("present fixture")
    };
    installation.delivery.lease = lease.clone();
    installation.operation_id = Uuid::new_v4();
    let receipt = installation.receipt().unwrap();
    report.private_binding = receipt.private_binding.clone();
    report.installation_receipt_digest = aq_selection::digest(&receipt).unwrap();
    report.validate_against(&lease, &receipt).unwrap();
    report
}
async fn retain_storage_scope(f: &Installed, report: &PresenceReport) {
    let receipt = PresenceReceipt {
        schema_version: 1,
        operation_id: report.operation_id,
        identity: report.identity.clone(),
        negotiation: report.negotiation.clone(),
        binding: report.binding.clone(),
        grant_id: report.grant_id,
        lease_id: report.lease_id,
        sequence: report.sequence,
        request_digest: report.digest().unwrap(),
        inventory_at_record: report.inventory.clone(),
        recorded_at_ms: f.initial.original.clock.server_time_ms,
        expires_at_ms: f.initial.original.clock.server_time_ms + 1000,
        disposition: Disposition::Recorded,
    };
    receipt.validate_for(report).unwrap();
    let req = canonical(report);
    let ack = canonical(&receipt);
    assert_eq!(Digest::of(&req), report.digest().unwrap());
    let mut tx = f.c.case.env.env.pool.begin().await.unwrap();
    let epoch:i64=sqlx::query_scalar("SELECT compatibility_epoch FROM profiled_inventory_semantic_heads WHERE node_id=$1 FOR UPDATE")
 .bind(report.identity.node_id).fetch_one(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO profiled_presence_operations(node_id,operation_id,kind,boot_id,attempt_id,grant_id,lease_id,sequence,inventory_revision,compatibility_epoch,snapshot_digest,request_bytes,request_digest,receipt_bytes,receipt_digest,recorded_at_ms,expires_at_ms) VALUES($1,$2,'presence',$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)")
 .bind(report.identity.node_id).bind(report.operation_id).bind(report.identity.boot_id).bind(report.binding.attempt_id).bind(report.grant_id).bind(report.lease_id)
 .bind(report.sequence as i64).bind(report.inventory.revision as i64).bind(epoch).bind(report.inventory.snapshot_digest.as_str()).bind(req).bind(report.digest().unwrap().as_str()).bind(&ack).bind(Digest::of(&ack).as_str()).bind(receipt.recorded_at_ms as i64).bind(receipt.expires_at_ms as i64).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO profiled_material_presence_heads(node_id,attempt_id,lease_id,boot_id,grant_id,operation_id,sequence,compatibility_epoch,revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,1)")
 .bind(report.identity.node_id).bind(report.binding.attempt_id).bind(report.lease_id).bind(report.identity.boot_id).bind(report.grant_id).bind(report.operation_id).bind(report.sequence as i64).bind(epoch)
 .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}
#[tokio::test]
async fn independent_same_node_storage_heads_keep_other_present_after_one_missing() {
    let f = setup_presence().await;
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let other = storage_scope(&f).await;
    retain_storage_scope(&f, &other).await;
    let pool = &f.c.case.env.env.pool;
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM profiled_authority_hot h WHERE attempt_id=$1")
            .bind(f.report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    let journals:Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(j) ORDER BY j.attempt_id),'[]') FROM profiled_owner_journals j").fetch_one(pool).await.unwrap();
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    let mut inventory = f.inventory_request.clone();
    inventory.operation_id = Uuid::new_v4();
    inventory.sequence += 1;
    let (status, body) = json_post(&f.c, "/v1/worker/profiled/inventory-v2", &inventory).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut negative = f.report.clone();
    next_observation(&mut negative);
    negative.presence = Presence::Missing {
        reason: MissingReason::Missing,
    };
    let (status, body) = send_presence(&f.c, &negative).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let heads:Vec<(Uuid,i64,i64,Vec<u8>)>=sqlx::query_as("SELECT h.attempt_id,h.sequence,h.compatibility_epoch,o.request_bytes FROM profiled_material_presence_heads h JOIN profiled_presence_operations o USING(node_id,operation_id) WHERE h.node_id=$1 ORDER BY h.attempt_id")
 .bind(f.report.identity.node_id).fetch_all(pool).await.unwrap();
    assert_eq!(heads.len(), 2);
    let a = heads
        .iter()
        .find(|h| h.0 == f.report.binding.attempt_id)
        .unwrap();
    let b = heads
        .iter()
        .find(|h| h.0 == other.binding.attempt_id)
        .unwrap();
    assert_eq!((a.1, b.1), (2, 1));
    assert_eq!(a.2, b.2);
    assert!(!decode_report(&a.3).unwrap().presence.is_present());
    assert!(decode_report(&b.3).unwrap().presence.is_present());
    let (status, body) = send_presence(&f.c, &f.report).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let other_operation: Uuid = sqlx::query_scalar(
        "SELECT operation_id FROM profiled_material_presence_heads WHERE attempt_id=$1",
    )
    .bind(other.binding.attempt_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(other_operation, other.operation_id);
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(h) FROM profiled_authority_hot h WHERE attempt_id=$1")
            .bind(f.report.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let after_journals:Value=sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(j) ORDER BY j.attempt_id),'[]') FROM profiled_owner_journals j").fetch_one(pool).await.unwrap();
    assert_eq!(journals, after_journals);
    let after_releases: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(releases, after_releases);
    let runnable_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(other.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(runnable_rows, 0);
    let claim: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(other.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(!claim);
}
