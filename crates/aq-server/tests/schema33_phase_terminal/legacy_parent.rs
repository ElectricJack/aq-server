//! Actual legacy publication→Accept→Grant→closure owner, synthetic suppression.
//! Current33 parent integrity only; prior actual32→33 legacy preservation is
//! retained separately. Never a surrogate for phase authority.
use super::common;
use aq_admission::{
    Digest,
    granted_closure::{Evidence, JournalCheckpoint, Request, SuppressionRecord},
};
use aq_server::profiled_execution::closure::{
    policy::{EvidenceKind, HistoricalVerifierPolicy, InstalledPolicy, Verifier},
    service,
};
use common::closure::{self, Prepared};
use std::sync::Arc;
use uuid::Uuid;
async fn request(c: &Prepared) -> Request {
    let granted = closure::grant(c).await.unwrap().original;
    let row:(serde_json::Value,)=sqlx::query_as("SELECT convert_from(metadata_bytes,'UTF8')::JSONB FROM profiled_native_registration_history h JOIN profiled_launch_registrations l USING(registration_id) WHERE l.attempt_id=$1").bind(c.request.binding.attempt_id).fetch_one(&c.case.env.env.pool).await.unwrap();
    let metadata: aq_server::profiled_execution::registration::NativeRegistration =
        serde_json::from_value(row.0).unwrap();
    let operation_id = Uuid::new_v4();
    let previous = JournalCheckpoint {
        revision: c.request.journal_revision,
        digest: c.request.journal_digest.clone(),
    };
    let record = SuppressionRecord {
        schema_version: 1,
        operation_id,
        suppression_id: Uuid::new_v4(),
        binding: c.request.binding.clone(),
        grant_id: granted.grant.grant_id,
        guardian_id: granted.guardian_id,
        launch_id: granted.launch_id,
        reservation_id: granted.reservation_id,
        original_kernel_boot_id: Uuid::new_v4(),
        recording_kernel_boot_id: Uuid::new_v4(),
        previous,
        terminal_revision: c.request.journal_revision + 1,
    };
    let request = Request {
        schema_version: 1,
        operation_id,
        binding: record.binding.clone(),
        grant_id: record.grant_id,
        guardian_id: record.guardian_id,
        launch_id: record.launch_id,
        reservation_id: record.reservation_id,
        runtime: metadata.request.runtime,
        qualification: metadata.qualification,
        journal: JournalCheckpoint {
            revision: record.terminal_revision,
            digest: Digest::of(b"synthetic terminal journal; not physical evidence"),
        },
        evidence: Evidence::Suppressed {
            record_digest: record.digest().unwrap(),
            record,
        },
    };
    request.validate().unwrap();
    request
}
async fn policy(c: &Prepared, r: &Request, revision: u64) -> Arc<InstalledPolicy> {
    HistoricalVerifierPolicy::new(
        revision,
        vec![Verifier {
            runtime: r.runtime.clone(),
            qualification: r.qualification.clone(),
            profile: common::profiled::artifact(
                "synthetic-recovery-profile",
                b"test-only opaque proof policy",
            ),
            kinds: vec![EvidenceKind::Suppressed, EvidenceKind::HostStopped],
        }],
    )
    .unwrap()
    .install(&c.case.env.env.state)
    .await
    .unwrap()
}

#[tokio::test]
async fn actual_current33_legacy_closure_retains_parent_against_delete_and_key_update() {
    let c = Box::pin(closure::setup()).await;
    let request = request(&c).await;
    let policy = policy(&c, &request, 1).await;
    let receipt = service::record(
        &c.case.env.env.state,
        &policy,
        &closure::auth(&c).await,
        &request,
    )
    .await
    .unwrap();
    receipt.validate_for(&request).unwrap();
    let pool = &c.case.env.env.pool;
    let before:Vec<u8>=sqlx::query_scalar("SELECT convert_to(to_jsonb(r)::text,'UTF8') FROM profiled_execution_receipts r WHERE node_id=$1 AND operation_id=$2").bind(c.request.identity.node_id).bind(c.request.operation_id).fetch_one(pool).await.unwrap();
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(version, aq_server::STATE_FORMAT_VERSION.to_string());
    assert!(aq_server::STATE_FORMAT_VERSION >= 33);
    let keys:(Option<Uuid>,Option<Uuid>)=sqlx::query_as("SELECT legacy_origin_operation_id,phase_origin_operation_id FROM profiled_launch_closures WHERE attempt_id=$1").bind(request.binding.attempt_id).fetch_one(pool).await.unwrap();
    assert_eq!(keys, (Some(c.request.operation_id), None));
    for statement in [
        "DELETE FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2",
        "UPDATE profiled_execution_receipts SET operation_id=gen_random_uuid() WHERE node_id=$1 AND operation_id=$2",
    ] {
        let error = sqlx::query(statement)
            .bind(c.request.identity.node_id)
            .bind(c.request.operation_id)
            .execute(pool)
            .await
            .unwrap_err();
        let error = error.as_database_error().unwrap();
        assert_eq!(error.code().as_deref(), Some("23503"));
        assert_eq!(
            error.constraint(),
            Some("profiled_launch_closures_legacy_origin_fk")
        );
    }
    let after:Vec<u8>=sqlx::query_scalar("SELECT convert_to(to_jsonb(r)::text,'UTF8') FROM profiled_execution_receipts r WHERE node_id=$1 AND operation_id=$2").bind(c.request.identity.node_id).bind(c.request.operation_id).fetch_one(pool).await.unwrap();
    assert_eq!(before, after);
    let retained: Vec<u8> = sqlx::query_scalar(
        "SELECT receipt_bytes FROM profiled_launch_closures WHERE attempt_id=$1",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&retained).unwrap(),
        serde_json::to_value(receipt).unwrap()
    );
}
