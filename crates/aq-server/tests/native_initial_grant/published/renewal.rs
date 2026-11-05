//! Continuing authority on actual assembled publication and retained installation.
use super::*;
use aq_admission::{control::ControlReply, transport::Renew};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;
fn request(c: &Prepared, grant: &InitialGrantDelivery) -> Renew {
    Renew {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: c.request.binding.clone(),
        grant_id: grant.original.grant.grant_id,
        reservation_id: c.request.reservation.reservation_id,
        guardian_id: c.request.guardian_id,
        launch_id: c.request.launch_id,
        journal_revision: c.request.journal_revision + 1,
    }
}
async fn send(c: &Prepared, r: &Renew) -> (StatusCode, ControlReply) {
    let response = aq_server::build_worker_router(c.case.env.env.state.clone())
        .oneshot(
            Request::post("/v1/worker/profiled/renew")
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header("aq-delivery-nonce", Uuid::new_v4().to_string())
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(r).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 65_536).await.unwrap();
    (
        status,
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| panic!("status={status} body={}", String::from_utf8_lossy(&body))),
    )
}
async fn cold_setup() -> Prepared {
    let mut case = native_offer::setup().await;
    let body = json!({"operation_id":Uuid::new_v4(),"boot_id":case.request.identity.boot_id,"sequence":2,"ttl_ms":30_000,"qualifications":case.env.input.node.harnesses.iter().map(|h|h.qualification.clone()).collect::<Vec<_>>(),"credentials":[]});
    let response = native_offer::post(
        &case.env.env.state,
        &case.env.env.credential,
        "/v1/worker/intelligence/inventory",
        serde_json::to_vec(&body).unwrap(),
    )
    .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 65_536).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    case.request.inventory_sequence = 2;
    let (status, body) = native_offer::poll(&case).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = serde_json::from_value(body).unwrap() else {
        panic!("expected publication")
    };
    assert_eq!(page.offers.len(), 1);
    setup_from(case, page.offers.remove(0)).await
}
#[tokio::test]
async fn original_cold_inventory_actual_installation_can_renew_without_retiming_originals() {
    let c = cold_setup().await;
    let initial = grant(&c).await.unwrap();
    let originals:Value=sqlx::query_scalar("SELECT jsonb_build_object('launch',l.graph_task_observation,'decision',d.decision,'initial',r.response) FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id JOIN profiled_execution_receipts r ON r.operation_id=$2 WHERE l.attempt_id=$1").bind(c.request.binding.attempt_id).bind(c.request.operation_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    let r = request(&c, &initial);
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let ControlReply::Renewed(first) = reply else {
        panic!("expected renewed")
    };
    assert_eq!(first.grant_id, initial.original.grant.grant_id);
    assert_eq!(first.execution_deadline_ms, initial.execution_deadline_ms);
    assert!(first.delivery.clock.expires_at_ms > initial.original.clock.expires_at_ms);
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let ControlReply::Renewed(replayed) = reply else {
        panic!("expected renewal replay")
    };
    assert_eq!(first.versions, replayed.versions);
    assert_eq!(
        first.delivery.clock.expires_at_ms,
        replayed.delivery.clock.expires_at_ms
    );
    // Old admission and material retrieval timestamps cannot be retimed.
    c.clock.store(
        initial.original.clock.expires_at_ms as i64 + 1,
        Ordering::SeqCst,
    );
    let mut next = r.clone();
    next.operation_id = Uuid::new_v4();
    next.journal_revision += 1;
    let (status, reply) = send(&c, &next).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let ControlReply::Renewed(continued) = reply else {
        panic!("expected ongoing permit")
    };
    assert_eq!(
        continued.execution_deadline_ms,
        initial.execution_deadline_ms
    );
    assert_eq!(continued.grant_id, initial.original.grant.grant_id);
    let unchanged:Value=sqlx::query_scalar("SELECT jsonb_build_object('launch',l.graph_task_observation,'decision',d.decision,'initial',r.response) FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id JOIN profiled_execution_receipts r ON r.operation_id=$2 WHERE l.attempt_id=$1").bind(c.request.binding.attempt_id).bind(c.request.operation_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(originals, unchanged);
}
#[tokio::test]
async fn newer_unavailable_inventory_cannot_be_rewritten_by_original_installation() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    sqlx::query("UPDATE intelligence_node_snapshots SET revision=revision+1,snapshot=jsonb_set(jsonb_set(snapshot,'{revision}',to_jsonb(revision+1)),'{credential_bindings}',(SELECT jsonb_agg(jsonb_set(b,'{available}','false'::jsonb)) FROM jsonb_array_elements(snapshot->'credential_bindings') b)) WHERE node_id=$1").bind(c.request.identity.node_id).execute(&c.case.env.env.pool).await.unwrap();
    let (status, reply) = send(&c, &request(&c, &initial)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='renew'"
        )
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        0
    );
}
#[tokio::test]
async fn missing_actual_installation_receipt_cannot_be_reconstructed_from_preparation() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    sqlx::query("DELETE FROM admission_material_receipts WHERE lease_id=$1")
        .bind(c.plan.materials[0].lease_id)
        .execute(&c.case.env.env.pool)
        .await
        .unwrap();
    let (status, reply) = send(&c, &request(&c, &initial)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_execution_receipts WHERE kind='renew'"
        )
        .fetch_one(&c.case.env.env.pool)
        .await
        .unwrap(),
        0
    );
}
#[tokio::test]
async fn exact_renewal_operation_cannot_mutate_journal_or_become_superseded_authority() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    let r = request(&c, &initial);
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let mut changed = r.clone();
    changed.journal_revision += 1;
    let (status, reply) = send(&c, &changed).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    // A later authority revision must never be reconstructed by replaying r.
    c.clock.fetch_add(1_000, Ordering::SeqCst);
    let mut next = changed;
    next.operation_id = Uuid::new_v4();
    let (status, reply) = send(&c, &next).await;
    assert_eq!(status, StatusCode::OK, "{reply:?}");
    let (status, reply) = send(&c, &r).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
}
#[tokio::test]
async fn expired_server_permit_cannot_be_resurrected_by_fresh_observations() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    c.clock.store(
        initial.original.clock.expires_at_ms as i64,
        Ordering::SeqCst,
    );
    let (status, reply) = send(&c, &request(&c, &initial)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    let expiry:i64=sqlx::query_scalar("SELECT (state#>>'{grant,expires_at_ms}')::bigint FROM profiled_authority_hot WHERE attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(expiry as u64, initial.original.clock.expires_at_ms);
}
#[tokio::test]
async fn changed_private_material_and_revoked_agent_scope_cannot_extend_a_permit() {
    let c = setup().await;
    let initial = grant(&c).await.unwrap();
    let registry = c.case.env.env.state.config.intelligence.as_ref().unwrap();
    let path = registry
        .config
        .credentials
        .iter()
        .find(|r| r.credential.profile == c.plan.materials[0].credential_profile)
        .unwrap()
        .secret_file
        .as_ref()
        .unwrap();
    let original = std::fs::read(path).unwrap();
    std::fs::write(path, b"changed synthetic private material").unwrap();
    let (status, reply) = send(&c, &request(&c, &initial)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    std::fs::write(path, original).unwrap();
    sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE WHERE attempt_id=$1")
        .bind(c.request.binding.attempt_id)
        .execute(&c.case.env.env.pool)
        .await
        .unwrap();
    let (status, reply) = send(&c, &request(&c, &initial)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{reply:?}");
    let expiry:i64=sqlx::query_scalar("SELECT (state#>>'{grant,expires_at_ms}')::bigint FROM profiled_authority_hot WHERE attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    assert_eq!(expiry as u64, initial.original.clock.expires_at_ms);
}

#[path = "renewal/races.rs"]
mod races;
#[path = "renewal/subscription.rs"]
mod subscription;
