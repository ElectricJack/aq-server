//! Assemble only from real owned observations/decisions and retained bytes.
use super::{
    artifacts::{RetainedSet, canonical, reference},
    compose::Compiled,
    now, on_heap, refused, startup,
};
use crate::{
    AppState,
    agent_access::scheduling::{ReadyPage, TaskContext},
    error::AppError,
    execution::profiled::{Effects, ProfiledState, Retention, Status, Stop},
    intelligence::resolver::Decision,
    profiled_execution::{
        artifacts::OwnerKind, registration::RegisteredRuntime, source::Observation,
    },
};
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, SourceSelection, TaskRevision, Text,
    execution_spec::ProfiledExecutionSpec,
    outcome::{Criterion, ExpectedOutcomeDefinition, OutcomeKind, SourceAnchor},
    startup::NativeOffer,
    transport::{DeliveryClock, Feature, OfferedLaunch},
};
use aq_selection::ResolvedSelection;
use aq_session_protocol as wire;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;
pub struct Built {
    pub runtime: RegisteredRuntime,
    pub native: NativeOffer,
    pub spec: ProfiledExecutionSpec,
    pub state: ProfiledState,
    pub retained: RetainedSet,
    pub graph_profile: Value,
    pub context: Value,
    pub central_observation: Value,
    pub source: Observation,
    pub decision: Decision,
    pub deadline: u64,
}
fn scope(p: &aq_session_api::Permission) -> Result<wire::Scope, AppError> {
    Ok(wire::Scope {
        operation: Text::new(&p.operation).map_err(|_| refused())?,
        resource: Text::new(&p.scope_reference).map_err(|_| refused())?,
    })
}
fn artifact(source: &aq_session_api::Source) -> Result<ArtifactRef, AppError> {
    Ok(ArtifactRef {
        id: Text::new(&source.identity).map_err(|_| refused())?,
        version: Text::new(&source.revision).map_err(|_| refused())?,
        digest: serde_json::from_value(json!(source.digest)).map_err(|_| refused())?,
    })
}
/// Offer capabilities; `[2..]` is the manifest's harness list, which carries
/// exactly one confinement capability for the frozen confinement mode.
fn capabilities(mode: aq_selection::ConfinementMode) -> Result<Vec<wire::Capability>, AppError> {
    [
        "aq.profiled-launch",
        "aq.terminal",
        "aq.harness.startup-context",
        mode.capability(),
        "aq.harness.stop",
    ]
    .into_iter()
    .map(|id| {
        Ok(wire::Capability {
            id: Text::new(id).map_err(|_| refused())?,
            version: 1,
        })
    })
    .collect()
}
fn criteria(context: &Value, task: Uuid) -> Result<Vec<Criterion>, AppError> {
    let root = context["nodes"]
        .as_array()
        .and_then(|a| a.iter().find(|n| n["id"] == task.to_string()))
        .ok_or_else(|| refused())?;
    let acceptance = root["acceptance_criteria"]
        .as_str()
        .ok_or_else(|| refused())?;
    let body = if acceptance.trim().is_empty() {
        root["body"].as_str().ok_or_else(|| refused())?
    } else {
        acceptance
    };
    if body.trim().is_empty() || body.len() > 16384 {
        return Err(refused());
    }
    let mut result = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let mut end = rest.len().min(8192);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        result.push(Criterion {
            id: aq_admission::Label::new(format!("authorized-task-text-{}", result.len() + 1))
                .map_err(|_| refused())?,
            text: rest[..end].into(),
        });
        rest = &rest[end..];
    }
    Ok(result)
}
async fn catalog_definitions(
    state: &AppState,
    set: &mut RetainedSet,
    compiled: &Compiled,
) -> Result<(ArtifactRef, Vec<ArtifactRef>), AppError> {
    let bytes = compiled.catalog.canonical_bytes().map_err(|_| refused())?;
    let catalog_ref = reference("aq.profile-catalog", &bytes)?;
    set.retain(
        state,
        OwnerKind::Catalog,
        ArtifactKind::Definition,
        catalog_ref.clone(),
        &bytes,
    )
    .await?;
    let composition = &compiled.candidates[0].composition;
    let mut sources = vec![
        composition.agent_profile.clone(),
        composition.intelligence_profile.clone(),
        composition.execution_profile.clone(),
    ];
    sources.extend(
        composition
            .instruction_order
            .iter()
            .filter(|i| i.source.identity.starts_with("module:"))
            .map(|i| i.source.clone()),
    );
    let mut modules = Vec::new();
    let mut seen = BTreeSet::new();
    for source in sources {
        if !seen.insert(source.identity.clone()) {
            continue;
        }
        let (kind, key) = source.identity.split_once(':').ok_or_else(|| refused())?;
        let c = &compiled.catalog.artifact().catalog;
        let value=match kind {
            "agent"=>serde_json::to_value(c.agents.iter().find(|v|aq_profiles::key(&v.id,v.version)==key).ok_or_else(|| refused())?),
            "intelligence"=>serde_json::to_value(c.intelligence.iter().find(|v|aq_profiles::key(&v.id,v.version)==key).ok_or_else(|| refused())?),
            "execution"=>serde_json::to_value(c.execution.iter().find(|v|aq_profiles::key(&v.id,v.version)==key).ok_or_else(|| refused())?),
            "module"=>{let metadata=c.modules.iter().find(|v|aq_profiles::key(&v.id,v.version)==key).ok_or_else(|| refused())?;Ok(json!({"metadata":metadata,"markdown":compiled.catalog.artifact().instructions.get(key).ok_or_else(|| refused())?}))},
            _=>return Err(refused()),
        }.map_err(|_|AppError::Internal)?;
        let bytes = canonical(&value)?;
        let r = artifact(&source)?;
        set.retain(
            state,
            OwnerKind::Catalog,
            ArtifactKind::Definition,
            r.clone(),
            &bytes,
        )
        .await?;
        if kind == "module" {
            modules.push(r);
        }
    }
    Ok((catalog_ref, modules))
}
pub struct Assembly<'a> {
    pub state: &'a AppState,
    pub runtime: &'a RegisteredRuntime,
    pub compiled: Compiled,
    pub decision: Decision,
    pub task: &'a TaskRevision,
    pub context: &'a TaskContext,
    pub ready: &'a ReadyPage,
    pub source: Observation,
    pub source_operation: Uuid,
    pub logical_identity: &'a str,
    pub generation: u64,
}
/// Re-derives the frozen confinement mode from the selection's placement facts
/// and the selected execution profile's confinement permission.
fn confinement_mode(
    compiled: &Compiled,
    composition: &aq_profiles::FrozenComposition,
    decision: &Decision,
    harness: &aq_selection::InstalledHarness,
) -> Result<aq_selection::ConfinementMode, AppError> {
    let key = composition
        .execution_profile
        .identity
        .strip_prefix("execution:")
        .ok_or_else(|| refused())?;
    let execution = compiled
        .catalog
        .artifact()
        .catalog
        .execution
        .iter()
        .find(|e| aq_profiles::key(&e.id, e.version) == key)
        .ok_or_else(|| refused())?;
    aq_selection::admit_confinement(
        decision.inputs.placement.as_ref(),
        &execution.confinement,
        harness,
    )
    .map_err(|_| refused())
}
pub async fn assemble(input: Assembly<'_>) -> Result<Built, AppError> {
    let Assembly {
        state,
        runtime,
        mut compiled,
        decision,
        task,
        context,
        ready,
        source,
        source_operation,
        logical_identity,
        generation,
    } = input;
    let selected: &ResolvedSelection = decision
        .outcome
        .resolved
        .as_ref()
        .ok_or_else(|| refused())?;
    if (decision.inputs.budget.attempts_started != 0 && decision.inputs.replacement.is_none())
        || decision
            .inputs
            .project
            .ceilings
            .require_hard_cost_enforcement
    {
        return Err(refused());
    }
    // A named ref is admitted as the exact commit it resolved to at offer time:
    // that commit is frozen into the export operation key, and the workflow
    // refuses an admission whose commit differs from the export's base (D1:
    // a ref-following project source). Anything else stays refused.
    if let SourceSelection::NamedRef { .. } = &source.admission.selection
        && (source.admission.resolved_object.format != aq_admission::ObjectFormat::Sha1
            || decision.inputs.replacement.is_some())
    {
        return Err(refused());
    }
    let index = compiled
        .candidates
        .iter()
        .position(|c| c.digest == selected.choice.composition_digest)
        .ok_or_else(|| refused())?;
    let composition = compiled.candidates.remove(index).composition;
    let policy = compiled.policies.remove(index);
    // The catalog definition helper must use this same selected composition.
    compiled.candidates = vec![aq_selection::PreparedComposition {
        digest: selected.choice.composition_digest.clone(),
        composition: composition.clone(),
    }];
    let configuration = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(|| refused())?;
    let (counted, fact) = super::super::context_renewal::evaluate(
        state,
        runtime,
        selected,
        &composition,
        now(state)?,
    )
    .await?;
    let fact = &fact;
    let until = (now(state)? / 86_400_000)
        .checked_add(8)
        .and_then(|n| n.checked_mul(86_400_000))
        .ok_or_else(|| refused())?;
    let mut set = RetainedSet::new(source.admission.project_id, until);
    let harness = decision
        .inputs
        .node
        .harnesses
        .iter()
        .find(|h| {
            h.harness == selected.choice.harness
                && h.cli_version == selected.choice.cli_version
                && h.adapter == selected.choice.adapter
                && h.environment == selected.choice.environment
        })
        .ok_or_else(|| refused())?;
    let mut needed = vec![
        runtime.runtime.clone(),
        runtime.qualification.clone(),
        harness.qualification.clone(),
        selected.choice.adapter.clone(),
        selected.choice.environment.clone(),
        selected.choice.credential_profile.clone(),
        fact.counting_evidence.clone(),
        fact.capacity_evidence.clone(),
    ];
    let project = &compiled.frozen.project.binding;
    let validation = ArtifactRef {
        id: Text::new(&project.policy.identity).map_err(|_| refused())?,
        version: Text::new(&project.policy.revision).map_err(|_| refused())?,
        digest: serde_json::from_value(json!(project.policy.digest)).map_err(|_| refused())?,
    };
    needed.push(validation.clone());
    let mut unique = Vec::new();
    let mut keys = BTreeSet::new();
    for r in needed {
        if keys.insert((r.id.clone(), r.version.clone())) {
            unique.push(r);
        } else if unique
            .iter()
            .any(|old: &ArtifactRef| old.id == r.id && old.version == r.version && old != &r)
        {
            return Err(refused());
        }
    }
    // D3: the default completion policy is settled by the policy actor.
    let review = configuration
        .definition_bodies
        .iter()
        .find(|d| d.artifact() == &validation)
        .map(|d| crate::profiled_execution::settlement::review_requirement(d.body()))
        .ok_or_else(|| refused())?;
    // Check every required actual body before writing any retained artifact.
    let definitions = unique
        .iter()
        .map(|r| {
            configuration
                .definition_bodies
                .iter()
                .find(|d| d.artifact() == r)
                .ok_or_else(|| refused())
        })
        .collect::<Result<Vec<_>, _>>()?;
    for definition in definitions {
        on_heap(|| {
            set.retain(
                state,
                OwnerKind::Composition,
                ArtifactKind::Definition,
                definition.artifact().clone(),
                definition.body().as_bytes(),
            )
        })
        .await?;
    }
    let (catalog, mut modules) =
        on_heap(|| catalog_definitions(state, &mut set, &compiled)).await?;
    let startup = on_heap(|| {
        startup::retain(
            state,
            &mut set,
            &composition,
            &counted,
            &compiled.projection,
            fact,
        )
    })
    .await?;
    modules.extend(startup.modules);
    let policy_ref = policy.artifact().map_err(|_| refused())?;
    let policy_bytes = canonical(&policy)?;
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Composition,
            ArtifactKind::Definition,
            policy_ref.clone(),
            &policy_bytes,
        )
    })
    .await?;
    modules.push(policy_ref);
    let composition_bytes = canonical(&composition)?;
    let composition_ref = reference("aq.frozen-composition", &composition_bytes)?;
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Composition,
            ArtifactKind::Composition,
            composition_ref,
            &composition_bytes,
        )
    })
    .await?;
    let model = aq_profiles::ResolvedGraphModel {
        provider: selected.choice.provider.clone(),
        model: selected.choice.model.clone(),
        native_reasoning: selected.choice.native_reasoning.clone(),
        reasoning_configuration_reference: selected
            .choice
            .reasoning_configuration_reference
            .clone(),
    };
    let profile = policy
        .resolve_profile(
            &model,
            &compiled.projection.context,
            &compiled.agent,
            &aq_profiles::GraphRetryBudget {
                max_attempts: 1,
                max_elapsed_ms: 5000,
                initial_delay_ms: 1,
                max_delay_ms: 1,
            },
        )
        .map_err(|_| refused())?;
    let profile_bytes = canonical(&profile)?;
    let profile_ref = ArtifactRef {
        id: Text::new(profile["profile_id"].as_str().ok_or_else(|| refused())?)
            .map_err(|_| refused())?,
        version: Text::new(profile["revision"].as_str().ok_or_else(|| refused())?)
            .map_err(|_| refused())?,
        digest: Digest::of(&profile_bytes),
    };
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Composition,
            ArtifactKind::Definition,
            profile_ref.clone(),
            &profile_bytes,
        )
    })
    .await?;
    let selection_bytes = canonical(selected)?;
    let selection_ref = reference("aq.sealed-selection", &selection_bytes)?;
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Selection,
            ArtifactKind::Selection,
            selection_ref.clone(),
            &selection_bytes,
        )
    })
    .await?;
    let source_retention_id =
        Uuid::new_v5(&source_operation, &canonical(&(&source.artifact, until))?);
    let source_retained = on_heap(|| {
        crate::profiled_execution::source::retain_admission(
            state,
            source_retention_id,
            &source,
            until,
        )
    })
    .await?;
    if set.entries.len() >= 126
        || set
            .entries
            .iter()
            .any(|(_, r)| r.artifact.digest == source_retained.artifact.digest)
        || set
            .entries
            .iter()
            .try_fold(source_retained.bytes, |sum, (_, r)| {
                sum.checked_add(r.bytes)
            })
            .is_none_or(|n| n > 16_777_216)
    {
        return Err(refused());
    }
    set.entries.push((source_retention_id, source_retained));
    let node = match selected.target {
        aq_selection::ExecutionTarget::Worker { node_id, boot_id } => (node_id, boot_id),
        _ => return Err(refused()),
    };
    let attempt_id = Uuid::new_v5(
        &source_operation,
        &canonical(&(decision.decision_id, node, generation))?,
    );
    let session = wire::SessionIdentity::Remote {
        node_id: node.0,
        boot_id: node.1,
        attempt_id,
        assignment_generation: generation,
        session_id: Uuid::new_v5(&attempt_id, b"native-session-v1"),
    };
    let lifecycle = aq_lifecycle_api::Binding {
        project_id: project.project_id,
        task_id: task.task_id,
        lineage_id: aq_admission::transport::task_lineage_id(
            project.project_id,
            task.workspace_id,
            task.task_id,
        )
        .map_err(|_| refused())?,
        attempt_id,
        attempt_generation: generation,
        session: session.clone(),
    };
    let outcome = ExpectedOutcomeDefinition {
        schema_version: 1,
        kind: OutcomeKind::SourceChangeReviewed,
        project_id: project.project_id,
        project_revision: compiled.frozen.project.revision,
        task: task.clone(),
        source: SourceAnchor {
            repository_id: source.admission.repository_id,
            binding_generation: source.admission.repository_binding_generation,
            base: source.admission.resolved_object.clone(),
            artifact: source.artifact.clone(),
        },
        task_observation: compiled.projection.artifact.clone(),
        task_context: compiled.projection.artifact.clone(),
        inputs: vec![compiled.projection.artifact.clone()],
        acceptance_criteria: criteria(&compiled.projection.context, task.task_id)?,
        validation_policy: validation,
        review,
        required_checks: vec![],
    };
    outcome.validate().map_err(|_| refused())?;
    let outcome_ref = outcome.artifact().map_err(|_| refused())?;
    let outcome_bytes = outcome.canonical_bytes().map_err(|_| refused())?;
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Outcome,
            ArtifactKind::Outcome,
            outcome_ref.clone(),
            &outcome_bytes,
        )
    })
    .await?;
    let spec = ProfiledExecutionSpec {
        schema_version: 1,
        lifecycle,
        project_revision: compiled.frozen.project.revision,
        task: task.clone(),
        source: source.admission.clone(),
        source_artifact: source.artifact.clone(),
        outcome_artifact: outcome_ref.clone(),
        graph_access_profile: profile_ref.clone(),
        task_observation: compiled.projection.artifact.clone(),
        task_context: compiled.projection.artifact.clone(),
        selection_digest: aq_selection::digest(selected).map_err(|_| AppError::Internal)?,
    };
    outcome.validate_against(&spec).map_err(|_| refused())?;
    let spec_ref = spec.artifact().map_err(|_| refused())?;
    let spec_bytes = spec.canonical_bytes().map_err(|_| refused())?;
    on_heap(|| {
        set.retain(
            state,
            OwnerKind::Composition,
            ArtifactKind::Composition,
            spec_ref.clone(),
            &spec_bytes,
        )
    })
    .await?;
    let requested = composition
        .requested_permissions
        .as_ref()
        .ok_or_else(|| refused())?
        .iter()
        .map(scope)
        .collect::<Result<Vec<_>, _>>()?;
    let effective = composition
        .effective_permissions
        .iter()
        .map(scope)
        .collect::<Result<Vec<_>, _>>()?;
    let mode = confinement_mode(&compiled, &composition, &decision, harness)?;
    let required_caps = capabilities(mode)?;
    let spending_budget = project
        .ceilings
        .cost_limit_microunits
        .map(|limit| {
            Ok::<_, AppError>(wire::SpendingBudget {
                currency: Text::new(
                    project
                        .ceilings
                        .currency
                        .as_ref()
                        .ok_or_else(|| refused())?,
                )
                .map_err(|_| refused())?,
                basis: wire::BudgetBasis::TaskCumulativeEstimatedCost,
                limit_microunits: limit,
            })
        })
        .transpose()?;
    let manifest = wire::LaunchManifest {
        schema_version: 1,
        catalog,
        modules,
        agent_profile: artifact(&composition.agent_profile)?,
        intelligence_profile: artifact(&composition.intelligence_profile)?,
        execution_profile: artifact(&composition.execution_profile)?,
        bindings: wire::LaunchBindings {
            session: session.clone(),
            project_id: project.project_id,
            project_revision: compiled.frozen.project.revision,
            graph_id: task.workspace_id,
            task_id: task.task_id,
            task_revision: task.version,
            attempt_id,
            assignment_generation: generation,
            source: source.artifact.clone(),
            outcome: outcome_ref,
            graph_access_profile: profile_ref.clone(),
        },
        model: wire::ModelSettings {
            provider: Text::new(&selected.choice.provider).map_err(|_| refused())?,
            model: Text::new(&selected.choice.model).map_err(|_| refused())?,
            reasoning: Text::new(&selected.choice.native_reasoning).map_err(|_| refused())?,
            max_output_tokens: selected.choice.max_output_tokens,
            spending_budget,
        },
        harness: wire::HarnessSettings {
            harness: Text::new(&selected.choice.harness).map_err(|_| refused())?,
            cli_version: Text::new(&selected.choice.cli_version).map_err(|_| refused())?,
            adapter: selected.choice.adapter.clone(),
            environment: selected.choice.environment.clone(),
            required_capabilities: required_caps[2..].to_vec(),
        },
        credentials: vec![wire::SecretRef {
            credential_profile: selected.choice.credential_profile.clone(),
            purpose: Text::new("model-authentication").map_err(|_| refused())?,
        }],
        permissions: wire::PermissionPlan {
            requested,
            effective: effective.clone(),
            required: effective,
            omissions: composition
                .omissions
                .iter()
                .map(|o| Text::new(&o.reason_code).map_err(|_| refused()))
                .collect::<Result<Vec<_>, _>>()?,
        },
        context: startup.context,
        resources: wire::ResourceLimits {
            memory_bytes: selected.choice.resources.memory_bytes,
            cpu_millis: selected.choice.resources.cpu_millis,
            process_limit: selected.choice.resources.process_limit,
            wall_time_ms: selected.choice.resources.wall_time_ms,
            stop_grace_ms: 5000.min(selected.choice.resources.wall_time_ms),
        },
        terminal: wire::TerminalPolicy {
            host: wire::TerminalHost::Tmux,
            input_enabled: super::result_startup::for_adapter(state, &selected.choice.adapter)?
                .is_some_and(|reporting| reporting.interactive),
            max_input_bytes: 4096,
            max_pending_input_bytes: 32768,
            max_frame_bytes: 65536,
            replay_bytes: 1048576,
        },
        selection_explanation: selection_ref,
    };
    let binding = wire::LaunchBinding {
        schema_version: 1,
        session,
        offer_id: Uuid::new_v5(&attempt_id, b"native-offer-v1"),
        attempt_id,
        assignment_generation: generation,
        execution_spec_digest: spec_ref.digest,
        launch_manifest_digest: wire::canonical_digest(&manifest).map_err(|_| refused())?,
        graph_access_profile_digest: profile_ref.digest.clone(),
    };
    let offer = wire::ProfiledOffer {
        binding,
        manifest,
        required_capabilities: required_caps,
    };
    let time = now(state)?;
    let deadline = [
        time.checked_add(30000).ok_or_else(|| refused())?,
        source.admission.expires_at_ms,
        selected.expires_at_ms,
        ready.stamp.expires_at_ms,
        context.stamp.expires_at_ms,
        counted.proof.expires_at_ms,
    ]
    .into_iter()
    .min()
    .ok_or_else(|| refused())?;
    let native = NativeOffer {
        schema_version: 1,
        offered: OfferedLaunch {
            lifecycle_binding: spec.lifecycle.clone(),
            offer: offer.clone(),
            clock: DeliveryClock::new(time, deadline).map_err(|_| refused())?,
            reservation_ttl_ms: 120000,
            required_features: vec![
                Feature::ArtifactTransferV1,
                Feature::ServerMaterialV1,
                Feature::DurableHostJournalV1,
            ],
            artifacts: set.descriptors(),
        },
        startup: startup.descriptor,
    };
    native.validate().map_err(|_| refused())?;
    spec.validate_retained(&offer, &native.offered.artifacts, time)
        .map_err(|_| refused())?;
    let source_binding = aq_worker_protocol::SourceBinding {
        repository_id: source.admission.repository_id,
        binding_generation: source.admission.repository_binding_generation,
        logical_identity: logical_identity.into(),
        base: aq_worker_protocol::ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: source.admission.resolved_object.hex.clone(),
        },
    };
    let graph = aq_worker_protocol::GraphBinding {
        workspace_id: task.workspace_id,
        profile_id: profile["profile_id"]
            .as_str()
            .ok_or_else(|| refused())?
            .into(),
        profile_revision: profile["revision"]
            .as_str()
            .ok_or_else(|| refused())?
            .into(),
        profile_digest: profile_ref.digest.as_str().into(),
        context_digest: compiled.projection.artifact.digest.as_str().into(),
    };
    let state_projection = ProfiledState {
        schema_version: 1,
        offer,
        status: Status::Offered,
        grant: None,
        execution_deadline_ms: None,
        source: source_binding,
        graph,
        cancellation_id: None,
        revoked: false,
        stop: Stop::Unknown,
        retention: Retention::Pending,
        effects: Effects::Unknown,
    };
    state_projection.validate().map_err(|_| refused())?;
    let central_observation = super::observation::StoredOffer {
        schema_version: 1,
        read_provenance: super::observation::ReadProvenance::capture(
            task,
            ready,
            context,
            compiled.projection.artifact.digest.clone(),
            time,
        )?,
        native_offer: native.clone(),
    }
    .value()?;
    Ok(Built {
        runtime: runtime.clone(),
        native,
        spec,
        state: state_projection,
        retained: set,
        graph_profile: profile,
        context: compiled.projection.context,
        central_observation,
        source,
        decision,
        deadline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_selection::ConfinementMode::{Container, Native};

    #[test]
    fn manifest_carries_exactly_one_confinement_capability() {
        for (mode, want, other) in [
            (
                Native,
                wire::NATIVE_CONFINEMENT_CAPABILITY,
                wire::CONTAINER_CONFINEMENT_CAPABILITY,
            ),
            (
                Container,
                wire::CONTAINER_CONFINEMENT_CAPABILITY,
                wire::NATIVE_CONFINEMENT_CAPABILITY,
            ),
        ] {
            let caps = capabilities(mode).unwrap();
            let harness: Vec<&str> = caps[2..].iter().map(|c| c.id.as_str()).collect();
            assert_eq!(
                harness,
                ["aq.harness.startup-context", want, "aq.harness.stop"]
            );
            assert!(!harness.contains(&other));
            wire::require_capabilities(&caps[2..], &caps).unwrap();
        }
    }
}
