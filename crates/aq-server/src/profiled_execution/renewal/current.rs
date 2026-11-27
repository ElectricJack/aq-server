//! New current facts over one exact held choice. Never invoke the selector or
//! restamp an initial Decision/BudgetSnapshot as ongoing authority.
use super::{health, now, pinned::Pinned};
use crate::{AppState, error::AppError, project_bindings::domain::FrozenProjectBinding};
use aq_admission::transport::Renew;
use aq_selection::{
    AvailabilityObservation, CapabilitySnapshot, CredentialBinding, ExecutionTarget,
    InstalledHarness,
};
use sqlx::{Postgres, Row, Transaction};
pub struct Current {
    pub health: health::Evidence,
    pub registry_revision: u64,
    pub inventory_revision: u64,
    pub inventory_expires_at_ms: u64,
}
fn invalid() -> AppError {
    AppError::Conflict("continuing choice is not currently authorized or supported".into())
}
fn ensure(supported: bool) -> Result<(), AppError> {
    if supported { Ok(()) } else { Err(invalid()) }
}
fn harness_supports(h: &InstalledHarness, p: &Pinned) -> bool {
    choice_supported_by_harness(h, &p.selection.choice, &p.policy)
}
/// The node's binding for exactly the held credential profile and revision.
fn holds_credential(binding: &CredentialBinding, p: &Pinned) -> bool {
    binding.credential_profile == p.selection.choice.credential_profile
        && binding.credential_revision == p.selection.credential_revision
}
pub(crate) fn choice_supported_by_harness(
    h: &InstalledHarness,
    c: &aq_selection::ConcreteChoice,
    policy: &aq_profiles::SelectionPolicy,
) -> bool {
    let limits = &h.limits;
    h.harness == c.harness
        && h.cli_version == c.cli_version
        && h.adapter == c.adapter
        && h.environment == c.environment
        && h.qualification == c.qualification
        // An installed harness runs any model (spec 0014 §5.2): the held choice's
        // model and reasoning were resolved and sealed at selection.
        && limits.context_bytes >= c.resources.context_bytes
        && limits.wall_time_ms >= c.resources.wall_time_ms
        && limits.memory_bytes >= c.resources.memory_bytes
        && limits.cpu_millis >= c.resources.cpu_millis
        && limits.process_limit >= c.resources.process_limit
        && limits.max_output_tokens >= c.max_output_tokens
        && aq_session_protocol::require_capabilities(&policy.required_capabilities, &h.capabilities)
            .is_ok()
}
pub async fn fence_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    request: &Renew,
    p: &Pinned,
    fresh: &FrozenProjectBinding,
) -> Result<Current, AppError> {
    let json = |binding| serde_json::to_value(binding).map_err(|_| AppError::Internal);
    ensure(
        fresh.project.revision == p.spec.project_revision
            && fresh.node_id == request.identity.node_id
            && json(&fresh.project.binding)? == json(&p.project.binding)?,
    )?;
    crate::project_bindings::fence_for_offer(state, tx, fresh).await?;
    let registry_revision = crate::intelligence::fence_current_registry(state, tx).await?;
    let loaded = state.config.intelligence.as_ref().ok_or_else(invalid)?;
    let index = authorized_credential_index(loaded, p, request)?;
    // C5b: a refreshed version of the same login is continuity; an operator
    // replace or a rotated configured file is not (credential identity).
    ensure(
        crate::profiled_execution::login_refresh::continuous_locked(tx, loaded, index, &p.material)
            .await?,
    )?;
    let (snapshot, revision, time) =
        load_inventory_locked(state, tx, request, p, &loaded.config.qualified_harnesses).await?;
    let scoped = crate::profiled_execution::presence::scoped_locked(
        state,
        tx,
        crate::profiled_execution::presence::HeldMaterial {
            binding: &request.binding,
            grant: request.grant_id,
            grant_revision: p.versions.grant_revision,
            original: &p.original,
            lease: &p.material,
            installation: &p.installation,
            snapshot: &snapshot,
        },
    )
    .await?;
    let inventory_ceiling = match scoped {
        Some(expiry) => expiry,
        None => unscoped_inventory_ceiling(p, &snapshot, revision, time)?,
    };
    crate::intelligence::fence_continuing_principal(
        state,
        tx,
        p.decision_id,
        p.selection.lineage.project_id,
    )
    .await?;
    let evidence = load_health_locked(state, tx, request, p).await?;
    Ok(Current {
        health: evidence,
        registry_revision,
        inventory_revision: revision,
        inventory_expires_at_ms: inventory_ceiling,
    })
}

