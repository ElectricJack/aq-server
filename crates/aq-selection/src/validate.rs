use crate::*;
use aq_session_protocol::Validate;
use std::collections::BTreeSet;
fn text(value: &str) -> Result<()> {
    ensure(
        !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "bounded reference required",
    )
}
fn target(value: &ExecutionTarget) -> Result<()> {
    ensure(
        match value {
            ExecutionTarget::Worker { node_id, boot_id } => !node_id.is_nil() && !boot_id.is_nil(),
            ExecutionTarget::Central {
                authority_id,
                generation,
            } => !authority_id.is_nil() && *generation > 0,
        },
        "target identity",
    )
}
fn lineage(value: &TaskLineage) -> Result<()> {
    ensure(
        !value.project_id.is_nil() && !value.workspace_id.is_nil() && !value.task_id.is_nil(),
        "lineage identity",
    )
}
fn bounds<T>(value: &[T], max: usize) -> Result<()> {
    ensure(value.len() <= max, "snapshot array bound")
}
fn placement(p: &Placement) -> Result<()> {
    bounds(&p.capabilities, 16)?;
    bounds(&p.required_capabilities, 16)?;
    bounds(&p.templates, 64)?;
    bounds(&p.environment_templates, 64)?;
    for value in p.capabilities.iter().chain(&p.required_capabilities) {
        text(value)?;
    }
    for template in &p.templates {
        bounds(&template.harnesses, 32)?;
        for h in &template.harnesses {
            text(&h.harness)?;
            text(&h.version)?;
        }
    }
    for t in &p.environment_templates {
        text(&t.environment)?;
    }
    Ok(())
}
pub(crate) fn fresh(observed: u64, expires: u64, now: u64) -> bool {
    observed > 0 && observed <= now && now < expires
}
impl AvailabilityObservation {
    pub fn validate(&self) -> Result<()> {
        ensure(
            !self.observation_id.is_nil()
                && self.sequence > 0
                && self.revision > 0
                && self.scope.credential_revision > 0,
            "observation identity/sequence",
        )?;
        text(&self.scope.provider)?;
        text(&self.scope.account_reference)?;
        text(&self.scope.endpoint_reference)?;
        if let Some(model) = &self.scope.model {
            text(model)?;
        }
        ensure(
            self.scope.node_id.is_none_or(|id| !id.is_nil()),
            "observation node",
        )?;
        ensure(
            self.received_at_ms > 0
                && self.expires_at_ms > self.received_at_ms
                && self.expires_at_ms - self.received_at_ms <= 3_600_000
                && self.cooldown_until_ms <= self.expires_at_ms,
            "observation timing",
        )?;
        match &self.source {
            ObservationSource::Operator { principal_id } => {
                ensure(!principal_id.is_nil(), "operator provenance")?
            }
            ObservationSource::Worker {
                node_id,
                boot_id,
                attempt_id,
                ..
            } => ensure(
                !node_id.is_nil()
                    && !boot_id.is_nil()
                    && !attempt_id.is_nil()
                    && self.scope.node_id == Some(*node_id)
                    && self.scope.model.is_some()
                    && self.state != Availability::Available,
                "worker feedback cannot broaden scope or claim availability",
            )?,
        }
        Ok(())
    }
}
pub(crate) fn validate(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
) -> Result<()> {
    lineage(&input.project.lineage)?;
    target(&input.project.target)?;
    target(&input.node.target)?;
    ensure(
        input.project.target == input.node.target && input.project.lineage == input.budget.lineage,
        "snapshot subject mismatch",
    )?;
    ensure(
        input.now_ms > 0
            && input.project.revision > 0
            && input.project.catalog_revision > 0
            && input.node.revision > 0
            && input.registry_revision > 0
            && input.budget.revision > 0,
        "snapshot revisions",
    )?;
    ensure(
        catalog.digest() == input.project.catalog_digest.as_str(),
        "catalog snapshot mismatch",
    )?;
    if let Some(p) = &input.placement {
        placement(p)?;
    }
    for value in [
        &input.project.agent_profile,
        &input.project.intelligence_profile,
        &input.project.execution_profile,
    ] {
        text(value)?;
    }
    bounds(&input.project.allowed_environments, 32)?;
    ensure(
        !input.project.allowed_environments.is_empty()
            && input
                .project
                .allowed_environments
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                == input.project.allowed_environments.len(),
        "project environment allowlist",
    )?;
    for environment in &input.project.allowed_environments {
        text(environment)?;
    }
    let c = &input.project.ceilings;
    ensure(
        c.context_bytes > 0
            && c.context_bytes <= 1_048_576
            && c.wall_time_ms > 0
            && c.memory_bytes > 0
            && c.cpu_millis > 0
            && c.process_limit > 0
            && c.cost_limit_microunits.is_some() == c.currency.is_some(),
        "project ceilings",
    )?;
    if let Some(currency) = &c.currency {
        ensure(
            currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
            "currency",
        )?;
    }
    bounds(&input.project.permission_ceiling, 128)?;
    bounds(&input.node.harnesses, 16)?;
    bounds(&input.node.credential_bindings, 32)?;
    bounds(&input.credentials, 32)?;
    bounds(&input.observations, 128)?;
    bounds(&input.estimates, 128)?;
    bounds(&input.compositions, 16)?;
    validate_installation(
        &input.credentials,
        &input.node.credential_bindings,
        input.node.material_delivery.as_ref(),
        &input.node.harnesses,
        &input.observations,
        input.now_ms,
    )?;
    let mut total = 0usize;
    let mut compositions = BTreeSet::new();
    for prepared in &input.compositions {
        let composition = &prepared.composition;
        ensure(
            compositions.insert((&composition.harness, &composition.environment)),
            "duplicate composition combination",
        )?;
        bounds(&composition.instruction_order, 128)?;
        for instruction in &composition.instruction_order {
            if let Some(key) = instruction.source.identity.strip_prefix("module:") {
                let expected = catalog
                    .source("module", key)
                    .map_err(|_| SelectionError::Invalid("unknown instruction module".into()))?;
                ensure(
                    instruction.source.digest == expected.digest
                        && instruction.source.revision == expected.revision
                        && catalog.artifact().instructions.get(key) == Some(&instruction.text),
                    "instruction module differs from immutable catalog",
                )?;
            } else if let Some(key) = instruction.source.identity.strip_prefix("agent:") {
                let expected = catalog
                    .source("agent", key)
                    .map_err(|_| SelectionError::Invalid("unknown instruction agent".into()))?;
                ensure(
                    instruction.source.digest == expected.digest
                        && instruction.source.revision == expected.revision
                        && catalog.artifact().catalog.agents.iter().any(|a| {
                            aq_profiles::key(&a.id, a.version) == key && a.role == instruction.text
                        }),
                    "instruction role differs from immutable catalog",
                )?;
            }
            ensure(instruction.text.len() <= 65_536, "instruction bound")?;
            total = total
                .checked_add(instruction.text.len())
                .ok_or_else(|| SelectionError::Invalid("snapshot overflow".into()))?;
            ensure(total <= 8_388_608, "composition aggregate bound")?;
        }
        ensure(
            prepared.digest == digest(composition)?
                && composition.catalog_digest == catalog.digest()
                && composition.project_binding_id == input.project.lineage.project_id
                && composition.project_revision == input.project.revision
                && composition.agent_profile.identity
                    == format!("agent:{}", input.project.agent_profile)
                && composition.intelligence_profile.identity
                    == format!("intelligence:{}", input.project.intelligence_profile)
                && composition.execution_profile.identity
                    == format!("execution:{}", input.project.execution_profile),
            "composition binding mismatch",
        )?;
        for (namespace, key, source) in [
            (
                "agent",
                &input.project.agent_profile,
                &composition.agent_profile,
            ),
            (
                "intelligence",
                &input.project.intelligence_profile,
                &composition.intelligence_profile,
            ),
            (
                "execution",
                &input.project.execution_profile,
                &composition.execution_profile,
            ),
        ] {
            let expected = catalog.source(namespace, key).map_err(|_| {
                SelectionError::Invalid("missing composition profile source".into())
            })?;
            ensure(
                source.digest == expected.digest && source.revision == expected.revision,
                "composition source digest mismatch",
            )?;
        }
        let execution = catalog
            .artifact()
            .catalog
            .execution
            .iter()
            .find(|e| aq_profiles::key(&e.id, e.version) == input.project.execution_profile)
            .ok_or_else(|| SelectionError::Invalid("execution profile absent".into()))?;
        bounds(&composition.effective_permissions, 128)?;
        for permission in &composition.effective_permissions {
            text(&permission.operation)?;
            text(&permission.scope_reference)?;
            ensure(
                execution.permissions.iter().any(|p| {
                    p.operation == permission.operation
                        && p.scope_reference == permission.scope_reference
                }) && input.project.permission_ceiling.iter().any(|p| {
                    p.operation.as_str() == permission.operation
                        && p.resource.as_str() == permission.scope_reference
                }),
                "composition permissions exceed selected ceilings",
            )?;
        }
    }
    if let Some(replacement) = &input.replacement {
        lineage(&replacement.lineage)?;
        ensure(
            replacement.lineage == input.project.lineage
                && !replacement.predecessor_attempt_id.is_nil(),
            "replacement subject mismatch",
        )?;
    }
    ensure(
        serde_json::to_vec(input)
            .map_err(|_| SelectionError::Invalid("snapshot serialization".into()))?
            .len()
            <= 8_388_608,
        "snapshot byte bound",
    )
}

