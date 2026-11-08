//! Current locked authorization/inventory/health for the exact accepted choice.
//! Caller owns authenticated node/attempt and current project/catalog locks.
//! No selector, external read, restamped selection, or global installed-key claim.
use super::{db, digest, now, phase, refused};
use crate::{
    AppState,
    error::AppError,
    intelligence::resolver,
    profiled_execution::{presence, renewal},
};
use aq_admission::{ArtifactRef, Digest, MaterialLease, Text};
use aq_profiles::SelectionPolicy;
use aq_selection::{
    Availability, AvailabilityObservation, CapabilitySnapshot, ConcreteChoice, CredentialDelivery,
    ExecutionTarget, ResolvedSelection,
};
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;
pub(super) struct Input<'a> {
    pub proposal: &'a phase::Proposal,
    pub selection: &'a ResolvedSelection,
    pub decision: &'a resolver::Decision,
    pub policy: &'a SelectionPolicy,
    pub material: &'a MaterialLease,
    pub registry_at_preflight: &'a super::RegistrySnapshot,
}
pub(super) struct Current {
    pub registry: ArtifactRef,
    pub registry_at_preflight: super::RegistrySnapshot,
    pub principal: ArtifactRef,
    pub inventory: CapabilitySnapshot,
    pub inventory_sequence: u64,
    pub inventory_request_digest: Digest,
    pub inventory_receipt_digest: Digest,
    pub compatibility_epoch: u64,
    pub health: renewal::HealthEvidence,
    pub availability: Vec<AvailabilityObservation>,
    pub availability_expires_at_ms: u64,
    pub checked_at_ms: u64,
    inventory_expires_at_ms: u64,
}
impl Current {
    pub fn fresh_at(&self, time: u64) -> Result<(), AppError> {
        if time < self.checked_at_ms
            || time >= self.inventory_expires_at_ms
            || time >= self.availability_expires_at_ms
        {
            return Err(refused());
        }
        Ok(())
    }
    pub fn inventory_expires_at_ms(&self) -> u64 {
        self.inventory_expires_at_ms
    }
}
fn pin(id: String, version: u64, hash: Digest) -> Result<ArtifactRef, AppError> {
    if version == 0 {
        return Err(refused());
    }
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| refused())?,
        version: Text::new(version.to_string()).map_err(|_| refused())?,
        digest: hash,
    })
}
fn relevant(o: &AvailabilityObservation, c: &ConcreteChoice, revision: u64, node: Uuid) -> bool {
    let s = &o.scope;
    s.credential_profile == c.credential_profile
        && s.credential_revision == revision
        && s.provider == c.provider
        && s.account_reference == c.account_reference
        && s.endpoint_reference == c.endpoint_reference
        && s.model.as_ref().is_none_or(|m| m == &c.model)
        && s.node_id.is_none_or(|n| n == node)
}
/// Conservative expiry from actual latest relevant positive sources. The shared
/// health owner separately enforces unknown, negatives and active cooldowns.
fn availability_ceiling(
    observations: &[AvailabilityObservation],
    c: &ConcreteChoice,
    revision: u64,
    node: Uuid,
    maximum_age_ms: u64,
    time: u64,
) -> Result<u64, AppError> {
    let mut latest: BTreeMap<Digest, &AvailabilityObservation> = BTreeMap::new();
    for o in observations {
        if !relevant(o, c, revision, node) {
            continue;
        }
        o.validate().map_err(|_| refused())?;
        let key = aq_selection::digest(&(&o.scope, &o.source)).map_err(|_| refused())?;
        if let Some(old) = latest.get(&key) {
            if o.sequence < old.sequence {
                continue;
            }
            if o.sequence == old.sequence {
                if aq_selection::digest(o).map_err(|_| refused())?
                    != aq_selection::digest(old).map_err(|_| refused())?
                {
                    return Err(refused());
                }
                continue;
            }
        }
        latest.insert(key, o);
    }
    latest
        .values()
        .filter_map(|o| {
            // Age is inclusive in health; this exclusive bound is conservative at
            // the final millisecond and never restarts a source's original age.
            let end = o
                .expires_at_ms
                .min(o.received_at_ms.checked_add(maximum_age_ms)?);
            (o.state == Availability::Available
                && o.received_at_ms <= time
                && time < end
                && o.cooldown_until_ms <= time)
                .then_some(end)
        })
        .min()
        .ok_or_else(|| refused())
}
pub(super) async fn fence_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    input: &Input<'_>,
) -> Result<Current, AppError> {
    let p = input.proposal;
    let s = input.selection;
    let c = &s.choice;
    let material = input.material;
    let (node, boot) = match p.original.offered.offer.binding.session {
        aq_admission::SessionIdentity::Remote {
            node_id, boot_id, ..
        } => (node_id, boot_id),
        _ => return Err(refused()),
    };
    let target = ExecutionTarget::Worker {
        node_id: node,
        boot_id: boot,
    };
    if s.target != target
        || input.decision.inputs.node.target != target
        || input.decision.inputs.node.revision != s.node_revision
        || aq_selection::digest(s).map_err(|_| refused())? != p.choice.selection_digest
        || aq_selection::digest(
            input
                .decision
                .outcome
                .resolved
                .as_ref()
                .ok_or_else(|| refused())?,
        )
        .map_err(|_| refused())?
            != p.choice.selection_digest
        || material.binding != p.original.offered.offer.binding
        || material.credential_profile != c.credential_profile
        || material.credential_revision != s.credential_revision
        || material.provider.as_str() != c.provider
        || material.account_reference.as_str() != c.account_reference
        || material.endpoint_reference.as_str() != c.endpoint_reference
    {
        return Err(refused());
    }
    material.validate().map_err(|_| refused())?;
    let registry_revision = crate::intelligence::fence_current_registry(state, tx).await?;
    let registry_row = sqlx::query(
        "SELECT revision,config_digest FROM intelligence_registry WHERE singleton FOR SHARE",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let revision = u64::try_from(registry_row.get::<i64, _>("revision")).map_err(|_| refused())?;
    let registry = pin(
        "aq.intelligence-registry".into(),
        revision,
        serde_json::from_value(serde_json::json!(
            registry_row.get::<String, _>("config_digest")
        ))
        .map_err(|_| refused())?,
    )?;
    if revision != registry_revision {
        return Err(refused());
    }
    super::validate_registry_current(input, &registry)?;
    let loaded = state
        .config
        .intelligence
        .as_ref()
        .ok_or_else(|| refused())?;
    let (index, credential) = loaded
        .credentials
        .iter()
        .enumerate()
        .find(|(_, a)| a.profile == c.credential_profile && a.revision == s.credential_revision)
        .ok_or_else(|| refused())?;
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
            .ok_or_else(|| refused())?
            .allows(node)
    {
        return Err(refused());
    }
    // C5b: a refreshed version of the same login is continuity; an operator
    // replace or a rotated configured file is not (credential identity).
    if !crate::profiled_execution::login_refresh::continuous_locked(tx, loaded, index, material)
        .await?
    {
        return Err(refused());
    }
    let row = sqlx::query("SELECT boot_id,revision,snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE").bind(node).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(|| refused())?;
    let inventory: CapabilitySnapshot =
        serde_json::from_value(row.get("snapshot")).map_err(|_| refused())?;
    let revision = u64::try_from(row.get::<i64, _>("revision")).map_err(|_| refused())?;
    let supports = |h: &aq_selection::InstalledHarness| {
        renewal::choice_supported_by_harness(h, c, input.policy)
    };
    if row.get::<Uuid, _>("boot_id") != boot
        || inventory.target != target
        || inventory.revision != revision
        || revision < s.node_revision
        || revision == 0
        || inventory.harnesses.len() > 16
        || inventory.credential_bindings.len() > 32
        || !inventory.harnesses.iter().any(supports)
        || !loaded.config.qualified_harnesses.iter().any(supports)
    {
        return Err(refused());
    }
    let current = presence::current_inventory_locked(state, tx, node, boot, &inventory).await?;
    let historical = presence::inventory_epoch_locked(
        tx,
        node,
        boot,
        s.node_revision,
        &aq_selection::digest(&input.decision.inputs.node).map_err(|_| refused())?,
    )
    .await?;
    if historical != current.compatibility_epoch {
        return Err(refused());
    }
    let bindings: Vec<_> = inventory
        .credential_bindings
        .iter()
        .filter(|b| {
            b.credential_profile == c.credential_profile
                && b.credential_revision == s.credential_revision
        })
        .collect();
    if bindings.len() != 1 || bindings[0].available {
        return Err(refused());
    }
    let binding = bindings[0];
    let provision = binding.preparation.as_ref().ok_or_else(|| refused())?;
    let Some(CredentialDelivery::ServerLease {
        owner,
        adapter,
        version,
    }) = &c.credential_delivery
    else {
        return Err(refused());
    };
    if provision.owner != *owner
        || credential.material_owner.as_ref() != Some(owner)
        || provision.credential_profile != c.credential_profile
        || provision.credential_revision != s.credential_revision
        || provision.target != target
        || provision.delivery.adapter != *adapter
        || provision.delivery.version != *version
        || inventory
            .material_delivery
            .as_ref()
            .is_none_or(|d| d.adapter != *adapter || d.version != *version)
    {
        return Err(refused());
    }
    crate::intelligence::fence_continuing_principal(
        state,
        tx,
        input.decision.decision_id,
        s.lineage.project_id,
    )
    .await?;
    let raw_principal: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_object('kind',authority_kind,'actor',actor_id,'project',authority_project_id,'policy',authority_policy_ref,'revision',authority_policy_revision,'digest',authority_policy_digest) FROM intelligence_decisions WHERE decision_id=$1 FOR SHARE")
        .bind(input.decision.decision_id).fetch_one(&mut **tx).await.map_err(db)?;
    let principal = pin(
        format!("aq.selection-principal/{}", input.decision.decision_id),
        1,
        aq_selection::digest(&raw_principal).map_err(|_| refused())?,
    )?;
    if principal != p.choice.principal {
        return Err(refused());
    }
    let raw: Vec<serde_json::Value> = sqlx::query_scalar("SELECT observation FROM intelligence_observations ORDER BY scope_source_digest LIMIT 1025 FOR SHARE").fetch_all(&mut **tx).await.map_err(db)?;
    let mut availability: Vec<AvailabilityObservation> = raw
        .into_iter()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|_| refused())?;
    let time = now(state)?;
    // The switch's attestation, as in selection; rows and attestation within 1,024.
    let credentials = std::slice::from_ref(credential);
    let mine = crate::intelligence::resolver::relevant(availability.clone(), credentials, node);
    let owner = aq_machine_control::owner_attested(tx, credentials, &mine, registry_revision, time);
    availability.extend(owner.await?);
    if availability.len() > 1024 {
        return Err(refused());
    }
    super::validate_observation_revisions(&availability, &registry)?;
    if inventory.observed_at_ms == 0
        || inventory.observed_at_ms > time
        || inventory
            .expires_at_ms
            .checked_sub(inventory.observed_at_ms)
            .is_none_or(|ttl| ttl == 0 || ttl > 120_000)
        || binding.observed_at_ms == 0
        || binding.observed_at_ms > time
        || provision.observed_at_ms == 0
        || provision.observed_at_ms > time
    {
        return Err(refused());
    }
    let health = renewal::evaluate_health(
        renewal::HealthSubject {
            profile: &c.credential_profile,
            revision: s.credential_revision,
            provider: &c.provider,
            account: &c.account_reference,
            endpoint: &c.endpoint_reference,
            model: &c.model,
            node,
        },
        &availability,
        input.policy.observation_max_age_ms,
        time,
    )?;
    if health.state != renewal::HealthState::ObservedAvailable {
        // Same refusal and redacted wire in all builds. A fixed test-only
        // classification identifies this actual branch without logging health
        // source details, credential scopes or arbitrary error strings.
        #[cfg(feature = "test-support")]
        return Err(AppError::Conflict(
            "phase execution health is not observed available".into(),
        ));
        #[cfg(not(feature = "test-support"))]
        return Err(refused());
    }
    let availability_expires_at_ms = availability_ceiling(
        &availability,
        c,
        s.credential_revision,
        node,
        input.policy.observation_max_age_ms,
        time,
    )?;
    let inventory_expires_at_ms = inventory
        .expires_at_ms
        .min(current.receipt.expires_at_ms)
        .min(binding.expires_at_ms)
        .min(provision.expires_at_ms);
    let result = Current {
        registry,
        registry_at_preflight: input.registry_at_preflight.clone(),
        principal,
        inventory_sequence: current.request.sequence,
        inventory_request_digest: current.request.digest().map_err(|_| refused())?,
        inventory_receipt_digest: digest(&current.receipt)?,
        compatibility_epoch: current.compatibility_epoch,
        inventory,
        health,
        availability,
        availability_expires_at_ms,
        checked_at_ms: time,
        inventory_expires_at_ms,
    };
    result.fresh_at(now(state)?)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_selection::{ObservationScope, ObservationSource};
    fn fixture() -> (ResolvedSelection, AvailabilityObservation) {
        let v: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../aq-selection/tests/fixtures/installed-selection-v0.1.0.json"
        ))
        .unwrap();
        let s: ResolvedSelection =
            serde_json::from_value(v["outcome"]["resolved"].clone()).unwrap();
        let o = AvailabilityObservation {
            observation_id: Uuid::new_v4(),
            scope: ObservationScope {
                credential_profile: s.choice.credential_profile.clone(),
                credential_revision: s.credential_revision,
                provider: s.choice.provider.clone(),
                account_reference: s.choice.account_reference.clone(),
                endpoint_reference: s.choice.endpoint_reference.clone(),
                model: Some(s.choice.model.clone()),
                node_id: Some(Uuid::from_u128(1)),
            },
            source: ObservationSource::Operator {
                principal_id: Uuid::from_u128(2),
            },
            sequence: 1,
            revision: 1,
            state: Availability::Available,
            received_at_ms: 100,
            expires_at_ms: 1000,
            cooldown_until_ms: 100,
        };
        (s, o)
    }
    #[test]
    fn provider_fact_cap_preserves_source_age_and_expiry() {
        let (s, mut o) = fixture();
        assert_eq!(
            availability_ceiling(
                &[o.clone()],
                &s.choice,
                s.credential_revision,
                Uuid::from_u128(1),
                100,
                150
            )
            .unwrap(),
            200
        );
        o.expires_at_ms = 175;
        assert_eq!(
            availability_ceiling(
                &[o.clone()],
                &s.choice,
                s.credential_revision,
                Uuid::from_u128(1),
                100,
                150
            )
            .unwrap(),
            175
        );
        assert!(
            availability_ceiling(
                &[o],
                &s.choice,
                s.credential_revision,
                Uuid::from_u128(1),
                100,
                175
            )
            .is_err()
        );
    }
    #[test]
    fn old_positive_cannot_supply_lifetime_after_newer_same_source_negative_expires() {
        let (s, o) = fixture();
        let mut newer = o.clone();
        newer.observation_id = Uuid::new_v4();
        newer.sequence = 2;
        newer.revision = 2;
        newer.state = Availability::AuthenticationFailed;
        newer.received_at_ms = 110;
        newer.expires_at_ms = 140;
        assert!(
            availability_ceiling(
                &[o, newer],
                &s.choice,
                s.credential_revision,
                Uuid::from_u128(1),
                100,
                150
            )
            .is_err()
        );
    }
    #[test]
    fn unrelated_future_cooling_and_conflicting_sources_supply_no_positive_cap() {
        let (s, o) = fixture();
        for mode in 0..3 {
            let mut changed = o.clone();
            match mode {
                0 => changed.scope.account_reference = "unrelated".into(),
                1 => changed.received_at_ms = 160,
                _ => changed.cooldown_until_ms = 900,
            }
            assert!(
                availability_ceiling(
                    &[changed],
                    &s.choice,
                    s.credential_revision,
                    Uuid::from_u128(1),
                    100,
                    150
                )
                .is_err()
            );
        }
        let mut conflict = o.clone();
        conflict.state = Availability::ModelUnsupported;
        assert!(
            availability_ceiling(
                &[o, conflict],
                &s.choice,
                s.credential_revision,
                Uuid::from_u128(1),
                100,
                150
            )
            .is_err()
        );
    }
}
