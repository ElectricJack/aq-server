//! Actual phase publication/acceptance/issuance, explicitly synthetic proof producer.
//! No physical host, model/provider, or remote proof qualification is claimed.
#[allow(dead_code)]
#[path = "phase_execution.rs"]
mod phase_fixture;
use aq_admission::{ArtifactKind, ArtifactRef, Digest, Text, preparation_v2::ExecutionReceipt};
use aq_phase_terminal_protocol as wire;
use aq_server::profiled_execution::{
    config::ProfiledConfig, definitions::Definition, phase_execution,
    phase_terminal::qualification::TerminalQualification,
};
pub use phase_fixture::Prepared;
use uuid::Uuid;
fn install(c: &mut ProfiledConfig) {
    let runtime = c.qualified_runtimes[0].clone();
    let proof =
        "Explicit test-only synthetic host/proof producer; no physical qualification".to_owned();
    let proof_ref = ArtifactRef {
        id: Text::new("aq.synthetic-phase-terminal-proof/test").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(proof.as_bytes()),
    };
    c.definition_bodies
        .push(Definition::new(proof_ref.clone(), ArtifactKind::Definition, proof).unwrap());
    let q = TerminalQualification {
        schema_version: 1,
        contract: wire::contract_artifact().unwrap(),
        runtime: runtime.runtime,
        runtime_qualification: runtime.qualification,
        proof_producer: proof_ref,
    };
    let body = serde_json::to_string(&q).unwrap();
    let reference = ArtifactRef {
        id: Text::new("aq.phase-terminal-qualification/test").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(body.as_bytes()),
    };
    c.definition_bodies
        .push(Definition::new(reference, ArtifactKind::Definition, body).unwrap());
}
pub fn prepared_terminal() -> impl std::future::Future<Output = Prepared> {
    phase_fixture::prepared_with_config(true, install)
}
pub async fn auth(f: &Prepared) -> aq_server::worker_api::AuthenticatedNode {
    phase_fixture::auth(&f.c).await
}
/// Actual phase issuer; StageA currently uses its exact unregistered SQL only
/// after real32 history exists. Registered33 fixtures skip the manual upgrade.
pub async fn issue(f: &Prepared) -> ExecutionReceipt {
    let delivery = phase_execution::prepare(
        &f.c.env.env.state,
        &auth(f).await,
        &f.prepared,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let pool = &f.c.env.env.pool;
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(pool)
            .await
            .unwrap();
    if version.parse::<u32>().unwrap() < 33 {
        assert_eq!(version, "32");
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(include_str!("../../migrations/0033_phase_terminal.sql"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO schema_migrations(version) VALUES(33)")
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    delivery.receipt
}
pub fn checkpoint(f: &Prepared) -> aq_admission::granted_closure::JournalCheckpoint {
    aq_admission::granted_closure::JournalCheckpoint {
        revision: f.prepared.original.journal_revision,
        digest: f.prepared.original.journal_digest.clone(),
    }
}
pub fn context<'a>(
    f: &'a Prepared,
    execution: &'a ExecutionReceipt,
    prepared: &'a aq_admission::granted_closure::JournalCheckpoint,
) -> wire::Context<'a> {
    let runtime =
        &f.c.env
            .env
            .state
            .config
            .profiled_execution
            .as_ref()
            .unwrap()
            .qualified_runtimes[0];
    wire::Context {
        execution,
        prepare: &f.prepared,
        accepted: &f.a,
        acceptance: &f.q,
        proposal: &f.p,
        runtime: &runtime.runtime,
        runtime_qualification: &runtime.qualification,
        prepared,
    }
}
/// Frozen Running DTO template is synthetic evidence only. All protocol scope
/// and ownership are taken from actual phase records, never a legacy grant.
pub fn started_request(f: &Prepared, execution: &ExecutionReceipt) -> wire::StartedRequest {
    let mut original: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    let checkpoint = checkpoint(f);
    let c = context(f, execution, &checkpoint);
    original.operation_id = Uuid::new_v4();
    original.binding = execution.grant.binding.clone();
    original.grant_id = execution.grant.grant_id;
    original.guardian_id = f.a.lease.guardian_id;
    original.launch_id = f.a.lease.launch_id;
    original.reservation_id = f.a.lease.hold.reservation.reservation_id;
    original.runtime = c.runtime.clone();
    original.qualification = c.runtime_qualification.clone();
    original.journal_revision = checkpoint.revision + 1;
    original.journal_digest = Digest::of(b"explicit synthetic phase Started journal snapshot");
    original.observation.guardian.intent.session = original.binding.session.clone();
    original.observation.guardian.intent.manifest_digest =
        original.binding.launch_manifest_digest.clone();
    original.observation.guardian.intent.launch_id = original.launch_id;
    original.observation_digest = original.observation.digest().unwrap();
    let request = wire::StartedRequest {
        schema_version: 1,
        negotiation: wire::Negotiation::current().unwrap(),
        origin: wire::Origin::for_context(&c).unwrap(),
        original,
    };
    request.validate_for(&c).unwrap();
    request
}
