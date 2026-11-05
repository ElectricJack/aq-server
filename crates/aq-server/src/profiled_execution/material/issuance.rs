//! Private-file observation for a prospective preparation lease. The caller
//! must fence current registry/assignment/configuration after this external I/O.
use super::{MaterialError, source_or_configured};
use crate::intelligence::config::LoadedRegistry;
use crate::profiled_execution::login_refresh::HeadSource;
use aq_admission::{Digest, Label, LaunchBinding, MaterialLease, SessionIdentity};
use aq_selection::{ExecutionTarget, ResolvedSelection};
use uuid::Uuid;

pub fn issue_for_selection(
    registry: &LoadedRegistry,
    selection: &ResolvedSelection,
    binding: &LaunchBinding,
    lease_id: Uuid,
    issued_at_ms: u64,
    expires_at_ms: u64,
    backend_kind: Option<aq_selection::BackendKind>,
) -> Result<MaterialLease, MaterialError> {
    issue_for_selection_from(
        registry,
        selection,
        binding,
        lease_id,
        issued_at_ms,
        expires_at_ms,
        backend_kind,
        None,
    )
}
/// Issues from the newest subscription-login lineage revision when `head` is
/// given (looked up by the caller before this blocking read).
#[allow(clippy::too_many_arguments)]
pub fn issue_for_selection_from(
    registry: &LoadedRegistry,
    selection: &ResolvedSelection,
    binding: &LaunchBinding,
    lease_id: Uuid,
    issued_at_ms: u64,
    expires_at_ms: u64,
    backend_kind: Option<aq_selection::BackendKind>,
    head: Option<&HeadSource>,
) -> Result<MaterialLease, MaterialError> {
    let ExecutionTarget::Worker { node_id, boot_id } = selection.target else {
        return Err(MaterialError::Scope);
    };
    if !matches!(binding.session,SessionIdentity::Remote {node_id:assigned,boot_id:boot,..} if assigned==node_id && boot==boot_id)
    {
        return Err(MaterialError::Scope);
    }
    let (index, credential) = registry
        .credentials
        .iter()
        .enumerate()
        .find(|(_, c)| {
            c.profile == selection.choice.credential_profile
                && c.revision == selection.credential_revision
        })
        .ok_or(MaterialError::Scope)?;
    let configured = registry
        .config
        .credentials
        .get(index)
        .ok_or(MaterialError::Unavailable)?;
    if !credential.enabled
        || !credential.provisionable
        || credential.material_owner.is_none()
        || configured.delivery_mode.is_none()
        || !configured.allowed_node_ids.contains(&node_id)
        || aq_selection::credential_backend_policy(&configured.allowed_backends, backend_kind)
            .is_err()
        || credential.provider != selection.choice.provider
        || credential.account_reference != selection.choice.account_reference
        || credential.endpoint_reference != selection.choice.endpoint_reference
    {
        return Err(MaterialError::Scope);
    }
    let material = source_or_configured(registry, index, head)?;
    let lease = MaterialLease {
        schema_version: 1,
        lease_id,
        binding: binding.clone(),
        credential_profile: credential.profile.clone(),
        credential_revision: credential.revision,
        provider: Label::new(&credential.provider).map_err(|_| MaterialError::Scope)?,
        account_reference: Label::new(&credential.account_reference)
            .map_err(|_| MaterialError::Scope)?,
        endpoint_reference: Label::new(&credential.endpoint_reference)
            .map_err(|_| MaterialError::Scope)?,
        purpose: Label::new("model-authentication").map_err(|_| MaterialError::Scope)?,
        material_digest: Digest::of(material.bytes()),
        material_bytes: material.bytes().len() as u64,
        issued_at_ms,
        expires_at_ms,
    };
    lease.validate().map_err(|_| MaterialError::Scope)?;
    Ok(lease)
}
