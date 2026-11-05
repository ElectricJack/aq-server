//! Fresh phase execution owner with separately gated bounded worker transport.
// Shared owner files intentionally instantiate distinct nominal protocol types.
// A module alias would cross the old/new contract boundary instead.
#![allow(clippy::duplicate_mod)]
mod current;
mod delegated_renewal;
pub use delegated_renewal::renew_delegated;
mod facts;
pub(crate) mod history;
mod observations;
mod pinned;
pub(super) mod policy;
pub mod transport;
use super::accepted_preparation as accepted_owner;
use aq_admission::preparation_v2 as phase;
pub use policy::ExecutionQualification;
use sqlx::Row;
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSet {
    CurrentProjectRegistryPrincipalInventoryHealthUnissuedGraphRetentionPrivateV1,
}
const EXECUTION_PREFIX: &str = "aq.phase-execution-qualification/";
#[derive(Clone)]
struct RegistrySnapshot;
include!("phase_execution/owner_core.rs");
include!("phase_execution/provenance.rs");

fn make_provenance(
    p: &pinned::Pinned,
    o: &observations::Observed,
    c: &current::Current,
) -> history::Provenance {
    history::Provenance {
        schema_version: 2,
        qualification: p.qualification.clone(),
        registration_id: p.registration_id,
        project: o.binding.clone(),
        health: c.health.clone(),
        graph: o.graph.clone(),
        retention: o.source.clone(),
        inventory_request_digest: c.inventory_request_digest.clone(),
        inventory_receipt_digest: c.inventory_receipt_digest.clone(),
        inventory_compatibility_epoch: c.compatibility_epoch,
        availability: c.availability.clone(),
        private_digest: o.file.digest.clone(),
        private_bytes: o.file.bytes,
        private_started_at_ms: o.file_started_at_ms,
        private_received_at_ms: o.file_received_at_ms,
    }
}
async fn registry_at_load(
    _: &AppState,
    _: &mut Transaction<'_, Postgres>,
    _: &accepted_owner::AcceptedHistory,
) -> Result<RegistrySnapshot, AppError> {
    Ok(RegistrySnapshot)
}
fn validate_registry_loads(_: &pinned::Pinned, _: &pinned::Pinned) -> Result<(), AppError> {
    Ok(())
}
fn validate_registry_current(
    input: &current::Input<'_>,
    registry: &ArtifactRef,
) -> Result<(), AppError> {
    if *registry == input.proposal.choice.registry {
        Ok(())
    } else {
        Err(refused())
    }
}
fn validate_observation_revisions(
    _: &[aq_selection::AvailabilityObservation],
    _: &ArtifactRef,
) -> Result<(), AppError> {
    Ok(())
}
fn registry_fact(p: &pinned::Pinned, c: &current::Current) -> phase::FactValue {
    let _ = &c.registry_at_preflight;
    phase::FactValue::Registry {
        registry: c.registry.clone(),
        credential_profile: p.input.material.credential_profile.clone(),
        credential_revision: p.input.material.credential_revision,
    }
}
fn validate_provenance_profile(
    _: &history::Provenance,
    _: &phase::ExecutionReceipt,
    _: &accepted_owner::AcceptedHistory,
) -> Result<(), AppError> {
    Ok(())
}
fn terminal_profile(
    c: &super::config::ProfiledConfig,
    runtime: &ArtifactRef,
    qualification: &ArtifactRef,
) -> Result<Option<ArtifactRef>, String> {
    super::phase_terminal::qualification::profile(c, runtime, qualification)
}

async fn validate_registry_replay(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    original: &history::PhaseExecutionHistory,
) -> Result<RegistrySnapshot, AppError> {
    crate::intelligence::fence_current_registry(state, tx).await?;
    let registry = sqlx::query(
        "SELECT revision,config_digest FROM intelligence_registry WHERE singleton FOR SHARE",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if registry.get::<i64, _>("revision").to_string()
        != original.accepted.proposal.choice.registry.version.as_str()
        || registry.get::<String, _>("config_digest")
            != original.accepted.proposal.choice.registry.digest.as_str()
    {
        return Err(refused());
    }
    Ok(RegistrySnapshot)
}

// Both nominal issuers consume the same bounded initial-execution request lane.
pub(super) fn request_permit() -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
    static REQUESTS: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    REQUESTS
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())
}

async fn replay_preflight(
    _: &AppState,
    _: &mut Transaction<'_, Postgres>,
    _: &crate::worker_api::AuthenticatedNode,
    _: &phase::PrepareRequest,
    _: &history::PhaseExecutionHistory,
    _: &RegistrySnapshot,
) -> Result<Option<pinned::Pinned>, AppError> {
    Ok(None)
}

async fn observe_ready(
    state: &AppState,
    graph: &crate::agent_access::Runtime,
    workspace: uuid::Uuid,
) -> Result<
    crate::agent_access::scheduling::ReadyPage,
    crate::agent_access::scheduling::SchedulingReadError,
> {
    graph
        .scheduling_ready_current(workspace, 8, None, state.clock.as_ref())
        .await
}