/// Structural checks shared by subject adapters; these never authenticate facts.
pub(crate) fn validate_installation(
    credential_rows: &[AuthorizedCredential],
    bindings: &[CredentialBinding],
    delivery: Option<&DeliveryCapability>,
    harness_rows: &[InstalledHarness],
    observation_rows: &[AvailabilityObservation],
    now_ms: u64,
) -> Result<()> {
    let mut credentials = BTreeSet::new();
    for credential in credential_rows {
        ensure(
            valid_allowed_backends(&credential.allowed_backends),
            "credential backend allowlist",
        )?;
        text(&credential.provider)?;
        text(&credential.account_reference)?;
        text(&credential.endpoint_reference)?;
        if let Some(owner) = &credential.material_owner {
            text(owner.id.as_str())?;
            text(owner.version.as_str())?;
        }
        ensure(
            credential.revision > 0
                && credentials.insert(format!(
                    "{}@{}",
                    credential.profile.id.as_str(),
                    credential.profile.version.as_str()
                )),
            "credential revision/identity conflict",
        )?;
    }
    if let Some(delivery) = delivery {
        text(delivery.adapter.id.as_str())?;
        text(delivery.adapter.version.as_str())?;
        ensure(delivery.version > 0, "delivery protocol version")?;
    }
    let mut installed_credentials = BTreeSet::new();
    for binding in bindings {
        if let Some(preparation) = &binding.preparation {
            for reference in [
                &preparation.owner,
                &preparation.credential_profile,
                &preparation.delivery.adapter,
            ] {
                text(reference.id.as_str())?;
                text(reference.version.as_str())?;
            }
            target(&preparation.target)?;
            ensure(
                preparation.credential_revision > 0
                    && preparation.delivery.version > 0
                    && preparation.observed_at_ms > 0
                    && preparation.expires_at_ms > preparation.observed_at_ms
                    && preparation.expires_at_ms - preparation.observed_at_ms <= 120_000,
                "preparation evidence bounds",
            )?;
        }
        ensure(
            binding.credential_revision > 0
                && installed_credentials.insert((
                    binding.credential_profile.digest.clone(),
                    binding.credential_revision,
                )),
            "credential evidence revision/duplicate",
        )?;
    }
    let mut harnesses = BTreeSet::new();
    for harness in harness_rows {
        text(&harness.harness)?;
        text(&harness.cli_version)?;
        bounds(&harness.models, 64)?;
        bounds(&harness.capabilities, 64)?;
        ensure(
            harnesses.insert((
                harness.harness.clone(),
                harness.adapter.clone().digest,
                harness.environment.clone().digest,
            )),
            "duplicate harness identity",
        )?;
        let mut models = BTreeSet::new();
        for model in &harness.models {
            for value in [
                &model.provider,
                &model.model,
                &model.reasoning_configuration_reference,
                &model.native_reasoning,
            ] {
                text(value)?;
            }
            ensure(
                models.insert((
                    &model.provider,
                    &model.model,
                    &model.reasoning_configuration_reference,
                )),
                "conflicting model mapping",
            )?;
        }
        for capability in &harness.capabilities {
            capability
                .validate()
                .map_err(|e| SelectionError::Invalid(e.to_string()))?;
        }
    }
    let mut observations = BTreeSet::new();
    for observation in observation_rows {
        observation.validate()?;
        ensure(
            observation.received_at_ms <= now_ms && observations.insert(observation.observation_id),
            "future or duplicate observation",
        )?;
    }
    Ok(())
}
