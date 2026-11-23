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
        self.validate_scope()?;
        self.validate_timing()?;
        self.validate_source()
    }

    fn validate_scope(&self) -> Result<()> {
        text(&self.scope.provider)?;
        text(&self.scope.account_reference)?;
        text(&self.scope.endpoint_reference)?;
        if let Some(model) = &self.scope.model {
            text(model)?;
        }
        ensure(
            self.scope.node_id.is_none_or(|id| !id.is_nil()),
            "observation node",
        )
    }

    fn validate_timing(&self) -> Result<()> {
        ensure(
            self.received_at_ms > 0
                && self.expires_at_ms > self.received_at_ms
                && self.expires_at_ms - self.received_at_ms <= 3_600_000
                && self.cooldown_until_ms <= self.expires_at_ms,
            "observation timing",
        )
    }

    fn validate_source(&self) -> Result<()> {
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
/// A selection over the launch a role snapshot resolved (spec 0014): the
/// project names that role, level and execution profile, and each prepared
/// startup text is for one harness and environment.
pub(crate) fn validate_launch(
    launch: &aq_profiles::roles::Launch<'_>,
    input: &SelectionInputs,
) -> Result<()> {
    common(input)?;
    ensure(
        input.project.role == launch.role.id
            && input.project.intelligence_profile == launch.intelligence.id
            && input.project.execution_profile == launch.execution.id,
        "launch differs from the project snapshot",
    )?;
    let mut seen = BTreeSet::new();
    for prepared in &input.launches {
        text(&prepared.harness)?;
        text(&prepared.environment)?;
        ensure(
            seen.insert((&prepared.harness, &prepared.environment)),
            "duplicate launch combination",
        )?;
        ensure(
            prepared.startup_bytes > 0 && prepared.startup_bytes <= prepared.startup_limit_bytes,
            "launch startup bytes exceed their limit",
        )?;
    }
    Ok(())
}
/// The checks of the inputs themselves.
fn common(input: &SelectionInputs) -> Result<()> {
    validate_subject(input)?;
    if let Some(p) = &input.placement {
        placement(p)?;
    }
    for value in [
        &input.project.role,
        &input.project.intelligence_profile,
        &input.project.execution_profile,
    ] {
        text(value)?;
    }
    if let Some(r) = &input.route {
        for limit in [&r.harnesses, &r.providers, &r.prefer] {
            bounds(limit, 32)?;
            limit.iter().try_for_each(|v| text(v))?;
        }
    }
    validate_environments(&input.project.allowed_environments)?;
    validate_ceilings(&input.project.ceilings)?;
    validate_arrays(input)?;
    validate_installation(input)?;
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

fn validate_subject(input: &SelectionInputs) -> Result<()> {
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
            && input.node.revision > 0
            && input.registry_revision > 0
            && input.budget.revision > 0,
        "snapshot revisions",
    )
}

fn validate_environments(environments: &[String]) -> Result<()> {
    bounds(environments, 32)?;
    ensure(
        !environments.is_empty()
            && environments.iter().collect::<BTreeSet<_>>().len() == environments.len(),
        "project environment allowlist",
    )?;
    environments
        .iter()
        .try_for_each(|environment| text(environment))
}

fn validate_ceilings(c: &ResourceCeilings) -> Result<()> {
    ensure(
        (1..=1_048_576).contains(&c.context_bytes)
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
    Ok(())
}

fn validate_arrays(input: &SelectionInputs) -> Result<()> {
    bounds(&input.project.permission_ceiling, 128)?;
    bounds(&input.node.harnesses, crate::MAX_NODE_HARNESSES)?;
    bounds(&input.node.credential_bindings, 32)?;
    bounds(&input.credentials, 32)?;
    bounds(&input.observations, 128)?;
    bounds(&input.estimates, 128)?;
    bounds(&input.launches, 16)
}

/// Structural checks shared by subject adapters; these never authenticate facts.
fn validate_installation(input: &SelectionInputs) -> Result<()> {
    validate_credentials(&input.credentials)?;
    validate_delivery(input.node.material_delivery.as_ref())?;
    validate_bindings(&input.node.credential_bindings)?;
    validate_harnesses(&input.node.harnesses)?;
    validate_observations(&input.observations, input.now_ms)
}

fn validate_credentials(credential_rows: &[AuthorizedCredential]) -> Result<()> {
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
    Ok(())
}

fn validate_delivery(delivery: Option<&DeliveryCapability>) -> Result<()> {
    if let Some(delivery) = delivery {
        text(delivery.adapter.id.as_str())?;
        text(delivery.adapter.version.as_str())?;
        ensure(delivery.version > 0, "delivery protocol version")?;
    }
    Ok(())
}

fn validate_bindings(bindings: &[CredentialBinding]) -> Result<()> {
    let mut installed_credentials = BTreeSet::new();
    for binding in bindings {
        if let Some(preparation) = &binding.preparation {
            validate_preparation(preparation)?;
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
    Ok(())
}

fn validate_preparation(preparation: &ProvisioningReadiness) -> Result<()> {
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
    )
}

fn validate_harnesses(harness_rows: &[InstalledHarness]) -> Result<()> {
    let mut harnesses = BTreeSet::new();
    for harness in harness_rows {
        text(&harness.harness)?;
        text(&harness.cli_version)?;
        bounds(&harness.capabilities, 64)?;
        ensure(
            harnesses.insert((
                harness.harness.clone(),
                harness.adapter.clone().digest,
                harness.environment.clone().digest,
            )),
            "duplicate harness identity",
        )?;
        for capability in &harness.capabilities {
            capability
                .validate()
                .map_err(|e| SelectionError::Invalid(e.to_string()))?;
        }
    }
    Ok(())
}

fn validate_observations(observation_rows: &[AvailabilityObservation], now_ms: u64) -> Result<()> {
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
