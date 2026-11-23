//! Facts are constructed only after actual owning adapters and final SQL fences.
//! DTO association validation is additional validation, not the source of truth.
use super::phase::{FactBody, FactValue, FreshConfirmation, OwnerFact, PrepareRequest};
use super::*;
use aq_admission::Text;
use uuid::Uuid;
fn reference(id: String, digest: Digest) -> Result<ArtifactRef, AppError> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| refused())?,
        version: Text::new("1").map_err(|_| refused())?,
        digest,
    })
}
pub(super) fn confirmation(
    p: &pinned::Pinned,
    q: &PrepareRequest,
    o: &observations::Observed,
    c: &current::Current,
    budget_revision: u64,
    time: u64,
) -> Result<FreshConfirmation, AppError> {
    o.validate_for(&p.input, time)?;
    c.fresh_at(time)?;
    let proposal = &p.history.proposal;
    let confirmation_id = Uuid::new_v4();
    let a = &p.history.receipt;
    let retention = reference(
        format!("aq.phase-retention/{confirmation_id}"),
        digest(&o.source)?,
    )?;
    let provider_observation = reference(
        format!("aq.phase-availability/{confirmation_id}"),
        digest(&c.availability)?,
    )?;
    let source_receipt = |name: &str| {
        q.original
            .artifacts
            .iter()
            .find(|r| {
                r.artifact.id.as_str()
                    == format!("aq.source-{name}/{}", p.input.source.manifest().transfer_id)
            })
            .cloned()
            .ok_or_else(|| refused())
    };
    let values = vec![
        (
            p.input.spec.project_revision,
            time,
            o.binding.binding_observation.expires_at_ms,
            FactValue::Project {
                project_id: p.input.spec.lifecycle.project_id,
                revision: p.input.spec.project_revision,
                catalog: p.input.offer.offered.offer.manifest.catalog.clone(),
                permissions_digest: proposal.choice.permission_digest.clone(),
            },
        ),
        (
            c.registry.version.as_str().parse().map_err(|_| refused())?,
            c.checked_at_ms,
            u64::MAX,
            super::registry_fact(p, c),
        ),
        (
            c.principal
                .version
                .as_str()
                .parse()
                .map_err(|_| refused())?,
            c.checked_at_ms,
            u64::MAX,
            FactValue::Principal {
                principal: c.principal.clone(),
                scope: p.input.spec.graph_access_profile.clone(),
            },
        ),
        (
            c.inventory.revision,
            c.checked_at_ms,
            c.inventory_expires_at_ms(),
            FactValue::Inventory {
                node_id: a.lease.identity.node_id,
                boot_id: a.lease.identity.boot_id,
                sequence: c.inventory_sequence,
                model: proposal.choice.model.clone(),
                harness: proposal.choice.harness.clone(),
                capabilities: p.capabilities.clone(),
                state: phase::InventoryState::Installed,
                credential_semantics: phase::GlobalCredentialSemantics::ProvisionableOnly,
                credential_profile: p.input.material.credential_profile.clone(),
                credential_revision: p.input.material.credential_revision,
            },
        ),
        (
            c.availability
                .iter()
                .map(|x| x.revision)
                .max()
                .ok_or_else(|| refused())?,
            c.checked_at_ms,
            c.availability_expires_at_ms,
            FactValue::Availability {
                provider: Text::new(p.input.material.provider.as_str()).map_err(|_| refused())?,
                account_reference: Text::new(p.input.material.account_reference.as_str())
                    .map_err(|_| refused())?,
                endpoint_reference: Text::new(p.input.material.endpoint_reference.as_str())
                    .map_err(|_| refused())?,
                state: phase::AvailabilityState::Ready,
                provider_observation,
            },
        ),
        (
            budget_revision,
            time,
            u64::MAX,
            FactValue::Budget {
                lineage_id: p.input.spec.lifecycle.lineage_id,
                // Each attempt has its own window, the execution profile's
                // wall time, and has used none of it when it is granted.
                elapsed_ms: Some(0),
                maximum_lineage_elapsed_ms: p
                    .input
                    .offer
                    .offered
                    .offer
                    .manifest
                    .resources
                    .wall_time_ms,
                remaining_cost_microunits: None,
            },
        ),
        (
            p.input.spec.task.version,
            o.graph
                .ready_requested_at_ms
                .min(o.graph.context_requested_at_ms),
            o.graph
                .ready_expires_at_ms
                .min(o.graph.context_expires_at_ms),
            FactValue::Context {
                task: p.input.spec.task.clone(),
                context: p.input.spec.task_context.clone(),
                full_context_digest: p.input.full_context_digest.clone(),
                graph_policy: p.input.spec.graph_access_profile.clone(),
            },
        ),
        (
            p.input.spec.source.repository_binding_generation,
            o.source.observed_at_ms(),
            o.source.expires_at_ms(),
            FactValue::Git {
                repository_id: p.input.spec.source.repository_id,
                binding_generation: p.input.spec.source.repository_binding_generation,
                base: p.input.spec.source.resolved_object.clone(),
                policy: p.input.spec.source.policy.clone(),
                retention,
                state: phase::RetentionState::Active,
                manifest: source_receipt("manifest")?,
                bundle: source_receipt("bundle")?,
            },
        ),
        (
            p.input.material.credential_revision,
            o.file_started_at_ms,
            o.file_started_at_ms
                .checked_add(10_000)
                .ok_or_else(|| refused())?,
            FactValue::PrivateMaterialBacking {
                lease_id: p.input.material.lease_id,
                backing_digest: o.file.digest.clone(),
                installation_receipt_digest: digest(&q.original.materials[0])?,
            },
        ),
    ];
    let mut facts = Vec::with_capacity(9);
    for (revision, observed, ceiling, value) in values {
        let kind = value.kind();
        let authority = proposal
            .owners
            .iter()
            .find(|x| x.kind == kind)
            .ok_or_else(|| refused())?
            .authority
            .clone();
        let body = FactBody {
            authority,
            binding: a.lease.binding.clone(),
            choice_digest: proposal.choice.digest().map_err(|_| refused())?,
            revision,
            observed_at_ms: observed,
            expires_at_ms: observed
                .checked_add(proposal.policy.maximum_observation_ms)
                .ok_or_else(|| refused())?
                .min(ceiling),
            value,
        };
        let fact = OwnerFact {
            record: ArtifactRef {
                id: Text::new(format!("aq.phase-fact/{confirmation_id}/{kind:?}"))
                    .map_err(|_| refused())?,
                version: Text::new(revision.to_string()).map_err(|_| refused())?,
                digest: OwnerFact::record_digest(&body).map_err(|_| refused())?,
            },
            body,
        };
        fact.validate_for(proposal, a, q, time)
            .map_err(|_| refused())?;
        facts.push(fact);
    }
    let result = FreshConfirmation {
        schema_version: 2,
        confirmation_id,
        prepare_digest: q.digest().map_err(|_| refused())?,
        accepted_receipt_digest: a.digest().map_err(|_| refused())?,
        checked_at_ms: time,
        facts,
    };
    result
        .validate_for(q, a, &p.history.request, proposal)
        .map_err(|_| refused())?;
    Ok(result)
}
