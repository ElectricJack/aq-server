mod preparation_v2_common;
use aq_admission::preparation_v2::decode;
use aq_admission::{control::DeliveryStamp, preparation_v2::*, transport::DeliveryClock, *};
use preparation_v2_common::*;
use serde_json::json;
#[test]
fn cold_preparation_and_execution_are_distinct_immutable_phases() {
    let c = case();
    assert_eq!(c.proposal.original.offered.clock.expires_at_ms, 2000);
    assert_eq!(c.proposal.spec.source.expires_at_ms, 2000);
    assert_eq!(c.accepted.lease.expires_at_ms, 31500);
    assert_eq!(c.execution.issued_at_ms, 30000);
    c.execution
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    let sample = c
        .delivery
        .sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            10_000_000_000,
            10_100_000_000,
            None,
        )
        .unwrap();
    assert_eq!(sample.deadline_ns(), 20_000_000_000);
    assert_eq!(sample.hard_deadline_ns(), 60_000_000_000);
    let delivery = AcceptedDelivery {
        schema_version: 2,
        receipt: c.accepted.clone(),
        delivery: DeliveryStamp {
            nonce: id(1001),
            clock: DeliveryClock::new(3000, 31500).unwrap(),
        },
    };
    let a = delivery
        .sample(
            &c.accept,
            &c.proposal,
            id(1001),
            1_000_000_000,
            1_100_000_000,
            None,
        )
        .unwrap();
    assert_eq!(a.deadline_ns(), 29_500_000_000);
}
#[test]
fn exact_contract_and_complete_unique_features_are_mandatory() {
    let c = case();
    for mode in 0..4 {
        let mut p = c.proposal.clone();
        match mode {
            0 => p.negotiation.contract.digest = Digest::of(b"different schema"),
            1 => {
                p.negotiation.features.pop();
            }
            2 => p.negotiation.features.push(p.negotiation.features[0]),
            _ => p.negotiation.contract.version = Text::new("legacy").unwrap(),
        };
        assert!(p.validate().is_err());
    }
}
#[test]
fn acceptance_cannot_change_hold_request_or_retime_expired_proposal() {
    let c = case();
    for mode in 0..5 {
        let mut a = c.accepted.clone();
        match mode {
            0 => a.lease.issued_at_ms = 2000,
            1 => a.request_digest = Digest::of(b"changed request"),
            2 => a.lease.hold.resources.disk_bytes += 1,
            3 => a.lease.identity.boot_id = id(9999),
            _ => a.lease.expires_at_ms = 100001,
        };
        assert!(a.validate_for(&c.accept, &c.proposal).is_err());
    }
}
#[test]
fn original_artifact_retention_caps_accepted_preparation() {
    let mut c = case();
    c.proposal.original.offered.artifacts[0].retain_until_ms = 30000;
    c.accept.proposal_digest = c.proposal.digest().unwrap();
    c.accepted.request_digest = c.accept.digest().unwrap();
    c.accepted.lease.proposal_digest = c.accept.proposal_digest.clone();
    assert!(c.accepted.validate_for(&c.accept, &c.proposal).is_err());
}
#[test]
fn exact_unique_artifacts_and_linked_transfer_roles_refuse_substitution() {
    let c = case();
    for mode in 0..4 {
        let mut q = c.prepare.clone();
        match mode {
            0 => {
                q.original.artifacts.pop();
            }
            1 => q.original.artifacts[0].bytes += 1,
            2 => {
                let n = q.original.artifacts.len();
                q.original.artifacts[n - 1].artifact.id =
                    Text::new(format!("aq.source-bundle/{}", id(999))).unwrap();
            }
            _ => {
                let n = q.original.artifacts.len();
                q.original.artifacts[n - 1] = q.original.artifacts[0].clone();
            }
        }
        assert!(q.validate_for(&c.accepted, &c.accept, &c.proposal).is_err());
    }
}
#[path = "preparation_v2/confirmation.rs"]
mod confirmation;
#[path = "preparation_v2/publication.rs"]
mod publication;
#[test]
fn accepted_phase_requires_explicit_new_worker_duration_bound() {
    let mut c = case();
    assert_eq!(c.accept.original.reservation_remaining_ms, 500);
    c.accepted.validate_for(&c.accept, &c.proposal).unwrap();
    c.accept.hold.maximum_preparation_ms = 1000;
    c.accepted.lease.hold = c.accept.hold.clone();
    c.accepted.request_digest = c.accept.digest().unwrap();
    assert!(c.accepted.validate_for(&c.accept, &c.proposal).is_err());
    for n in [0, 30001, u64::MAX] {
        c.accept.hold.maximum_preparation_ms = n;
        assert!(c.accept.validate_for(&c.proposal).is_err());
    }
}

#[path = "preparation_v2/long_phase_manifest.rs"]
mod long_phase_manifest;
