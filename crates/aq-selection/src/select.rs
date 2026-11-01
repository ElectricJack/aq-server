use crate::candidate_core::*;
use crate::validate::{fresh, validate};
use crate::*;
use aq_profiles::{SelectionPolicy, UnknownCostPolicy};
fn global_budget(input: &SelectionInputs, policy: &SelectionPolicy) -> Option<&'static str> {
    let budget = &input.budget;
    if input.now_ms >= input.project.expires_at_ms {
        return Some("project_admission_snapshot_stale");
    }
    if !fresh(budget.observed_at_ms, budget.expires_at_ms, input.now_ms) {
        return Some("lineage_budget_stale");
    }
    if budget.attempts_started >= policy.max_attempts {
        return Some("lineage_retry_budget_exhausted");
    }
    if budget.elapsed_ms.is_none()
        || budget
            .elapsed_ms
            .is_some_and(|ms| ms >= policy.max_lineage_elapsed_ms)
    {
        return Some("lineage_time_budget_exhausted_or_unknown");
    }
    if budget.attempts_started > 0 && budget.last_attempt_id.is_none() {
        return Some("lineage_predecessor_unknown");
    }
    if budget.attempts_started > 0
        && input.replacement.as_ref().is_none_or(|e| {
            !fresh(e.observed_at_ms, e.expires_at_ms, input.now_ms)
                || Some(e.predecessor_attempt_id) != budget.last_attempt_id
        })
    {
        return Some("replacement_stop_retention_reconciliation_required");
    }
    None
}
fn cost(
    input: &SelectionInputs,
    policy: &SelectionPolicy,
    harness: &InstalledHarness,
    estimate: Option<&CandidateEstimate>,
) -> std::result::Result<(), (CandidateState, &'static str)> {
    let project = &input.project.ceilings;
    let p = &policy.budget;
    let budget = &input.budget;
    let hard = project.require_hard_cost_enforcement || p.require_hard_cost_enforcement;
    if hard
        && (harness.limits.hard_cost_enforcer.is_none()
            || budget.hard_budget_provider.is_none()
            || budget.accounting != AccountingCompleteness::Complete)
    {
        return Err((
            CandidateState::Unsupported,
            "hard_cost_enforcement_unqualified",
        ));
    }
    if p.require_complete_accounting && budget.accounting != AccountingCompleteness::Complete {
        return Err((CandidateState::PolicyDenied, "complete_accounting_required"));
    }
    if project.currency.is_some() && p.currency.is_some() && project.currency != p.currency {
        return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
    }
    let limit = match (project.cost_limit_microunits, p.max_cost_microunits) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let currency = project.currency.as_ref().or(p.currency.as_ref());
    if let Some(limit) = limit {
        if budget
            .currency
            .as_ref()
            .is_some_and(|known| Some(known) != currency)
            || estimate.is_some_and(|e| Some(&e.currency) != currency)
        {
            return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
        }
        if budget.spent_microunits.is_some_and(|spent| spent >= limit) {
            return Err((
                CandidateState::PolicyDenied,
                "lineage_cost_budget_exhausted",
            ));
        }
        if budget.spent_microunits.is_none()
            || estimate.is_none()
            || budget.accounting != AccountingCompleteness::Complete
        {
            if !hard
                && !p.require_complete_accounting
                && p.unknown_cost == UnknownCostPolicy::AllowBounded
            {
                return Ok(());
            }
            return Err((CandidateState::PolicyDenied, "cost_unknown"));
        }
        let estimate = estimate.unwrap();
        if budget.currency.as_ref() != currency || Some(&estimate.currency) != currency {
            return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
        }
        if budget
            .spent_microunits
            .unwrap()
            .checked_add(estimate.microunits)
            .is_none_or(|sum| sum > limit)
        {
            return Err((
                CandidateState::PolicyDenied,
                "lineage_cost_budget_exhausted",
            ));
        }
    }
    Ok(())
}
pub fn select(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
) -> Result<SelectionOutcome> {
    for composition in &input.compositions {
        composition.composition.validate_native_v2().map_err(|_| {
            SelectionError::Invalid("native selection requires schema2 composition".into())
        })?;
    }
    select_internal(catalog, input)
}
/// Historical schema1 inspection/replay compatibility only. Never use this
/// entrypoint for a new native admission or offer fence.
pub fn select_legacy_for_inspection(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
) -> Result<SelectionOutcome> {
    ensure(
        input.compositions.iter().all(|c| {
            c.composition.schema_version == 1
                && c.composition.graph_access_policy_digest.is_none()
                && c.composition.graph_access_policy_kind.is_none()
                && c.composition.requested_permissions.is_none()
        }),
        "legacy composition shape",
    )?;
    select_internal(catalog, input)
}
fn select_internal(
    catalog: &aq_profiles::ValidatedCatalog,
    input: &SelectionInputs,
) -> Result<SelectionOutcome> {
    validate(catalog, input)?;
    let definitions = &catalog.artifact().catalog;
    let agent = definitions
        .agents
        .iter()
        .find(|a| aq_profiles::key(&a.id, a.version) == input.project.agent_profile)
        .ok_or_else(|| SelectionError::Invalid("agent absent".into()))?;
    ensure(
        agent.intelligence == input.project.intelligence_profile
            && agent.execution == input.project.execution_profile,
        "profile combination mismatch",
    )?;
    let intelligence = definitions
        .intelligence
        .iter()
        .find(|i| aq_profiles::key(&i.id, i.version) == agent.intelligence)
        .unwrap();
    let execution = definitions
        .execution
        .iter()
        .find(|e| aq_profiles::key(&e.id, e.version) == agent.execution)
        .unwrap();
    let Some(policy) = &intelligence.selection_policy else {
        return Ok(SelectionOutcome {
            resolved: None,
            traces: vec![],
            reason_codes: vec!["selection_policy_missing".into()],
        });
    };
    policy
        .validate()
        .map_err(|e| SelectionError::Invalid(e.to_string()))?;
    let compositions: Vec<_> = input
        .compositions
        .iter()
        .map(|c| CoreComposition {
            digest: &c.digest,
            harness: &c.composition.harness,
            environment: &c.composition.environment,
            context_limit_bytes: c.composition.context_limit_bytes,
            startup_bytes: c
                .composition
                .instruction_order
                .iter()
                .filter(|i| i.delivery == aq_profiles::Delivery::Startup)
                .map(|i| i.text.len() as u64)
                .sum(),
        })
        .collect();
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
        intelligence,
        execution,
        policy,
        &CoreInputs {
            target,
            installation: CoreInstallation {
                harnesses: &input.node.harnesses,
                observed_at_ms: input.node.observed_at_ms,
                expires_at_ms: input.node.expires_at_ms,
            },
            credentials: &input.credentials,
            observations: &input.observations,
            estimates: &input.estimates,
            compositions: &compositions,
            ceilings: &input.project.ceilings,
            allowed_environments: &input.project.allowed_environments,
            now_ms: input.now_ms,
            global: global_budget(input, policy),
            environment_denied: "project_environment_denied",
            installation_stale: "node_capability_snapshot_stale",
            delivery: &delivery,
            cost: &cost_check,
        },
    )?;
    let traces = core.traces;
    let resolved = if let Some((choice, credential_revision, index)) = core.selected {
        let mut expires = input
            .node
            .expires_at_ms
            .min(input.budget.expires_at_ms)
            .min(input.project.expires_at_ms)
            .min(input.now_ms.saturating_add(2000));
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
        expires = influencing_expiry(
            &input.observations,
            input.now_ms,
            policy,
            &traces,
            index,
            expires,
        );
        Some(ResolvedSelection {
            schema_version: 1,
            lineage: input.project.lineage.clone(),
            target: input.project.target.clone(),
            choice,
            snapshot_digest: digest(input)?,
            catalog_digest: input.project.catalog_digest.clone(),
            project_revision: input.project.revision,
            catalog_revision: input.project.catalog_revision,
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

fn credential_delivery(
    input: &SelectionInputs,
    credential: &AuthorizedCredential,
) -> Option<Option<CredentialDelivery>> {
    let mut installed = false;
    for binding in &input.node.credential_bindings {
        if binding.credential_profile != credential.profile
            || binding.credential_revision != credential.revision
            || !fresh(binding.observed_at_ms, binding.expires_at_ms, input.now_ms)
        {
            continue;
        }
        installed |= binding.available;
        if let (Some(preparation), Some(owner), Some(supported)) = (
            &binding.preparation,
            &credential.material_owner,
            &input.node.material_delivery,
        ) && preparation.owner == *owner
            && preparation.credential_profile == credential.profile
            && preparation.credential_revision == credential.revision
            && preparation.target == input.project.target
            && matches!(preparation.target, ExecutionTarget::Worker { .. })
            && preparation.delivery.adapter == supported.adapter
            && preparation.delivery.version == 1
            && supported.version == 1
            && fresh(
                preparation.observed_at_ms,
                preparation.expires_at_ms,
                input.now_ms,
            )
        {
            return Some(Some(CredentialDelivery::ServerLease {
                owner: owner.clone(),
                adapter: supported.adapter.clone(),
                version: 1,
            }));
        }
    }
    installed.then_some(None)
}
