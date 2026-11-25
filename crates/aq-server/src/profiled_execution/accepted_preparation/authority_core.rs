// Actual immutable phase provenance. Never manufactures a legacy plan.
use super::phase::{AcceptRequest, AcceptedReceipt, Proposal};
use super::*;
use crate::{
    execution::{
        authority::{self, Policy, hot::HotState},
        profiled::Status,
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{LaunchBinding, MaterialLease};
use sqlx::{Row, postgres::PgRow};
pub(super) async fn proposal_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<Proposal, AppError> {
    let row = sqlx::query("SELECT * FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE")
        .bind(binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| refused())?;
    let proposal: Proposal = serde_json::from_value(row.get("proposal")).map_err(|_| refused())?;
    proposal.validate().map_err(|_| refused())?;
    if (row.get::<i64, _>("published_at_ms") as u64)
        < proposal.original.offered.clock.server_time_ms
        || (row.get::<i64, _>("published_at_ms") as u64)
            >= proposal.original.offered.clock.expires_at_ms
        || proposal.original.offered.offer.binding != *binding
        || proposal.digest().map_err(|_| refused())?.as_str()
            != row.get::<String, _>("proposal_digest")
        || !matches!(
            binding.session,
            aq_admission::SessionIdentity::Remote { node_id, boot_id, .. }
                if row.get::<uuid::Uuid, _>("node_id") == node_id
                    && row.get::<uuid::Uuid, _>("boot_id") == boot_id
        )
    {
        return Err(refused());
    }
    Ok(proposal)
}
pub(super) async fn current_policy_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    proposal: &Proposal,
) -> Result<(), AppError> {
    let binding = &proposal.original.offered.offer.binding;
    let scope = &proposal.original.offered.offer.manifest.bindings;
    crate::task_workspace_resume::fence_start(tx, scope.project_id, scope.task_id, binding.attempt_id).await?;
    let runtime = super::super::registration::current_runtime_locked(
        state,
        tx,
        auth.id(),
        match binding.session {
            aq_admission::SessionIdentity::Remote { boot_id, .. } => boot_id,
            _ => return Err(refused()),
        },
    )
    .await?;
    let (pin, policy) = policy_for(state, &runtime.runtime, &runtime.qualification)?;
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT policy FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if stored != value(&pin)?
        || value(&proposal.policy)? != value(&policy.phase)?
        || value(&proposal.owners)? != value(&policy.owners)?
    {
        return Err(refused());
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(|| refused())?;
    super::super::policies::fence_configuration_locked(tx, config).await?;
    Ok(())
}
pub(super) async fn accepted_locked(
    tx: &mut Transaction<'_, Postgres>,
    proposal: &Proposal,
    request: &AcceptRequest,
) -> Result<Option<AcceptedReceipt>, AppError> {
    let rows = sqlx::query(
        "SELECT * FROM profiled_v2_accepts WHERE attempt_id=$1 OR (node_id=$2 AND operation_id=$3) ORDER BY attempt_id LIMIT 2 FOR SHARE",
    )
        .bind(request.original.acceptance.binding.attempt_id)
        .bind(request.original.identity.node_id)
        .bind(request.original.operation_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(db)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(refused());
    }
    let row = &rows[0];
    let receipt: AcceptedReceipt =
        serde_json::from_value(row.get("receipt")).map_err(|_| refused())?;
    receipt
        .validate_for(request, proposal)
        .map_err(|_| refused())?;
    if row.get::<serde_json::Value, _>("request") != value(request)?
        || row.get::<String, _>("request_digest")
            != request.digest().map_err(|_| refused())?.as_str()
        || row.get::<String, _>("receipt_digest")
            != receipt.digest().map_err(|_| refused())?.as_str()
        || row.get::<uuid::Uuid, _>("operation_id") != request.original.operation_id
        || row.get::<uuid::Uuid, _>("node_id") != request.original.identity.node_id
        || row.get::<uuid::Uuid, _>("attempt_id") != receipt.lease.binding.attempt_id
        || row.get::<uuid::Uuid, _>("lease_id") != receipt.lease.materials[0].lease_id
        || row.get::<i64, _>("issued_at_ms") as u64 != receipt.lease.issued_at_ms
        || row.get::<i64, _>("expires_at_ms") as u64 != receipt.lease.expires_at_ms
    {
        return Err(refused());
    }
    Ok(Some(receipt))
}
pub(super) async fn live_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    proposal: &Proposal,
    request: &AcceptRequest,
    receipt: &AcceptedReceipt,
) -> Result<(), AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    let binding = &receipt.lease.binding;
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(raw).map_err(|_| refused())?;
    hot.validate_against(binding).map_err(|_| refused())?;
    if !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || view.status != Status::Preparing
        || hot.grant.is_some()
        || hot.preparation_operation_id != Some(request.original.operation_id)
        || view.offer_id != binding.offer_id
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
        || hot.owner.as_ref().is_none_or(|o| {
            o.guardian_id != receipt.lease.guardian_id
                || o.launch_id != receipt.lease.launch_id
                || o.reservation_id != receipt.lease.hold.reservation.reservation_id
        })
    {
        return Err(refused());
    }
    current_policy_locked(state, tx, auth, proposal).await?;
    let reg = super::super::registration::admitted_runtime_locked(
        tx,
        auth.id(),
        request.original.identity.boot_id,
    )
    .await?;
    if reg.request.identity != request.original.identity {
        return Err(refused());
    }
    current_project_locked(state, tx, auth, proposal, view.project_id).await?;
    crate::intelligence::fence_current_registry(state, tx).await?;
    validate_live_registry(state, tx, proposal, receipt).await?;
    crate::intelligence::resolver::fence_preparation_input_principal(state, tx, binding.attempt_id)
        .await?;
    material_locked(tx, request, receipt).await?;
    let time = now(state)?;
    if time < receipt.lease.issued_at_ms || time >= receipt.lease.expires_at_ms {
        return Err(refused());
    }
    Ok(())
}

