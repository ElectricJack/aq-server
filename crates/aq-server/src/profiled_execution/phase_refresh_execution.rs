//! Fresh execution owner for the explicitly negotiated registry-refresh profile.
//! The actual owner algorithm is shared with frozen phase execution; four
//! authenticated registry reads remain distinct records, never restamped DTOs.
// Shared owner files intentionally instantiate distinct nominal protocol types.
// A module alias would cross the old/new contract boundary instead.
#![allow(clippy::duplicate_mod)]
#[path = "phase_execution/current.rs"]
mod current;
#[path = "phase_execution/delegated_renewal.rs"]
mod delegated_renewal;
pub use delegated_renewal::renew_delegated;
#[path = "phase_execution/facts.rs"]
mod facts;
#[path = "phase_execution/history.rs"]
pub(crate) mod history;
#[path = "phase_execution/observations.rs"]
mod observations;
#[path = "phase_execution/pinned.rs"]
mod pinned;
#[path = "phase_execution/policy.rs"]
pub(super) mod policy;
use super::accepted_refresh as accepted_owner;
use aq_admission::preparation_refresh_v1 as phase;
pub use policy::ExecutionQualification;
const EXECUTION_PREFIX: &str = "aq.phase-refresh-execution-qualification/";
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSet {
    RegistryObservationContinuityCurrentProjectPrincipalInventoryHealthUnissuedGraphRetentionPrivateV1,
}
#[derive(Clone)]
struct RegistrySnapshot {
    accepted: ArtifactRef,
    observed: ArtifactRef,
}
include!("phase_execution/owner_core.rs");
include!("phase_refresh_execution/provenance.rs");

async fn registry_at_load(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    accepted: &accepted_owner::AcceptedHistory,
) -> Result<RegistrySnapshot, AppError> {
    let observed = accepted_owner::read_registry_locked(state, tx).await?;
    phase::validate_registry_chain(
        &accepted.proposal.choice.registry,
        &accepted.receipt.lease.registry_at_accept,
        &observed,
        &observed,
    )
    .map_err(|_| refused())?;
    Ok(RegistrySnapshot {
        accepted: accepted.receipt.lease.registry_at_accept.clone(),
        observed,
    })
}

fn validate_registry_loads(
    before: &pinned::Pinned,
    after: &pinned::Pinned,
) -> Result<(), AppError> {
    if before.registry_at_load.accepted != after.registry_at_load.accepted {
        return Err(refused());
    }
    phase::validate_registry_chain(
        &after.history.proposal.choice.registry,
        &after.history.receipt.lease.registry_at_accept,
        &before.registry_at_load.observed,
        &after.registry_at_load.observed,
    )
    .map_err(|_| refused())
}

fn validate_registry_current(
    input: &current::Input<'_>,
    registry: &ArtifactRef,
) -> Result<(), AppError> {
    phase::validate_registry_chain(
        &input.proposal.choice.registry,
        &input.registry_at_preflight.accepted,
        &input.registry_at_preflight.observed,
        registry,
    )
    .map_err(|_| refused())
}

fn validate_observation_revisions(
    observations: &[aq_selection::AvailabilityObservation],
    registry: &ArtifactRef,
) -> Result<(), AppError> {
    let revision = phase::registry_revision(registry).map_err(|_| refused())?;
    if observations.iter().any(|o| o.revision > revision) {
        return Err(refused());
    }
    Ok(())
}

fn registry_fact(p: &pinned::Pinned, c: &current::Current) -> phase::FactValue {
    phase::FactValue::Registry {
        selected_registry: p.history.proposal.choice.registry.clone(),
        accepted_registry: p.history.receipt.lease.registry_at_accept.clone(),
        preflight_registry: c.registry_at_preflight.observed.clone(),
        current_registry: c.registry.clone(),
        credential_profile: p.input.material.credential_profile.clone(),
        credential_revision: p.input.material.credential_revision,
    }
}

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
        selected_registry: p.history.proposal.choice.registry.clone(),
        accepted_registry: p.history.receipt.lease.registry_at_accept.clone(),
        preflight_registry: c.registry_at_preflight.observed.clone(),
        current_registry: c.registry.clone(),
    }
}

fn validate_provenance_profile(
    p: &history::Provenance,
    receipt: &phase::ExecutionReceipt,
    accepted: &accepted_owner::AcceptedHistory,
) -> Result<(), AppError> {
    phase::validate_registry_chain(
        &p.selected_registry,
        &p.accepted_registry,
        &p.preflight_registry,
        &p.current_registry,
    )
    .map_err(|_| refused())?;
    if p.selected_registry != accepted.proposal.choice.registry
        || p.accepted_registry != accepted.receipt.lease.registry_at_accept
    {
        return Err(refused());
    }
    let fact = receipt
        .confirmation
        .facts
        .iter()
        .find_map(|f| match &f.body.value {
            phase::FactValue::Registry {
                selected_registry,
                accepted_registry,
                preflight_registry,
                current_registry,
                ..
            } => Some((
                selected_registry,
                accepted_registry,
                preflight_registry,
                current_registry,
            )),
            _ => None,
        })
        .ok_or_else(refused)?;
    if fact
        != (
            &p.selected_registry,
            &p.accepted_registry,
            &p.preflight_registry,
            &p.current_registry,
        )
    {
        return Err(refused());
    }
    Ok(())
}

fn terminal_profile(
    config: &super::config::ProfiledConfig,
    runtime: &ArtifactRef,
    qualification: &ArtifactRef,
) -> Result<Option<ArtifactRef>, String> {
    super::phase_refresh_terminal::qualification::profile(config, runtime, qualification)
}

async fn validate_registry_replay(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    original: &history::PhaseExecutionHistory,
) -> Result<RegistrySnapshot, AppError> {
    validate_provenance_profile(&original.provenance, &original.receipt, &original.accepted)?;
    let observed = accepted_owner::read_registry_locked(state, tx).await?;
    // Current authority may advance since issue. Its immutable config must not
    // change and no replay may roll back below the actual original issue read.
    phase::validate_registry_chain(
        &original.provenance.selected_registry,
        &original.provenance.accepted_registry,
        &original.provenance.current_registry,
        &observed,
    )
    .map_err(|_| refused())?;
    Ok(RegistrySnapshot {
        accepted: original.provenance.accepted_registry.clone(),
        observed,
    })
}

fn validate_replay_floor(
    before: &RegistrySnapshot,
    after: &RegistrySnapshot,
) -> Result<(), AppError> {
    phase::validate_registry_chain(
        &before.accepted,
        &after.accepted,
        &before.observed,
        &after.observed,
    )
    .map_err(|_| refused())
}

async fn replay_preflight(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &crate::worker_api::AuthenticatedNode,
    q: &phase::PrepareRequest,
    original: &history::PhaseExecutionHistory,
    registry: &RegistrySnapshot,
) -> Result<Option<pinned::Pinned>, AppError> {
    let pinned = Box::pin(pinned::load_replay_locked(state, tx, auth, q, original)).await?;
    validate_replay_floor(registry, &pinned.registry_at_load)?;
    Ok(Some(pinned))
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
