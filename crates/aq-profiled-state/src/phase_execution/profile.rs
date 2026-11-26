use super::{Error, decode, digest, same};
use crate::{hot::HotState, native_offer::ReadProvenance};
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, MaterialLease, Text, execution_spec::ProfiledExecutionSpec,
    startup::NativeOffer,
};
use aq_selection::{AvailabilityObservation, CapabilitySnapshot};
use aq_session_protocol::Capability;
use phase::{
    AcceptRequest, AcceptedReceipt, ExecutionReceipt, FactBody, FactValue, FreshConfirmation,
    OwnerFact, PrepareRequest, Proposal,
};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::Row;
use uuid::Uuid;

pub struct AcceptedDocuments<'a> {
    pub receipt: &'a AcceptedReceipt,
    pub request: &'a AcceptRequest,
    pub proposal: &'a Proposal,
}

pub struct IssuedDocuments<P> {
    pub request: PrepareRequest,
    pub receipt: ExecutionReceipt,
    pub receipt_digest: Digest,
    pub provenance: P,
}

impl<P: DeserializeOwned + Serialize> IssuedDocuments<P> {
    pub fn decode_row(row: &sqlx::postgres::PgRow) -> Result<Self, Error> {
        let request: PrepareRequest = decode(
            &row.get::<Vec<u8>, _>("request_bytes"),
            &row.get::<String, _>("request_bytes_digest"),
        )?;
        let receipt: ExecutionReceipt = decode(
            &row.get::<Vec<u8>, _>("receipt_bytes"),
            &row.get::<String, _>("receipt_bytes_digest"),
        )?;
        let provenance: P = decode(
            &row.get::<Vec<u8>, _>("provenance_bytes"),
            &row.get::<String, _>("provenance_digest"),
        )?;
        let receipt_digest = receipt.digest().map_err(|_| Error::Invalid)?;
        Ok(Self {
            request,
            receipt,
            receipt_digest,
            provenance,
        })
    }

    pub fn validate_row(
        &self,
        row: &sqlx::postgres::PgRow,
        node: Uuid,
        binding: &LaunchBinding,
        grant: Uuid,
        accepted: AcceptedDocuments<'_>,
    ) -> Result<(), Error> {
        let Self {
            request,
            receipt,
            receipt_digest,
            ..
        } = self;
        receipt
            .validate_for(
                request,
                accepted.receipt,
                accepted.request,
                accepted.proposal,
            )
            .map_err(|_| Error::Invalid)?;
        if row.get::<Uuid, _>("node_id") != node
            || row.get::<Uuid, _>("boot_id") != request.original.identity.boot_id
            || request.original.identity.node_id != node
            || receipt.grant.binding != *binding
            || receipt.grant.grant_id != grant
            || row.get::<Uuid, _>("grant_id") != grant
            || row.get::<Uuid, _>("operation_id") != request.original.operation_id
            || row.get::<String, _>("request_digest")
                != request.digest().map_err(|_| Error::Invalid)?.as_str()
            || row.get::<String, _>("receipt_digest") != receipt_digest.as_str()
            || row.get::<String, _>("accepted_receipt_digest")
                != accepted
                    .receipt
                    .digest()
                    .map_err(|_| Error::Invalid)?
                    .as_str()
            || row.get::<i64, _>("issued_at_ms") as u64 != receipt.issued_at_ms
            || row.get::<i64, _>("expires_at_ms") as u64 != receipt.grant.expires_at_ms
            || row.get::<i64, _>("execution_deadline_ms") as u64 != receipt.execution_deadline_ms
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

pub enum GrantMode<'a> {
    Fresh,
    Replay(&'a ExecutionReceipt),
    Continuing(&'a ExecutionReceipt),
}
impl<'a> GrantMode<'a> {
    fn issued(&self) -> Option<&'a ExecutionReceipt> {
        match self {
            Self::Fresh => None,
            Self::Replay(issued) | Self::Continuing(issued) => Some(issued),
        }
    }
}
pub fn require_hot_state(
    mode: GrantMode<'_>,
    hot: &HotState,
    q: &PrepareRequest,
    accepted_request: &AcceptRequest,
) -> Result<(), Error> {
    let Some(issued) = mode.issued() else {
        if hot.grant.is_some() || hot.execution_deadline_ms.is_some() {
            return Err(Error::Invalid);
        }
        return Ok(());
    };
    let live = hot.grant.as_ref().ok_or(Error::Invalid)?;
    let grant_matches = if matches!(mode, GrantMode::Continuing(_)) {
        live.binding == issued.grant.binding
            && live.grant_id == issued.grant.grant_id
            && live.expires_at_ms >= issued.grant.expires_at_ms
            && live.expires_at_ms <= issued.execution_deadline_ms
    } else {
        same(live, &issued.grant)?
    };
    if !grant_matches
        || hot.execution_deadline_ms != Some(issued.execution_deadline_ms)
        || hot.preparation_operation_id != Some(accepted_request.original.operation_id)
        || hot.owner.as_ref().is_none_or(|o| {
            o.guardian_id != q.original.guardian_id
                || o.launch_id != q.original.launch_id
                || o.reservation_id != q.original.reservation.reservation_id
        })
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

/// Projections of actual owning records. Construction grants no permission;
/// the server must validate observations and complete its SQL fences first.
pub struct FactInputs<'a> {
    pub proposal: &'a Proposal,
    pub accepted: &'a AcceptedReceipt,
    pub accepted_request: &'a AcceptRequest,
    pub request: &'a PrepareRequest,
    pub spec: &'a ProfiledExecutionSpec,
    pub offer: &'a NativeOffer,
    pub material: &'a MaterialLease,
    pub full_context_digest: &'a Digest,
    pub capabilities: &'a [Capability],
    pub registry_fact: FactValue,
    pub registry: &'a ArtifactRef,
    pub principal: &'a ArtifactRef,
    pub inventory: &'a CapabilitySnapshot,
    pub inventory_sequence: u64,
    pub inventory_expires_at_ms: u64,
    pub availability: &'a [AvailabilityObservation],
    pub availability_expires_at_ms: u64,
    pub checked_at_ms: u64,
    pub binding_expires_at_ms: u64,
    pub graph: &'a ReadProvenance,
    pub source_digest: Digest,
    pub source_observed_at_ms: u64,
    pub source_expires_at_ms: u64,
    pub transfer_id: Uuid,
    pub file_digest: &'a Digest,
    pub file_started_at_ms: u64,
}
fn reference(id: String, digest: Digest) -> Result<ArtifactRef, Error> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| Error::Invalid)?,
        version: Text::new("1").map_err(|_| Error::Invalid)?,
        digest,
    })
}
pub fn confirmation(
    inputs: &FactInputs<'_>,
    budget_revision: u64,
    time: u64,
    confirmation_id: Uuid,
) -> Result<FreshConfirmation, Error> {
    let values = observed_facts(inputs, budget_revision, time, confirmation_id)?;
    let mut facts = Vec::with_capacity(values.len());
    for value in values {
        facts.push(seal_fact(
            inputs.proposal,
            inputs.accepted,
            inputs.request,
            time,
            confirmation_id,
            value,
        )?);
    }
    let result = FreshConfirmation {
        schema_version: 2,
        confirmation_id,
        prepare_digest: inputs.request.digest().map_err(|_| Error::Invalid)?,
        accepted_receipt_digest: inputs.accepted.digest().map_err(|_| Error::Invalid)?,
        checked_at_ms: time,
        facts,
    };
    result
        .validate_for(
            inputs.request,
            inputs.accepted,
            inputs.accepted_request,
            inputs.proposal,
        )
        .map_err(|_| Error::Invalid)?;
    Ok(result)
}

