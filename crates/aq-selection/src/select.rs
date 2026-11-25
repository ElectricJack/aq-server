use crate::candidate_core::*;
use crate::validate::fresh;
use crate::*;
mod budget;
use aq_profiles::SelectionPolicy;
use budget::{cost, global_budget};
/// Upper bound for a live offer decision and its server-owned budget snapshot.
/// Actual input expirations can shorten this window; admission must recheck them.
pub const OFFER_SELECTION_TTL_MS: u64 = 30_000;

/// Select for a live native offer among the candidates of the launch a role
/// snapshot resolved (spec 0014), on the harnesses the node has installed.
/// The decision allows time for poll, proposal and accept.
pub fn select_for_offer(
    launch: &aq_profiles::roles::Launch<'_>,
    input: &SelectionInputs,
) -> Result<SelectionOutcome> {
    crate::validate::validate_launch(launch, input)?;
    let policy = &launch.selection_policy;
    policy
        .validate()
        .map_err(|e| SelectionError::Invalid(e.to_string()))?;
    let prepared: Vec<_> = (input.launches.iter())
        .map(|l| CoreLaunch {
            digest: &l.digest,
            harness: &l.harness,
            environment: &l.environment,
            context_limit_bytes: l.startup_limit_bytes,
            startup_bytes: l.startup_bytes,
        })
        .collect();
    let mut profile = CoreProfile::launch(launch);
    // Spec 0017 §5.4: the frozen binding allows exactly the placed environment.
    if profile.environments.is_empty() {
        profile.environments = &input.project.allowed_environments;
    }
    finish(&profile, policy, &prepared, input)
}
/// The evaluation and the sealed result.
fn finish(
    profile: &CoreProfile<'_>,
    policy: &SelectionPolicy,
    launches: &[CoreLaunch<'_>],
    input: &SelectionInputs,
) -> Result<SelectionOutcome> {
    let delivery = |credential: &AuthorizedCredential| credential_delivery(input, credential);
    let cost_check = |harness: &InstalledHarness, estimate: Option<&CandidateEstimate>| {
        cost(input, policy, harness, estimate)
    };
    let target = match &input.project.target {
        ExecutionTarget::Worker { node_id, boot_id } => ObservationTarget::Worker {
            node_id: *node_id,
            boot_id: *boot_id,
        },
        ExecutionTarget::Central { .. } => ObservationTarget::CentralInstallation,
    };
    let core = evaluate(
        profile,
        policy,
        &CoreInputs {
            backend_kind: input.node.backend_kind,
            target,
            installation: CoreInstallation {
                harnesses: &input.node.harnesses,
                observed_at_ms: input.node.observed_at_ms,
                expires_at_ms: input.node.expires_at_ms,
            },
            credentials: &input.credentials,
            observations: &input.observations,
            estimates: &input.estimates,
            launches,
            ceilings: &input.project.ceilings,
            allowed_environments: &input.project.allowed_environments,
            placement: input.placement.as_ref(),
            route: input.route.as_ref(),
            now_ms: input.now_ms,
            global: global_budget(input, policy),
            environment_denied: "project_environment_denied",
            installation_stale: "node_capability_snapshot_stale",
            delivery: &delivery,
            cost: &cost_check,
        },
    )?;
    seal_outcome(core, input, policy)
}

fn seal_outcome(
    core: CoreOutcome,
    input: &SelectionInputs,
    policy: &SelectionPolicy,
) -> Result<SelectionOutcome> {
    let traces = core.traces;
    let resolved = if let Some((choice, credential_revision, index)) = core.selected {
        let expires = binding_expiry(input, &choice, credential_revision);
        let expires = influencing_expiry(input, policy, &traces[..=index], expires);
        Some(ResolvedSelection {
            schema_version: 1,
            lineage: input.project.lineage.clone(),
            target: input.project.target.clone(),
            choice,
            snapshot_digest: digest(input)?,
            role_snapshot_digest: input.project.role_snapshot_digest.clone(),
            project_revision: input.project.revision,
            node_revision: input.node.revision,
            registry_revision: input.registry_revision,
            credential_revision,
            budget_revision: input.budget.revision,
            selected_at_ms: input.now_ms,
            expires_at_ms: expires,
            traces: traces.clone(),
        })
    } else {
        None
    };
    Ok(SelectionOutcome {
        reason_codes: core.reason_codes,
        resolved,
        traces,
    })
}

fn binding_expiry(
    input: &SelectionInputs,
    choice: &ConcreteChoice,
    credential_revision: u64,
) -> u64 {
    let mut expires = input
        .node
        .expires_at_ms
        .min(input.budget.expires_at_ms)
        .min(input.project.expires_at_ms)
        .min(input.now_ms.saturating_add(OFFER_SELECTION_TTL_MS));
    for binding in &input.node.credential_bindings {
        if binding.credential_profile == choice.credential_profile
            && binding.credential_revision == credential_revision
        {
            expires = expires.min(binding.expires_at_ms);
            if choice.credential_delivery.is_some()
                && let Some(preparation) = &binding.preparation
            {
                expires = expires.min(preparation.expires_at_ms);
            }
        }
    }
    expires
}

fn credential_delivery(
    input: &SelectionInputs,
    credential: &AuthorizedCredential,
) -> Option<Option<CredentialDelivery>> {
    let mut installed = false;
    for binding in &input.node.credential_bindings {
        if !binding.matches_current(credential, input.now_ms) {
            continue;
        }
        installed |= binding.available;
        if let Some(delivery) = prepared_delivery(binding, credential, input) {
            return Some(Some(delivery));
        }
    }
    installed.then_some(None)
}

fn prepared_delivery(
    binding: &CredentialBinding,
    credential: &AuthorizedCredential,
    input: &SelectionInputs,
) -> Option<CredentialDelivery> {
    let preparation = binding.preparation.as_ref()?;
    let owner = credential.material_owner.as_ref()?;
    let supported = input.node.material_delivery.as_ref()?;
    (preparation.owner == *owner
        && preparation.credential_profile == credential.profile
        && preparation.credential_revision == credential.revision
        && preparation.target == input.project.target
        && matches!(preparation.target, ExecutionTarget::Worker { .. })
        && preparation.supports_delivery(supported, input.now_ms))
    .then(|| CredentialDelivery::ServerLease {
        owner: owner.clone(),
        adapter: supported.adapter.clone(),
        version: 1,
    })
}
impl ProvisioningReadiness {
    fn supports_delivery(&self, supported: &DeliveryCapability, now_ms: u64) -> bool {
        self.delivery.adapter == supported.adapter
            && self.delivery.version == 1
            && supported.version == 1
            && fresh(self.observed_at_ms, self.expires_at_ms, now_ms)
    }
}

impl CredentialBinding {
    fn matches_current(&self, credential: &AuthorizedCredential, now_ms: u64) -> bool {
        self.credential_profile == credential.profile
            && self.credential_revision == credential.revision
            && fresh(self.observed_at_ms, self.expires_at_ms, now_ms)
    }
}
