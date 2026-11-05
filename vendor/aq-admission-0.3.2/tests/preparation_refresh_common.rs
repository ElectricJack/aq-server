#![allow(dead_code)]
//! Synthetic data vectors only; they cannot authenticate facts or permit launch.
#[path = "preparation_v2_common.rs"]
mod legacy_data;
use aq_admission::{preparation_refresh_v1::*, *};
pub use legacy_data::{canonical, id, reference};
use serde_json::{Value, json};
pub fn registry(revision: u64) -> ArtifactRef {
    ArtifactRef {
        id: Text::new("aq.intelligence-registry").unwrap(),
        version: Text::new(revision.to_string()).unwrap(),
        digest: Digest::of(b"synthetic-unchanged-registry-configuration"),
    }
}
#[derive(Clone)]
pub struct Case {
    pub proposal: Proposal,
    pub accept: AcceptRequest,
    pub accepted: AcceptedReceipt,
    pub prepare: PrepareRequest,
    pub execution: ExecutionReceipt,
    pub delivery: ExecutionDelivery,
}
pub fn case() -> Case {
    // Reuse original transport/spec/material input data, never convert an old
    // profile receipt/fact or change its tag to stand in for the new profile.
    let base = legacy_data::case();
    let b = &base.proposal.choice;
    let negotiation = Negotiation {
        contract: contract_artifact().unwrap(),
        features: required_features(),
    };
    let proposal = Proposal {
        schema_version: 2,
        negotiation: negotiation.clone(),
        policy: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 30000,
            maximum_execution_lease_ms: 30000,
            maximum_observation_ms: 2000,
        },
        original: base.proposal.original.clone(),
        spec: base.proposal.spec.clone(),
        choice: ChoicePins {
            selection_digest: b.selection_digest.clone(),
            registry: registry(1),
            principal: b.principal.clone(),
            permission_digest: b.permission_digest.clone(),
            credential_profile: b.credential_profile.clone(),
            credential_revision: b.credential_revision,
            account_reference: b.account_reference.clone(),
            endpoint_reference: b.endpoint_reference.clone(),
            model: b.model.clone(),
            harness: b.harness.clone(),
            maximum_lineage_elapsed_ms: b.maximum_lineage_elapsed_ms,
        },
        full_context_digest: base.proposal.full_context_digest.clone(),
        owners: FactKind::all()
            .into_iter()
            .map(|kind| OwnerAuthority {
                kind,
                authority: reference(&format!("synthetic-refresh-owner/{kind:?}")),
            })
            .collect(),
    };
    proposal.validate().unwrap();
    let h = &base.accept.hold;
    let accept = AcceptRequest {
        schema_version: 2,
        negotiation: negotiation.clone(),
        proposal_digest: proposal.digest().unwrap(),
        original: base.accept.original.clone(),
        hold: ResourceHold {
            maximum_preparation_ms: h.maximum_preparation_ms,
            reservation: h.reservation.clone(),
            resources: h.resources,
            source_profile: h.source_profile.clone(),
            source_disk_bytes: h.source_disk_bytes,
        },
    };
    accept.validate_for(&proposal).unwrap();
    let l = &base.accepted.lease;
    let material = l.materials[0].clone();
    let accepted = AcceptedReceipt {
        schema_version: 2,
        negotiation: negotiation.clone(),
        operation_id: accept.original.operation_id,
        request_digest: accept.digest().unwrap(),
        lease: AcceptedPreparationLease {
            lease_id: l.lease_id,
            revision: 1,
            identity: l.identity.clone(),
            binding: l.binding.clone(),
            guardian_id: l.guardian_id,
            launch_id: l.launch_id,
            hold: accept.hold.clone(),
            proposal_digest: proposal.digest().unwrap(),
            policy_digest: proposal.policy.digest().unwrap(),
            choice_digest: proposal.choice.digest().unwrap(),
            issued_at_ms: l.issued_at_ms,
            expires_at_ms: l.expires_at_ms,
            materials: vec![material.clone()],
            registry_at_accept: registry(2),
        },
        versions: base.accepted.versions.clone(),
    };
    accepted.validate_for(&accept, &proposal).unwrap();
    let prepare = PrepareRequest {
        schema_version: 2,
        negotiation: negotiation.clone(),
        accepted_receipt_digest: accepted.digest().unwrap(),
        expected_preparation_revision: 1,
        original: base.prepare.original.clone(),
    };
    prepare.validate_for(&accepted, &accept, &proposal).unwrap();
    let installed = prepare.original.materials[0].clone();
    let p = &proposal;
    let c = &p.choice;
    let values = vec![
        FactValue::Project {
            project_id: p.spec.lifecycle.project_id,
            revision: p.spec.project_revision,
            catalog: p.original.offered.offer.manifest.catalog.clone(),
            permissions_digest: c.permission_digest.clone(),
        },
        FactValue::Registry {
            selected_registry: c.registry.clone(),
            accepted_registry: accepted.lease.registry_at_accept.clone(),
            preflight_registry: registry(3),
            current_registry: registry(4),
            credential_profile: c.credential_profile.clone(),
            credential_revision: c.credential_revision,
        },
        FactValue::Principal {
            principal: c.principal.clone(),
            scope: p.spec.graph_access_profile.clone(),
        },
        FactValue::Inventory {
            node_id: accepted.lease.identity.node_id,
            boot_id: accepted.lease.identity.boot_id,
            sequence: 2,
            model: c.model.clone(),
            harness: c.harness.clone(),
            capabilities: accept.original.acceptance.advertised_capabilities.clone(),
            state: InventoryState::Installed,
            credential_semantics: GlobalCredentialSemantics::ProvisionableOnly,
            credential_profile: c.credential_profile.clone(),
            credential_revision: c.credential_revision,
        },
        FactValue::Availability {
            provider: c.model.provider.clone(),
            account_reference: c.account_reference.clone(),
            endpoint_reference: c.endpoint_reference.clone(),
            state: AvailabilityState::Ready,
            provider_observation: reference("actual-provider-observation-claim"),
        },
        FactValue::Budget {
            lineage_id: p.spec.lifecycle.lineage_id,
            elapsed_ms: Some(0),
            maximum_lineage_elapsed_ms: c.maximum_lineage_elapsed_ms,
            remaining_cost_microunits: p
                .original
                .offered
                .offer
                .manifest
                .model
                .spending_budget
                .as_ref()
                .map(|b| b.limit_microunits),
        },
        FactValue::Context {
            task: p.spec.task.clone(),
            context: p.spec.task_context.clone(),
            full_context_digest: p.full_context_digest.clone(),
            graph_policy: p.spec.graph_access_profile.clone(),
        },
        FactValue::Git {
            repository_id: p.spec.source.repository_id,
            binding_generation: p.spec.source.repository_binding_generation,
            base: p.spec.source.resolved_object.clone(),
            policy: p.spec.source.policy.clone(),
            retention: reference("active-retention-claim"),
            state: RetentionState::Active,
            manifest: prepare.original.artifacts[prepare.original.artifacts.len() - 2].clone(),
            bundle: prepare.original.artifacts.last().unwrap().clone(),
        },
        FactValue::PrivateMaterialBacking {
            lease_id: material.lease_id,
            backing_digest: material.material_digest.clone(),
            installation_receipt_digest: Digest::of(&canonical(&installed)),
        },
    ];
    let facts = values
        .into_iter()
        .map(|value| {
            let kind = value.kind();
            let revision = if kind == FactKind::Registry { 4 } else { 2 };
            let body = FactBody {
                authority: p
                    .owners
                    .iter()
                    .find(|o| o.kind == kind)
                    .unwrap()
                    .authority
                    .clone(),
                binding: accepted.lease.binding.clone(),
                choice_digest: accepted.lease.choice_digest.clone(),
                revision,
                observed_at_ms: 29900,
                expires_at_ms: 31900,
                value,
            };
            let record = ArtifactRef {
                id: Text::new(format!("synthetic-refresh-fact/{kind:?}")).unwrap(),
                version: Text::new(revision.to_string()).unwrap(),
                digest: OwnerFact::record_digest(&body).unwrap(),
            };
            OwnerFact { record, body }
        })
        .collect();
    let confirmation = FreshConfirmation {
        schema_version: 2,
        confirmation_id: id(800),
        prepare_digest: prepare.digest().unwrap(),
        accepted_receipt_digest: accepted.digest().unwrap(),
        checked_at_ms: 30000,
        facts,
    };
    confirmation
        .validate_for(&prepare, &accepted, &accept, &proposal)
        .unwrap();
    let execution = ExecutionReceipt {
        schema_version: 2,
        negotiation,
        operation_id: prepare.original.operation_id,
        request_digest: prepare.digest().unwrap(),
        accepted_receipt_digest: accepted.digest().unwrap(),
        issued_at_ms: 30000,
        grant: base.execution.grant.clone(),
        execution_deadline_ms: 90000,
        confirmation,
        versions: base.execution.versions.clone(),
    };
    execution
        .validate_for(&prepare, &accepted, &accept, &proposal)
        .unwrap();
    let delivery = ExecutionDelivery {
        schema_version: 2,
        receipt: execution.clone(),
        delivery: base.delivery.delivery.clone(),
    };
    Case {
        proposal,
        accept,
        accepted,
        prepare,
        execution,
        delivery,
    }
}
pub fn resign(f: &mut OwnerFact) {
    f.record.digest = OwnerFact::record_digest(&f.body).unwrap();
}
pub fn documents(c: &Case) -> Vec<Document> {
    vec![
        Document::Proposal(Box::new(c.proposal.clone())),
        Document::Accept(Box::new(c.accept.clone())),
        Document::Accepted(Box::new(c.accepted.clone())),
        Document::AcceptedDelivery(Box::new(AcceptedDelivery {
            schema_version: 2,
            receipt: c.accepted.clone(),
            delivery: aq_admission::control::DeliveryStamp {
                nonce: id(1001),
                clock: aq_admission::transport::DeliveryClock::new(3000, 31500).unwrap(),
            },
        })),
        Document::Prepare(Box::new(c.prepare.clone())),
        Document::Confirmation(Box::new(c.execution.confirmation.clone())),
        Document::Execution(Box::new(c.execution.clone())),
        Document::ExecutionDelivery(Box::new(c.delivery.clone())),
    ]
}
pub fn fixtures() -> Value {
    let c = case();
    json!({"authority_admission":false,"case":"synthetic structural registry-continuity data, not authenticated owner/runtime evidence",
    "documents":documents(&c),"proposal":c.proposal,"accept":c.accept,"accepted":c.accepted,"prepare":c.prepare,"execution":c.execution,"delivery":c.delivery})
}

pub fn legacy_documents_for_refusal() -> Vec<Vec<u8>> {
    legacy_data::documents(&legacy_data::case())
        .iter()
        .map(canonical)
        .collect()
}

/// Shared frozen source case for tests that exercise both phase profiles.
pub fn frozen_case() -> legacy_data::Case {
    legacy_data::case()
}
