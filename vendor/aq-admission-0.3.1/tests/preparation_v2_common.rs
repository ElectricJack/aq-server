#![allow(dead_code)]
use aq_admission::preparation_v2::Proposal;
use aq_admission::{
    control::{DeliveryStamp, Versions},
    execution_spec::ProfiledExecutionSpec,
    preparation_v2::*,
    startup::NativeOffer,
    transport::{Accept, ArtifactReceipt, DeliveryClock, Prepare},
    *,
};
use serde_json::{Value, json};
use uuid::Uuid;
pub fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
pub fn reference(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
pub fn canonical(v: &impl serde::Serialize) -> Vec<u8> {
    let mut v = serde_json::to_value(v).unwrap();
    v.sort_all_objects();
    serde_json::to_vec(&v).unwrap()
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
    let control: Value =
        serde_json::from_str(include_str!("../artifacts/control.fixture.json")).unwrap();
    let spec: Value =
        serde_json::from_str(include_str!("../artifacts/execution-spec.fixture.json")).unwrap();
    let spec: ProfiledExecutionSpec = serde_json::from_value(spec["spec"].clone()).unwrap();
    let mut original: NativeOffer =
        serde_json::from_value(control["native_offer"].clone()).unwrap();
    original.offered.offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    original.offered.offer.manifest.resources.wall_time_ms = 60000;
    original.offered.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&original.offered.offer.manifest).unwrap();
    for d in &mut original.offered.artifacts {
        d.retain_until_ms = 100000;
    }
    let mut d = original.offered.artifacts[0].clone();
    d.artifact = spec.artifact().unwrap();
    d.kind = ArtifactKind::Composition;
    d.bytes = spec.canonical_bytes().unwrap().len() as u64;
    original.offered.artifacts.push(d);
    let mut d = original
        .offered
        .artifacts
        .iter()
        .find(|d| d.artifact == spec.task_context)
        .unwrap()
        .clone();
    d.artifact = spec.task_observation.clone();
    d.bytes = 1;
    original.offered.artifacts.push(d);
    let model = original.offered.offer.manifest.model.clone();
    let harness = original.offered.offer.manifest.harness.clone();
    let credential = original.offered.offer.manifest.credentials[0]
        .credential_profile
        .clone();
    let choice = ChoicePins {
        selection_digest: spec.selection_digest.clone(),
        registry: reference("registry"),
        principal: reference("principal"),
        permission_digest: Digest::of(&canonical(&original.offered.offer.manifest.permissions)),
        credential_profile: credential,
        credential_revision: 1,
        account_reference: Text::new("account").unwrap(),
        endpoint_reference: Text::new("endpoint").unwrap(),
        model,
        harness,
        maximum_lineage_elapsed_ms: 60000,
    };
    let negotiation = Negotiation {
        contract: contract_artifact().unwrap(),
        features: required_features(),
    };
    let owners = FactKind::all()
        .iter()
        .map(|k| OwnerAuthority {
            kind: *k,
            authority: reference(&format!("synthetic-owner/{k:?}")),
        })
        .collect();
    let proposal = Proposal {
        schema_version: 2,
        negotiation: negotiation.clone(),
        policy: PhasePolicy {
            schema_version: 2,
            maximum_preparation_ms: 30000,
            maximum_execution_lease_ms: 30000,
            maximum_observation_ms: 2000,
        },
        original,
        spec,
        choice,
        full_context_digest: Digest::of(b"synthetic exact full scoped task context"),
        owners,
    };
    proposal.validate().unwrap();
    let mut original: Accept =
        serde_json::from_value(control["requests"]["accept"].clone()).unwrap();
    original.acceptance.binding = proposal.original.offered.offer.binding.clone();
    let hold = ResourceHold {
        maximum_preparation_ms: 30000,
        reservation: original.reservation.clone(),
        resources: Resources {
            slots: 1,
            memory_bytes: 1 << 30,
            cpu_millis: 100000,
            disk_bytes: 32 << 30,
        },
        source_profile: reference("source-profile"),
        source_disk_bytes: (21 << 30) + (1 << 20),
    };
    let accept = AcceptRequest {
        schema_version: 2,
        negotiation: negotiation.clone(),
        proposal_digest: proposal.digest().unwrap(),
        original,
        hold,
    };
    accept.validate_for(&proposal).unwrap();
    let plan: transport::PreparationPlan =
        serde_json::from_value(control["replies"][1]["body"]["original"].clone()).unwrap();
    let mut material = plan.materials[0].clone();
    material.binding = accept.original.acceptance.binding.clone();
    material.provider =
        serde_json::from_value(json!(proposal.choice.model.provider.as_str())).unwrap();
    material.account_reference =
        serde_json::from_value(json!(proposal.choice.account_reference.as_str())).unwrap();
    material.endpoint_reference =
        serde_json::from_value(json!(proposal.choice.endpoint_reference.as_str())).unwrap();
    material.expires_at_ms = 60000;
    let accepted = AcceptedReceipt {
        schema_version: 2,
        negotiation: negotiation.clone(),
        operation_id: accept.original.operation_id,
        request_digest: accept.digest().unwrap(),
        lease: AcceptedPreparationLease {
            lease_id: id(400),
            revision: 1,
            identity: accept.original.identity.clone(),
            binding: accept.original.acceptance.binding.clone(),
            guardian_id: accept.original.guardian_id,
            launch_id: accept.original.launch_id,
            hold: accept.hold.clone(),
            proposal_digest: accept.proposal_digest.clone(),
            policy_digest: proposal.policy.digest().unwrap(),
            choice_digest: proposal.choice.digest().unwrap(),
            issued_at_ms: 1500,
            expires_at_ms: 31500,
            materials: vec![material.clone()],
        },
        versions: Versions {
            record_revision: 3,
            authority_revision: 2,
            grant_revision: 0,
        },
    };
    accepted.validate_for(&accept, &proposal).unwrap();
    let mut artifacts = proposal
        .original
        .offered
        .artifacts
        .iter()
        .map(|d| ArtifactReceipt {
            artifact: d.artifact.clone(),
            bytes: d.bytes,
            digest: d.artifact.digest.clone(),
        })
        .collect::<Vec<_>>();
    for member in ["manifest", "bundle"] {
        let a = reference(&format!("aq.source-{member}/{}", id(500)));
        artifacts.push(ArtifactReceipt {
            digest: a.digest.clone(),
            artifact: a,
            bytes: 10,
        });
    }
    let installed = MaterialReceipt {
        schema_version: 1,
        operation_id: id(600),
        lease_id: material.lease_id,
        binding: material.binding.clone(),
        material_digest: material.material_digest.clone(),
        private_binding: reference(&format!("aq.private-material/{}", material.lease_id)),
        received_at_ms: 1600,
    };
    let prepare = PrepareRequest {
        schema_version: 2,
        negotiation: negotiation.clone(),
        accepted_receipt_digest: accepted.digest().unwrap(),
        expected_preparation_revision: 1,
        original: Prepare {
            schema_version: 1,
            operation_id: id(700),
            identity: accepted.lease.identity.clone(),
            binding: accepted.lease.binding.clone(),
            reservation: accepted.lease.hold.reservation.clone(),
            guardian_id: accepted.lease.guardian_id,
            launch_id: accepted.lease.launch_id,
            journal_revision: accept.original.journal_revision + 2,
            journal_digest: Digest::of(b"synthetic Prepared journal"),
            artifacts,
            materials: vec![installed.clone()],
        },
    };
    prepare.validate_for(&accepted, &accept, &proposal).unwrap();
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
            registry: c.registry.clone(),
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
            let body = FactBody {
                authority: p
                    .owners
                    .iter()
                    .find(|x| x.kind == kind)
                    .unwrap()
                    .authority
                    .clone(),
                binding: accepted.lease.binding.clone(),
                choice_digest: accepted.lease.choice_digest.clone(),
                revision: 2,
                observed_at_ms: 29900,
                expires_at_ms: 31900,
                value,
            };
            let record = ArtifactRef {
                id: Text::new(format!("synthetic-fact/{kind:?}")).unwrap(),
                version: Text::new("2").unwrap(),
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
        grant: aq_session_protocol::ProfiledGrant {
            binding: accepted.lease.binding.clone(),
            grant_id: id(900),
            expires_at_ms: 50000,
        },
        execution_deadline_ms: 90000,
        confirmation,
        versions: Versions {
            record_revision: 4,
            authority_revision: 3,
            grant_revision: 1,
        },
    };
    execution
        .validate_for(&prepare, &accepted, &accept, &proposal)
        .unwrap();
    let delivery = ExecutionDelivery {
        schema_version: 2,
        receipt: execution.clone(),
        delivery: DeliveryStamp {
            nonce: id(1000),
            clock: DeliveryClock::new(40000, 50000).unwrap(),
        },
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
        Document::Prepare(Box::new(c.prepare.clone())),
        Document::Confirmation(Box::new(c.execution.confirmation.clone())),
        Document::Execution(Box::new(c.execution.clone())),
        Document::ExecutionDelivery(Box::new(c.delivery.clone())),
    ]
}
pub fn fixtures() -> Value {
    let c = case();
    json!({"authority_admission":false,"case":"sanitized synthetic correlations; no authenticated owners or files","proposal":c.proposal,"accept":c.accept,"accepted":c.accepted,"prepare":c.prepare,"execution":c.execution,"delivery":c.delivery,"documents":documents(&c)})
}
