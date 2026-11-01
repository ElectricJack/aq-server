//! Actual phase issuer and SQL closure owner. Proof templates are explicitly
//! synthetic: these tests qualify the consumer, not physical worker production.
mod common;
#[allow(dead_code)]
#[path = "common/phase_execution.rs"]
mod fixture;
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, Text, granted_closure as original,
    preparation_v2::ExecutionReceipt,
};
use aq_phase_terminal_protocol as wire;
use aq_server::profiled_execution::{
    config::ProfiledConfig,
    definitions::Definition,
    phase_execution,
    phase_terminal::{closure, qualification::TerminalQualification},
    policies,
};
use fixture::{Prepared, auth};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;
const SQL: &str = include_str!("../migrations/0033_phase_terminal.sql");

fn qualify(c: &mut ProfiledConfig) {
    let r = &c.qualified_runtimes[0];
    let body = serde_json::to_string(&TerminalQualification {
        schema_version: 1,
        contract: wire::contract_artifact().unwrap(),
        runtime: r.runtime.clone(),
        runtime_qualification: r.qualification.clone(),
        proof_producer: fixture::reference("synthetic-owner/no-start-proof-not-physical"),
    })
    .unwrap();
    c.definition_bodies.push(
        Definition::new(
            ArtifactRef {
                id: Text::new("aq.phase-terminal-qualification/closure-test").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(body.as_bytes()),
            },
            ArtifactKind::Definition,
            body,
        )
        .unwrap(),
    );
}
async fn setup(kind: usize) -> (Prepared, ExecutionReceipt, wire::ClosureRequest) {
    let f = aq_server::test_support::with_state32_store(Box::pin(fixture::prepared_with_config(
        true, qualify,
    )))
    .await;
    let mut tx = f.c.env.env.pool.begin().await.unwrap();
    sqlx::raw_sql(SQL).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO schema_migrations(version) VALUES(33)")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let execution = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(&f.c).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap()
    .receipt;
    let q = synthetic_proof(&f, &execution, kind);
    (f, execution, q)
}
fn synthetic_proof(
    f: &Prepared,
    execution: &ExecutionReceipt,
    kind: usize,
) -> wire::ClosureRequest {
    let templates: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aq-phase-terminal-protocol/artifacts/fixtures.json"
    )))
    .unwrap();
    let mut q: wire::ClosureRequest =
        serde_json::from_value(templates["closure_requests"][kind].clone()).unwrap();
    let runtime =
        &f.c.env
            .env
            .state
            .config
            .profiled_execution
            .as_ref()
            .unwrap()
            .qualified_runtimes[0];
    let prepared = original::JournalCheckpoint {
        revision: f.prepared.original.journal_revision,
        digest: f.prepared.original.journal_digest.clone(),
    };
    let context = wire::Context {
        execution,
        prepare: &f.prepared,
        accepted: &f.a,
        acceptance: &f.q,
        proposal: &f.p,
        runtime: &runtime.runtime,
        runtime_qualification: &runtime.qualification,
        prepared: &prepared,
    };
    q.origin = wire::Origin::for_context(&context).unwrap();
    let r = &mut q.original;
    r.operation_id = Uuid::new_v4();
    r.binding = execution.grant.binding.clone();
    r.grant_id = execution.grant.grant_id;
    r.guardian_id = f.a.lease.guardian_id;
    r.launch_id = f.a.lease.launch_id;
    r.reservation_id = f.a.lease.hold.reservation.reservation_id;
    r.runtime = runtime.runtime.clone();
    r.qualification = runtime.qualification.clone();
    r.journal = original::JournalCheckpoint {
        revision: prepared.revision + 2,
        digest: Digest::of(b"synthetic terminal journal, not physical persistence"),
    };
    match &mut r.evidence {
        original::Evidence::Suppressed {
            record,
            record_digest,
        } => {
            record.operation_id = r.operation_id;
            record.suppression_id = Uuid::new_v4();
            record.binding = r.binding.clone();
            record.grant_id = r.grant_id;
            record.guardian_id = r.guardian_id;
            record.launch_id = r.launch_id;
            record.reservation_id = r.reservation_id;
            record.previous = prepared.clone();
            record.terminal_revision = r.journal.revision;
            *record_digest = record.digest().unwrap();
        }
        original::Evidence::HostStopped {
            driver_intent,
            proof,
            proof_digest,
        } => {
            driver_intent.binding = r.binding.clone();
            driver_intent.grant_id = r.grant_id;
            driver_intent.guardian_id = r.guardian_id;
            driver_intent.reservation_id = r.reservation_id;
            driver_intent.host.launch_id = r.launch_id;
            driver_intent.host.session = r.binding.session.clone();
            driver_intent.host.manifest_digest = r.binding.launch_manifest_digest.clone();
            driver_intent.journal = original::JournalCheckpoint {
                revision: prepared.revision + 1,
                digest: Digest::of(b"synthetic driver checkpoint, not physical persistence"),
            };
            proof.observation.guardian.intent = driver_intent.host.clone();
            *proof_digest = proof.digest().unwrap();
        }
    }
    q.validate_for(&context).unwrap();
    q
}
async fn snapshot(f: &Prepared) -> Value {
    sqlx::query_scalar("SELECT jsonb_build_object('hot',(SELECT to_jsonb(h) FROM profiled_authority_hot h LIMIT 1),'attempt',(SELECT to_jsonb(a) FROM execution_attempts a LIMIT 1),'journal',(SELECT to_jsonb(j) FROM profiled_owner_journals j LIMIT 1),'leases',(SELECT jsonb_agg(to_jsonb(l) ORDER BY lease_id) FROM admission_material_leases l),'bindings',(SELECT jsonb_agg(to_jsonb(b) ORDER BY attempt_id) FROM agent_launch_bindings b),'closures',(SELECT count(*) FROM profiled_launch_closures),'capacity',(SELECT jsonb_agg(to_jsonb(c) ORDER BY attempt_id) FROM profiled_phase_terminal_capacity c))")
        .fetch_one(&f.c.env.env.pool).await.unwrap()
}
async fn assert_closed_held(f: &Prepared, q: &wire::ClosureRequest, ack: &wire::ClosureReceipt) {
    ack.validate_for(q).unwrap();
    let state = snapshot(f).await;
    assert_eq!(state["attempt"]["holds_task"], true);
    assert_eq!(state["hot"]["state"]["revoked"], true);
    assert_eq!(state["hot"]["state"]["status"], "cancelling");
    assert_eq!(state["hot"]["state"]["stop"]["kind"], "unknown");
    assert_eq!(state["journal"]["phase"], "prepared");
    assert_eq!(state["closures"], 1);
    let counts:(i64,i64,i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_started_receipts),(SELECT count(*) FROM profiled_stopped_receipts),(SELECT count(*) FROM profiled_result_reports),(SELECT count(*) FROM profiled_verified_releases),(SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare')")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0, 0, 0));
    let row = sqlx::query("SELECT initial_authority_kind,initial_authority_receipt_digest,phase_request_digest,phase_envelope_bytes,phase_ack_bytes,context_bytes,legacy_origin_operation_id,phase_origin_operation_id FROM profiled_launch_closures")
        .fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(
        row.get::<String, _>("initial_authority_kind"),
        "phase_execution_v2"
    );
    assert_eq!(
        row.get::<String, _>("initial_authority_receipt_digest"),
        q.origin.execution_receipt_digest.as_str()
    );
    assert_eq!(
        row.get::<String, _>("phase_request_digest"),
        q.digest().unwrap().as_str()
    );
    let retained: wire::ClosureRequest =
        serde_json::from_slice(&row.get::<Vec<u8>, _>("phase_envelope_bytes")).unwrap();
    assert_eq!(retained.digest().unwrap(), q.digest().unwrap());
    let retained =
        wire::decode_closure_receipt(&row.get::<Vec<u8>, _>("phase_ack_bytes"), q).unwrap();
    assert_eq!(
        serde_json::to_value(retained).unwrap(),
        serde_json::to_value(ack).unwrap()
    );
    let context: Value = serde_json::from_slice(&row.get::<Vec<u8>, _>("context_bytes")).unwrap();
    assert_eq!(context["schema_version"], 2);
    assert_eq!(context["origin"], json!(q.origin));
    assert!(
        context.get("grant").is_none(),
        "no fabricated legacy grant context"
    );
    assert!(
        row.get::<Option<Uuid>, _>("legacy_origin_operation_id")
            .is_none()
    );
    assert_eq!(
        row.get::<Uuid, _>("phase_origin_operation_id"),
        f.prepared.original.operation_id
    );
}

#[tokio::test]
async fn suppressed_phase_closure_after_expiry_and_retirement_is_historical_and_held() {
    let (mut f, execution, q) = setup(0).await;
    let node = auth(&f.c).await;
    let future = execution.execution_deadline_ms + 1;
    f.c.env.env.state.clock = std::sync::Arc::new(move || {
        chrono::DateTime::from_timestamp_millis(future as i64).unwrap()
    });
    let mut config = (*f.c.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    p.qualified_runtimes.clear();
    p.context_qualifications.clear();
    p.definition_bodies.retain(|d| {
        !d.artifact()
            .id
            .as_str()
            .starts_with("aq.accepted-preparation-policy/")
            && !d
                .artifact()
                .id
                .as_str()
                .starts_with("aq.phase-execution-qualification/")
    });
    p.revision += 1;
    p.validate().unwrap();
    config.profiled_execution = Some(std::sync::Arc::new(p));
    f.c.env.env.state.config = std::sync::Arc::new(config);
    policies::install(&f.c.env.env.state).await.unwrap();
    let ack = closure::record(&f.c.env.env.state, &node, &q)
        .await
        .unwrap();
    assert_closed_held(&f, &q, &ack).await;
    let before = snapshot(&f).await;
    let replay = closure::record(&f.c.env.env.state, &node, &q)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(ack).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(snapshot(&f).await, before);
}

#[tokio::test]
async fn pre_started_host_stop_can_omit_pane_and_root_status_without_inventing_started() {
    let (f, _, q) = setup(3).await;
    let ack = closure::record(&f.c.env.env.state, &auth(&f.c).await, &q)
        .await
        .unwrap();
    assert_closed_held(&f, &q, &ack).await;
}

#[tokio::test]
async fn wrong_origin_checkpoint_or_current_credential_cannot_change_phase_authority() {
    let (f, _, q) = setup(0).await;
    let node = auth(&f.c).await;
    let before = snapshot(&f).await;
    let mut wrong = q.clone();
    wrong.origin.accepted_receipt_digest = Digest::of(b"other accepted origin");
    assert!(
        closure::record(&f.c.env.env.state, &node, &wrong)
            .await
            .is_err()
    );
    wrong = q.clone();
    if let original::Evidence::Suppressed {
        record,
        record_digest,
    } = &mut wrong.original.evidence
    {
        record.previous.digest = Digest::of(b"other Prepared checkpoint");
        *record_digest = record.digest().unwrap();
    }
    wrong.validate().unwrap();
    assert!(
        closure::record(&f.c.env.env.state, &node, &wrong)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(node.id())
        .bind("0".repeat(64))
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    assert!(
        closure::record(&f.c.env.env.state, &node, &q)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
}

#[tokio::test]
async fn failed_closure_insert_rolls_back_permission_revocation_and_retains_retry_identity() {
    let (f, _, q) = setup(0).await;
    let node = auth(&f.c).await;
    let before = snapshot(&f).await;
    sqlx::raw_sql("CREATE FUNCTION reject_test_closure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test closure fault'; END $$; CREATE TRIGGER zz_test_closure_fault BEFORE INSERT ON profiled_launch_closures FOR EACH ROW EXECUTE FUNCTION reject_test_closure();")
        .execute(&f.c.env.env.pool).await.unwrap();
    assert!(
        closure::record(&f.c.env.env.state, &node, &q)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
    sqlx::query("DROP TRIGGER zz_test_closure_fault ON profiled_launch_closures")
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    let ack = closure::record(&f.c.env.env.state, &node, &q)
        .await
        .unwrap();
    assert_closed_held(&f, &q, &ack).await;
    let before = snapshot(&f).await;
    let mut changed = q.clone();
    changed.original.operation_id = Uuid::new_v4();
    if let original::Evidence::Suppressed {
        record,
        record_digest,
    } = &mut changed.original.evidence
    {
        record.operation_id = changed.original.operation_id;
        *record_digest = record.digest().unwrap();
    }
    changed.validate().unwrap();
    assert!(
        closure::record(&f.c.env.env.state, &node, &changed)
            .await
            .is_err()
    );
    assert_eq!(snapshot(&f).await, before);
}