/// Without scoped presence, the node report itself must still back the material.
fn unscoped_inventory_ceiling(
    p: &Pinned,
    snapshot: &CapabilitySnapshot,
    revision: u64,
    time: u64,
) -> Result<u64, AppError> {
    let binding = snapshot
        .credential_bindings
        .iter()
        .find(|b| holds_credential(b, p));
    installation_supports_inventory(
        revision,
        p.selection.node_revision,
        binding.map(|b| (b.available, b.observed_at_ms, b.expires_at_ms)),
        time,
        p.installation.lease_id == p.material.lease_id
            && p.installation.material_digest == p.material.material_digest,
    )?;
    Ok(binding
        .filter(|b| b.available)
        .map_or(snapshot.expires_at_ms, |b| {
            snapshot.expires_at_ms.min(b.expires_at_ms)
        }))
}
fn authorized_credential_index(
    loaded: &crate::intelligence::config::LoadedRegistry,
    p: &Pinned,
    request: &Renew,
) -> Result<usize, AppError> {
    let c = &p.selection.choice;
    let (index, credential) = loaded
        .credentials
        .iter()
        .enumerate()
        .find(|(_, credential)| {
            credential.profile == c.credential_profile
                && credential.revision == p.selection.credential_revision
        })
        .ok_or_else(invalid)?;
    let config = loaded.config.credentials.get(index);
    ensure(
        credential.enabled
            && credential.provisionable
            && credential.material_owner.is_some()
            && credential.provider == c.provider
            && credential.account_reference == c.account_reference
            && credential.endpoint_reference == c.endpoint_reference
            && config.ok_or_else(invalid)?.allows(request.identity.node_id),
    )?;
    Ok(index)
}
async fn load_inventory_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    request: &Renew,
    p: &Pinned,
    qualified_harnesses: &[InstalledHarness],
) -> Result<(CapabilitySnapshot, u64, u64), AppError> {
    let row=sqlx::query("SELECT boot_id,revision,snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE").bind(request.identity.node_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    let snapshot: CapabilitySnapshot =
        serde_json::from_value(row.get("snapshot")).map_err(|_| invalid())?;
    let qualified = super::super::definitions::qualified(state, tx, qualified_harnesses).await?;
    let time = now(state)?;
    let revision = row.get::<i64, _>("revision") as u64;
    let target = ExecutionTarget::Worker {
        node_id: request.identity.node_id,
        boot_id: request.identity.boot_id,
    };
    let ttl = snapshot.expires_at_ms.checked_sub(snapshot.observed_at_ms);
    ensure(
        row.get::<uuid::Uuid, _>("boot_id") == request.identity.boot_id
            && snapshot.target == target
            && snapshot.revision == revision
            && revision >= p.selection.node_revision
            && snapshot.revision != 0
            && snapshot.harnesses.len() <= 16
            && snapshot.credential_bindings.len() <= 32
            && snapshot.observed_at_ms != 0
            && ttl.is_some_and(|ttl| ttl != 0 && ttl <= 120_000)
            && snapshot.observed_at_ms <= time
            && time < snapshot.expires_at_ms,
    )?;
    ensure(
        snapshot.harnesses.iter().any(|h| harness_supports(h, p))
            && qualified.iter().any(|h| harness_supports(h, p)),
    )?;
    let held = snapshot
        .credential_bindings
        .iter()
        .filter(|b| holds_credential(b, p));
    ensure(held.count() <= 1)?;
    Ok((snapshot, revision, time))
}
async fn load_health_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    request: &Renew,
    p: &Pinned,
) -> Result<health::Evidence, AppError> {
    let c = &p.selection.choice;
    let observations:Vec<serde_json::Value>=sqlx::query_scalar("SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1025 FOR SHARE").fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    ensure(observations.len() <= 1024)?;
    let observations: Vec<AvailabilityObservation> = observations
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|_| invalid())?;
    let time = now(state)?;
    let evidence = health::evaluate(
        health::Subject {
            profile: &c.credential_profile,
            revision: p.selection.credential_revision,
            provider: &c.provider,
            account: &c.account_reference,
            endpoint: &c.endpoint_reference,
            model: &c.model,
            node: request.identity.node_id,
        },
        &observations,
        p.policy.observation_max_age_ms,
        time,
    )?;
    if evidence.state == health::State::Denied {
        return Err(AppError::Conflict(
            "fresh scoped provider failure or cooldown prevents continuation".into(),
        ));
    }
    Ok(evidence)
}

// Only an actual retained matching installation can supersede the original
// pre-install report. Later node reports are authoritative negative evidence.
fn installation_supports_inventory(
    revision: u64,
    original_revision: u64,
    binding: Option<(bool, u64, u64)>,
    time: u64,
    retained_matching_installation: bool,
) -> Result<(), AppError> {
    match binding {
        Some((true, observed, expires)) if observed <= time && time < expires => Ok(()),
        Some((true, _, _)) => Err(invalid()),
        _ if revision == original_revision && retained_matching_installation => Ok(()),
        _ => Err(invalid()),
    }
}
#[cfg(test)]
mod tests {
    use super::installation_supports_inventory as supported;
    #[test]
    fn original_cold_inventory_requires_actual_installation() {
        assert!(supported(3, 3, Some((false, 10, 40)), 20, true).is_ok());
        assert!(supported(3, 3, None, 20, true).is_ok());
        assert!(supported(3, 3, Some((false, 10, 40)), 20, false).is_err());
        assert!(supported(3, 3, None, 20, false).is_err());
    }
    #[test]
    fn newer_missing_unavailable_and_expired_positive_cannot_be_rewritten() {
        assert!(supported(4, 3, Some((false, 10, 40)), 20, true).is_err());
        assert!(supported(4, 3, None, 20, true).is_err());
        assert!(supported(4, 3, Some((true, 10, 20)), 20, true).is_err());
        assert!(supported(3, 3, Some((true, 21, 40)), 20, true).is_err());
        assert!(supported(4, 3, Some((true, 10, 40)), 20, true).is_ok());
    }
}
