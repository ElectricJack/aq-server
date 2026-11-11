//! Removing an explicit assignment hands selection back to AQ's normal router.
use super::*;
use aq_profiles::{FallbackReason, IntelligenceBudgetPolicy, SelectionPolicy, UnknownCostPolicy};
use std::collections::BTreeSet;

pub(super) fn remove(
    current: &ValidatedCatalog,
    state: &ActivationState,
    old: &Intelligence,
    choices: &[ModelChoice],
    project_uses_profile: bool,
) -> Result<Prepared> {
    let old_key = aq_profiles::key(&old.id, old.version);
    let mut catalog = current.artifact().catalog.clone();
    let needs_router =
        project_uses_profile || catalog.agents.iter().any(|a| a.intelligence == old_key);
    let mut references = BTreeMap::new();
    let mut routing = None;
    catalog.intelligence.retain(|p| p.id != old.id);
    if needs_router {
        let existing = current
            .artifact()
            .catalog
            .intelligence
            .iter()
            .find(|p| p.automatic_routing == Some(true));
        let mut router = automatic(current, state, old, choices, existing)?;
        let previous_router = existing.map(|p| aq_profiles::key(&p.id, p.version));
        let router_key = aq_profiles::key(&router.id, router.version);
        references.insert(format!("intelligence:{old_key}"), router_key.clone());
        if let Some(previous) = &previous_router
            && previous != &router_key
        {
            references.insert(format!("intelligence:{previous}"), router_key.clone());
        }
        let mut agents = Vec::new();
        for agent in &mut catalog.agents {
            if agent.intelligence != old_key
                && !previous_router
                    .as_ref()
                    .is_some_and(|key| key == &agent.intelligence && key != &router_key)
            {
                continue;
            }
            let previous = aq_profiles::key(&agent.id, agent.version);
            agent.version = version(state, "agent", &agent.id, agent.version)?;
            agent.intelligence = router_key.clone();
            references.insert(
                format!("agent:{previous}"),
                aq_profiles::key(&agent.id, agent.version),
            );
            agents.push(agent.id.clone());
        }
        catalog.intelligence.retain(|p| p.id != router.id);
        router.automatic_routing = Some(true);
        catalog.intelligence.push(router);
        routing = Some(
            json!({"mode":"automatic","profile_reference":router_key,"agent_profiles":agents}),
        );
    }
    let catalog = aq_profiles::validate(catalog, current.artifact().instructions.clone())
        .map_err(|_| invalid("The updated routing configuration is invalid"))?;
    Ok(Prepared {
        catalog,
        profile: old.clone(),
        references,
        changed: true,
        deleted: true,
        routing,
    })
}

fn automatic(
    current: &ValidatedCatalog,
    state: &ActivationState,
    deleted: &Intelligence,
    choices: &[ModelChoice],
    existing: Option<&Intelligence>,
) -> Result<Intelligence> {
    let mut candidates: Vec<_> = choices
        .iter()
        .filter(|c| !c.preference_only)
        .map(|c| ModelCandidate {
            id: String::new(),
            provider: c.provider.clone(),
            model: c.model.clone(),
            reasoning_configuration_reference: c.reasoning_configuration_reference.clone(),
            harnesses: vec![c.harness.clone()],
        })
        .collect();
    if candidates.is_empty()
        && let Some(router) = existing
    {
        candidates = router.candidates.clone();
    }
    if candidates.is_empty() {
        candidates = current
            .artifact()
            .catalog
            .intelligence
            .iter()
            .filter(|p| p.automatic_routing != Some(true))
            .flat_map(|p| p.candidates.clone())
            .collect();
    }
    if candidates.is_empty() {
        candidates = deleted.candidates.clone();
    }
    let mut seen = BTreeSet::new();
    candidates.retain(|c| {
        seen.insert((
            c.provider.clone(),
            c.model.clone(),
            c.reasoning_configuration_reference.clone(),
            c.harnesses.clone(),
        ))
    });
    candidates.truncate(aq_profiles::MAX_MODULES);
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.id = format!("route-{}", index + 1);
    }
    let mut policy = existing
        .and_then(|p| p.selection_policy.clone())
        .or_else(|| deleted.selection_policy.clone())
        .or_else(|| {
            current
                .artifact()
                .catalog
                .intelligence
                .iter()
                .find_map(|p| p.selection_policy.clone())
        })
        .unwrap_or(SelectionPolicy {
            schema_version: 1,
            fallback_on: Vec::new(),
            allow_unknown_availability: false,
            observation_max_age_ms: 30_000,
            max_output_tokens: 8192,
            max_attempts: 2,
            max_lineage_elapsed_ms: 3_600_000,
            required_capabilities: Vec::new(),
            budget: IntelligenceBudgetPolicy {
                max_cost_microunits: None,
                currency: None,
                require_complete_accounting: false,
                require_hard_cost_enforcement: false,
                unknown_cost: UnknownCostPolicy::AllowBounded,
            },
        });
    policy.fallback_on = vec![
        FallbackReason::MissingCredentials,
        FallbackReason::Unavailable,
        FallbackReason::Unsupported,
        FallbackReason::StaleUnknown,
        FallbackReason::PolicyDenied,
    ];
    let id = existing.map(|p| p.id.clone()).unwrap_or_else(|| {
        let mut name = "router".to_string();
        let mut suffix = 1;
        while current
            .artifact()
            .catalog
            .intelligence
            .iter()
            .any(|p| p.id == name && p.automatic_routing != Some(true))
        {
            name = format!("router-{suffix}");
            suffix += 1;
        }
        name
    });
    let mut router = Intelligence {
        id,
        version: existing.map_or(0, |p| p.version),
        name: Some("Automatic router".into()),
        description: Some("Model and provider selected automatically for future work.".into()),
        automatic_routing: Some(true),
        candidates,
        selection_policy: Some(policy),
    };
    if existing.is_none_or(|p| value(p).ok() != value(&router).ok()) || router.id == deleted.id {
        router.version = version(state, "intelligence", &router.id, router.version)?;
    }
    Ok(router)
}