async fn current_project_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    proposal: &Proposal,
    project_id: uuid::Uuid,
) -> Result<(), AppError> {
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| refused())?;
    if !binding.enabled
        || !binding.eligible_node(&state.config.projects, auth.id())
        || project.get::<i64, _>("revision") as u64 != proposal.spec.project_revision
        || binding.repository_id != proposal.spec.source.repository_id
        || binding.repository_binding_generation
            != proposal.spec.source.repository_binding_generation
        || state
            .config
            .profiled_execution
            .as_ref()
            .and_then(|c| c.projects.iter().find(|x| x.project_id == project_id))
            .is_none_or(|x| !x.enabled)
    {
        return Err(refused());
    }
    Ok(())
}

pub(super) async fn material_locked(
    tx: &mut Transaction<'_, Postgres>,
    request: &AcceptRequest,
    receipt: &AcceptedReceipt,
) -> Result<(), AppError> {
    let actual = sqlx::query(
        "SELECT m.* FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 FOR SHARE OF m,l",
    )
        .bind(receipt.lease.materials[0].lease_id)
        .bind(receipt.lease.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(|| refused())?;
    validate_material_record(
        &actual,
        &receipt.lease.binding,
        request,
        &receipt.lease.materials[0],
    )?;
    if actual.get::<String, _>("state") != "admitted" {
        return Err(refused());
    }
    Ok(())
}

/// Immutable lease body and indices remain required even after live admission ends.
pub(super) fn validate_material_record(
    actual: &PgRow,
    binding: &LaunchBinding,
    request: &AcceptRequest,
    material: &MaterialLease,
) -> Result<(), AppError> {
    if actual.get::<uuid::Uuid, _>("prospective_attempt_id") != binding.attempt_id
        || actual.get::<Option<uuid::Uuid>, _>("admitted_attempt_id") != Some(binding.attempt_id)
        || actual.get::<uuid::Uuid, _>("offer_id") != binding.offer_id
        || actual.get::<uuid::Uuid, _>("node_id") != request.original.identity.node_id
        || actual.get::<uuid::Uuid, _>("boot_id") != request.original.identity.boot_id
        || actual.get::<uuid::Uuid, _>("operation_id") != request.original.operation_id
        || actual.get::<String, _>("request_digest")
            != request.digest().map_err(|_| refused())?.as_str()
    {
        return Err(refused());
    }
    validate_material_contents(actual, material)
}

fn validate_material_contents(actual: &PgRow, material: &MaterialLease) -> Result<(), AppError> {
    if actual.get::<serde_json::Value, _>("lease") != value(material)?
        || actual.get::<String, _>("credential_identity")
            != format!(
                "{}@{}",
                material.credential_profile.id.as_str(),
                material.credential_profile.version.as_str()
            )
        || actual.get::<i64, _>("credential_revision") as u64 != material.credential_revision
        || actual.get::<String, _>("purpose") != material.purpose.as_str()
        || actual.get::<i64, _>("issued_at_ms") as u64 != material.issued_at_ms
        || actual.get::<i64, _>("expires_at_ms") as u64 != material.expires_at_ms
    {
        return Err(refused());
    }
    Ok(())
}
