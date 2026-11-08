//! New current facts over one exact held choice. Never invoke the selector or
//! restamp an initial Decision/BudgetSnapshot as ongoing authority.
use super::{health, now, pinned::Pinned};
use crate::{AppState, error::AppError, project_bindings::domain::FrozenProjectBinding};
use aq_admission::transport::Renew;
use aq_selection::{
    AvailabilityObservation, CapabilitySnapshot, ExecutionTarget, InstalledHarness,
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
fn harness_supports(h: &InstalledHarness, p: &Pinned) -> bool {
    choice_supported_by_harness(h, &p.selection.choice, &p.policy)
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
        && h.models.iter().any(|m| {
            m.provider == c.provider
                && m.model == c.model
                && m.reasoning_configuration_reference == c.reasoning_configuration_reference
                && m.native_reasoning == c.native_reasoning
        })
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
    if fresh.project.revision != p.spec.project_revision
        || fresh.node_id != request.identity.node_id
        || fresh.project.binding.catalog_digest != p.selection.catalog_digest.as_str()
        || serde_json::to_value(&fresh.project.binding).map_err(|_| AppError::Internal)?
            != serde_json::to_value(&p.project.binding).map_err(|_| AppError::Internal)?
    {
        return Err(invalid());
    }
    crate::project_bindings::fence_for_offer(state, tx, fresh).await?;
    let registry_revision = crate::intelligence::fence_current_registry(state, tx).await?;
    let loaded = state.config.intelligence.as_ref().ok_or_else(invalid)?;
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
    if !credential.enabled
        || !credential.provisionable
        || credential.material_owner.is_none()
        || credential.provider != c.provider
        || credential.account_reference != c.account_reference
        || credential.endpoint_reference != c.endpoint_reference
        || !loaded
            .config
            .credentials
            .get(index)
            .ok_or_else(invalid)?
            .allows(request.identity.node_id)
    {
        return Err(invalid());
    }
    // C5b: a refreshed version of the same login is continuity; an operator
    // replace or a rotated configured file is not (credential identity).
    if !crate::profiled_execution::login_refresh::continuous_locked(tx, loaded, index, &p.material)
        .await?
    {
        return Err(invalid());
    }
    let row=sqlx::query("SELECT boot_id,revision,snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE").bind(request.identity.node_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    let snapshot: CapabilitySnapshot =
        serde_json::from_value(row.get("snapshot")).map_err(|_| invalid())?;
    let time = now(state)?;
    let revision = row.get::<i64, _>("revision") as u64;
    if row.get::<uuid::Uuid, _>("boot_id") != request.identity.boot_id
        || snapshot.target
            != (ExecutionTarget::Worker {
                node_id: request.identity.node_id,
                boot_id: request.identity.boot_id,
            })
        || snapshot.revision != revision
        || revision < p.selection.node_revision
        || snapshot.revision == 0
        || snapshot.harnesses.len() > 16
        || snapshot.credential_bindings.len() > 32
        || snapshot.observed_at_ms == 0
        || snapshot
            .expires_at_ms
            .checked_sub(snapshot.observed_at_ms)
            .is_none_or(|ttl| ttl == 0 || ttl > 120_000)
        || snapshot.observed_at_ms > time
        || time >= snapshot.expires_at_ms
        || !snapshot.harnesses.iter().any(|h| harness_supports(h, p))
        || !loaded
            .config
            .qualified_harnesses
            .iter()
            .any(|h| harness_supports(h, p))
    {
        return Err(invalid());
    }
    if snapshot
        .credential_bindings
        .iter()
        .filter(|binding| {
            binding.credential_profile == c.credential_profile
                && binding.credential_revision == p.selection.credential_revision
        })
        .count()
        > 1
    {
        return Err(invalid());
    }
    let binding = snapshot.credential_bindings.iter().find(|binding| {
        binding.credential_profile == c.credential_profile
            && binding.credential_revision == p.selection.credential_revision
    });
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
    let inventory_ceiling = if let Some(expiry) = scoped {
        expiry
    } else {
        installation_supports_inventory(
            revision,
            p.selection.node_revision,
            binding.map(|b| (b.available, b.observed_at_ms, b.expires_at_ms)),
            time,
            p.installation.lease_id == p.material.lease_id
                && p.installation.material_digest == p.material.material_digest,
        )?;
        binding
            .filter(|b| b.available)
            .map_or(snapshot.expires_at_ms, |b| {
                snapshot.expires_at_ms.min(b.expires_at_ms)
            })
    };
    crate::intelligence::fence_continuing_principal(
        state,
        tx,
        p.decision_id,
        p.selection.lineage.project_id,
    )
    .await?;
    let observations:Vec<serde_json::Value>=sqlx::query_scalar("SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1025 FOR SHARE").fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    if observations.len() > 1024 {
        return Err(invalid());
    }
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
    Ok(Current {
        health: evidence,
        registry_revision,
        inventory_revision: revision,
        inventory_expires_at_ms: inventory_ceiling,
    })
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
