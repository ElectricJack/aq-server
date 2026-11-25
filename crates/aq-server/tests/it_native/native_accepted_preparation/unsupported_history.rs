//! Unsupported-origin negatives. The actual accepted history is retained, then a
//! synthetic hot Grant/prepared head is deliberately shaped to exercise old
//! consumers. This is not an issued execution grant or physical host evidence.
use super::*;
use aq_server::{
    execution::{authority::hot::HotState, profiled::Status},
    profiled_execution::{journal, renewal, started},
};
use serde_json::json;
async fn shaped() -> (Case, aq_admission::started::Request) {
    // Functional scope check only. Original publication timestamps stay intact;
    // freezing the subsequent clock does not qualify timing or execution facts.
    let (c, _, accept) = setup_with_functional_clock(true).await;
    let accepted = owner::accept(&c.env.env.state, &auth(&c).await, &accept, Uuid::new_v4())
        .await
        .unwrap();
    let lease = &accepted.receipt.lease;
    let mut request: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    request.operation_id = Uuid::new_v4();
    request.binding = lease.binding.clone();
    request.grant_id = Uuid::new_v4();
    request.guardian_id = lease.guardian_id;
    request.launch_id = lease.launch_id;
    request.reservation_id = lease.hold.reservation.reservation_id;
    let runtime = &c
        .env
        .env
        .state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .qualified_runtimes[0];
    request.runtime = runtime.runtime.clone();
    request.qualification = runtime.qualification.clone();
    request.journal_revision = accept.original.journal_revision + 2;
    request.journal_digest =
        Digest::of(b"synthetic unsupported-origin Started, not an actual host");
    request.observation.guardian.intent.launch_id = request.launch_id;
    request.observation.guardian.intent.session = request.binding.session.clone();
    request.observation.guardian.intent.manifest_digest =
        request.binding.launch_manifest_digest.clone();
    request.observation_digest = request.observation.digest().unwrap();
    request.validate().unwrap();
    retain_shaped_grant(
        &c,
        &request,
        lease.issued_at_ms,
        accept.original.journal_revision + 1,
    )
    .await;
    (c, request)
}
async fn retain_shaped_grant(
    c: &Case,
    request: &aq_admission::started::Request,
    issued_at_ms: u64,
    prepared_revision: u64,
) {
    let mut tx = c.env.env.pool.begin().await.unwrap();
    let raw: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let mut hot: HotState = serde_json::from_value(raw).unwrap();
    hot.status = Status::Granted;
    hot.execution_deadline_ms = Some(issued_at_ms + 30_000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        expires_at_ms: issued_at_ms + 10_000,
    });
    hot.validate_against(&request.binding).unwrap();
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(request.binding.attempt_id)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    journal::record_prepared(
        &mut tx,
        &request.binding,
        hot.owner.as_ref().unwrap(),
        request.grant_id,
        prepared_revision,
        &Digest::of(b"synthetic unsupported-origin Prepared"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
async fn snapshot(c: &Case) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j),'accepted',(SELECT to_jsonb(a) FROM profiled_v2_accepts a),'started',(SELECT count(*) FROM profiled_started_receipts),'legacy',(SELECT count(*) FROM profiled_execution_receipts),'held',(SELECT holds_task FROM execution_attempts))")
        .fetch_one(&c.env.env.pool).await.unwrap()
}
#[tokio::test]
async fn accepted_phase_origin_cannot_enter_legacy_started_through_shaped_hot_grant() {
    let (c, request) = shaped().await;
    let before = snapshot(&c).await;
    assert_eq!(before["started"], 0);
    assert_eq!(before["legacy"], 0);
    assert_eq!(before["held"], true);
    let result = started::record(&c.env.env.state, &auth(&c).await, &request).await;
    assert!(
        result.is_err(),
        "phase origin must not acquire legacy Started: {result:?}"
    );
    assert_eq!(snapshot(&c).await, before);
}
#[tokio::test]
async fn accepted_phase_origin_cannot_acquire_legacy_original_grant_through_renewal() {
    let (c, started) = shaped().await;
    let request = aq_admission::transport::Renew {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: c.request.identity.clone(),
        binding: started.binding,
        grant_id: started.grant_id,
        reservation_id: started.reservation_id,
        guardian_id: started.guardian_id,
        launch_id: started.launch_id,
        journal_revision: started.journal_revision,
    };
    aq_admission::transport::TransportDocument::Renew(request.clone())
        .validate()
        .unwrap();
    let before = snapshot(&c).await;
    assert!(
        renewal::renew(&c.env.env.state, &auth(&c).await, &request, Uuid::new_v4())
            .await
            .is_err()
    );
    assert_eq!(snapshot(&c).await, before);
}