struct FactObservation {
    revision: u64,
    observed_at_ms: u64,
    ceiling: u64,
    value: FactValue,
}

fn observed_facts(
    inputs: &FactInputs<'_>,
    budget_revision: u64,
    time: u64,
    confirmation_id: Uuid,
) -> Result<Vec<FactObservation>, Error> {
    let proposal = inputs.proposal;
    let a = inputs.accepted;
    let retention = reference(
        format!("aq.phase-retention/{confirmation_id}"),
        inputs.source_digest.clone(),
    )?;
    let provider_observation = reference(
        format!("aq.phase-availability/{confirmation_id}"),
        digest(&inputs.availability)?,
    )?;
    let source_receipt = |name: &str| {
        inputs
            .request
            .original
            .artifacts
            .iter()
            .find(|r| r.artifact.id.as_str() == format!("aq.source-{name}/{}", inputs.transfer_id))
            .cloned()
            .ok_or(Error::Invalid)
    };
    Ok(vec![
        FactObservation {
            revision: inputs.spec.project_revision,
            observed_at_ms: time,
            ceiling: inputs.binding_expires_at_ms,
            value: FactValue::Project {
                project_id: inputs.spec.lifecycle.project_id,
                revision: inputs.spec.project_revision,
                catalog: inputs.offer.offered.offer.manifest.catalog.clone(),
                permissions_digest: proposal.choice.permission_digest.clone(),
            },
        },
        FactObservation {
            revision: inputs
                .registry
                .version
                .as_str()
                .parse()
                .map_err(|_| Error::Invalid)?,
            observed_at_ms: inputs.checked_at_ms,
            ceiling: u64::MAX,
            value: inputs.registry_fact.clone(),
        },
        FactObservation {
            revision: inputs
                .principal
                .version
                .as_str()
                .parse()
                .map_err(|_| Error::Invalid)?,
            observed_at_ms: inputs.checked_at_ms,
            ceiling: u64::MAX,
            value: FactValue::Principal {
                principal: inputs.principal.clone(),
                scope: inputs.spec.graph_access_profile.clone(),
            },
        },
        FactObservation {
            revision: inputs.inventory.revision,
            observed_at_ms: inputs.checked_at_ms,
            ceiling: inputs.inventory_expires_at_ms,
            value: FactValue::Inventory {
                node_id: a.lease.identity.node_id,
                boot_id: a.lease.identity.boot_id,
                sequence: inputs.inventory_sequence,
                model: proposal.choice.model.clone(),
                harness: proposal.choice.harness.clone(),
                capabilities: inputs.capabilities.to_vec(),
                state: phase::InventoryState::Installed,
                credential_semantics: phase::GlobalCredentialSemantics::ProvisionableOnly,
                credential_profile: inputs.material.credential_profile.clone(),
                credential_revision: inputs.material.credential_revision,
            },
        },
        FactObservation {
            revision: inputs
                .availability
                .iter()
                .map(|x| x.revision)
                .max()
                .ok_or(Error::Invalid)?,
            observed_at_ms: inputs.checked_at_ms,
            ceiling: inputs.availability_expires_at_ms,
            value: FactValue::Availability {
                provider: Text::new(inputs.material.provider.as_str())
                    .map_err(|_| Error::Invalid)?,
                account_reference: Text::new(inputs.material.account_reference.as_str())
                    .map_err(|_| Error::Invalid)?,
                endpoint_reference: Text::new(inputs.material.endpoint_reference.as_str())
                    .map_err(|_| Error::Invalid)?,
                state: phase::AvailabilityState::Ready,
                provider_observation,
            },
        },
        FactObservation {
            revision: budget_revision,
            observed_at_ms: time,
            ceiling: u64::MAX,
            value: FactValue::Budget {
                lineage_id: inputs.spec.lifecycle.lineage_id,
                // Each attempt has its own window, the execution profile's
                // wall time, and has used none of it when it is granted.
                elapsed_ms: Some(0),
                maximum_lineage_elapsed_ms: inputs
                    .offer
                    .offered
                    .offer
                    .manifest
                    .resources
                    .wall_time_ms,
                remaining_cost_microunits: None,
            },
        },
        FactObservation {
            revision: inputs.spec.task.version,
            observed_at_ms: inputs
                .graph
                .ready_requested_at_ms
                .min(inputs.graph.context_requested_at_ms),
            ceiling: inputs
                .graph
                .ready_expires_at_ms
                .min(inputs.graph.context_expires_at_ms),
            value: FactValue::Context {
                task: inputs.spec.task.clone(),
                context: inputs.spec.task_context.clone(),
                full_context_digest: inputs.full_context_digest.clone(),
                graph_policy: inputs.spec.graph_access_profile.clone(),
            },
        },
        FactObservation {
            revision: inputs.spec.source.repository_binding_generation,
            observed_at_ms: inputs.source_observed_at_ms,
            ceiling: inputs.source_expires_at_ms,
            value: FactValue::Git {
                repository_id: inputs.spec.source.repository_id,
                binding_generation: inputs.spec.source.repository_binding_generation,
                base: inputs.spec.source.resolved_object.clone(),
                policy: inputs.spec.source.policy.clone(),
                retention,
                state: phase::RetentionState::Active,
                manifest: source_receipt("manifest")?,
                bundle: source_receipt("bundle")?,
            },
        },
        FactObservation {
            revision: inputs.material.credential_revision,
            observed_at_ms: inputs.file_started_at_ms,
            ceiling: inputs
                .file_started_at_ms
                .checked_add(10_000)
                .ok_or(Error::Invalid)?,
            value: FactValue::PrivateMaterialBacking {
                lease_id: inputs.material.lease_id,
                backing_digest: inputs.file_digest.clone(),
                installation_receipt_digest: digest(&inputs.request.original.materials[0])?,
            },
        },
    ])
}

