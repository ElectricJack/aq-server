#![allow(dead_code)]
#[path = "common.rs"]
pub mod common;
use aq_admission::{
    scoped_presence::*,
    transport::{DeliveryClock, MaterialDelivery, WorkerIdentity},
    *,
};
use common::*;
pub fn negotiation() -> Negotiation {
    let mut reference = artifact("aq.scoped-material-presence-qualification/fixture");
    reference.digest = qualification().digest().unwrap();
    Negotiation {
        protocol: Protocol::ScopedMaterialPresenceV1,
        qualification: reference,
    }
}

pub fn qualification() -> Qualification {
    Qualification {
        schema_version: 1,
        protocol: Protocol::ScopedMaterialPresenceV1,
        runtime: artifact("fixture-runtime"),
        runtime_qualification: artifact("fixture-runtime-qualification"),
        adapter: artifact("fixture-presence-adapter"),
        adapter_qualification: artifact("fixture-presence-adapter-evidence"),
        inventory_semantics: InventorySemantics::ProvisionableOnlyAndScopedPresenceV1,
        observation_protocol: ObservationProtocol::GrantIssuanceBoundedBoottimeV1,
        maximum_age_ms: 30_000,
        maximum_ttl_ms: 30_000,
    }
}
pub fn inventory() -> InventoryV2 {
    InventoryV2 {
        schema_version: 1,
        operation_id: id(90),
        identity: WorkerIdentity {
            node_id: id(11),
            boot_id: id(12),
        },
        runtime: artifact("fixture-runtime"),
        runtime_qualification: artifact("fixture-runtime-qualification"),
        negotiation: negotiation(),
        sequence: 2,
        ttl_ms: 30_000,
        qualifications: vec![artifact("fixture-harness")],
        provisionable_only: vec![CredentialReference {
            profile: lease().credential_profile,
            revision: 1,
        }],
    }
}
pub fn record() -> PrivateInstallationRecord {
    let delivery = MaterialDelivery {
        lease: lease(),
        clock: DeliveryClock::new(1100, 2000).unwrap(),
    };
    let deadline = delivery
        .clock
        .client_deadline_ns(10_000_000, 20_000_000)
        .unwrap();
    PrivateInstallationRecord {
        schema_version: 1,
        operation_id: id(91),
        preparation_digest: Digest::of(b"synthetic preparation"),
        worker: inventory().identity,
        kernel_boot_id: id(92),
        clock_source: ClockSource::Boottime,
        delivery,
        sent_ns: 10_000_000,
        received_ns: 20_000_000,
        material_synced_ns: 30_000_000,
        retrieval_deadline_ns: deadline,
    }
}
pub fn report() -> PresenceReport {
    let installation = record();
    let receipt = installation.receipt().unwrap();
    PresenceReport {
        schema_version: 1,
        operation_id: id(93),
        identity: inventory().identity,
        runtime: inventory().runtime,
        runtime_qualification: inventory().runtime_qualification,
        negotiation: negotiation(),
        binding: binding(),
        grant_id: id(94),
        guardian_id: id(95),
        launch_id: id(96),
        reservation_id: id(97),
        lease_id: lease().lease_id,
        installation_receipt_digest: {
            let mut value = serde_json::to_value(&receipt).unwrap();
            value.sort_all_objects();
            Digest::of(&serde_json::to_vec(&value).unwrap())
        },
        private_binding: receipt.private_binding,
        inventory: InventoryIdentity {
            operation_id: inventory().operation_id,
            sequence: 2,
            revision: 2,
            request_digest: inventory().digest().unwrap(),
            snapshot_digest: Digest::of(b"synthetic normalized node inventory"),
        },
        authority: AuthorityAnchor {
            kind: AuthorityKind::InitialGrant,
            operation_id: id(98),
            grant_revision: 1,
            expires_at_ms: 5100,
        },
        sequence: 1,
        ttl_ms: 30_000,
        observation: Observation {
            kernel_boot_id: id(92),
            clock_source: ClockSource::Boottime,
            permit_acquired_ns: 2_000_000_000,
            permit_deadline_ns: 12_000_000_000,
            read_started_ns: 2_010_000_000,
            read_completed_ns: 2_020_000_000,
            sent_ns: 2_030_000_000,
        },
        presence: Presence::Present {
            observed_material_digest: installation.delivery.lease.material_digest.clone(),
            observed_material_bytes: installation.delivery.lease.material_bytes,
            installation: Box::new(installation),
        },
    }
}