fn seal_fact(
    proposal: &Proposal,
    a: &AcceptedReceipt,
    request: &PrepareRequest,
    time: u64,
    confirmation_id: Uuid,
    observation: FactObservation,
) -> Result<OwnerFact, Error> {
    let FactObservation {
        revision,
        observed_at_ms,
        ceiling,
        value,
    } = observation;
    let kind = value.kind();
    let authority = proposal
        .owners
        .iter()
        .find(|x| x.kind == kind)
        .ok_or(Error::Invalid)?
        .authority
        .clone();
    let body = FactBody {
        authority,
        binding: a.lease.binding.clone(),
        choice_digest: proposal.choice.digest().map_err(|_| Error::Invalid)?,
        revision,
        observed_at_ms,
        expires_at_ms: observed_at_ms
            .checked_add(proposal.policy.maximum_observation_ms)
            .ok_or(Error::Invalid)?
            .min(ceiling),
        value,
    };
    let fact = OwnerFact {
        record: ArtifactRef {
            id: Text::new(format!("aq.phase-fact/{confirmation_id}/{kind:?}"))
                .map_err(|_| Error::Invalid)?,
            version: Text::new(revision.to_string()).map_err(|_| Error::Invalid)?,
            digest: OwnerFact::record_digest(&body).map_err(|_| Error::Invalid)?,
        },
        body,
    };
    fact.validate_for(proposal, a, request, time)
        .map_err(|_| Error::Invalid)?;
    Ok(fact)
}

#[cfg(test)]
mod tests {
    include!("profile_tests.rs");
}
