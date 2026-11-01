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
#[test]
fn mandatory_owner_fact_set_hash_and_lifetime_are_bounded() {
    let c = case();
    for mode in 0..5 {
        let mut f = c.execution.confirmation.clone();
        match mode {
            0 => {
                f.facts.pop();
            }
            1 => f.facts[1] = f.facts[0].clone(),
            2 => f.facts[0].record.digest = Digest::of(b"changed fact"),
            3 => {
                f.facts[0].body.expires_at_ms += 1;
                resign(&mut f.facts[0]);
            }
            _ => {
                f.facts[0].body.authority = reference("foreign-owner");
                resign(&mut f.facts[0]);
            }
        }
        assert!(
            f.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err()
        );
    }
}
#[test]
fn changed_current_choice_context_source_and_unknown_facts_deny_initial_confirmation() {
    let c = case();
    for mode in 0..11 {
        let mut f = c.execution.confirmation.clone();
        let kind = match mode {
            0..=2 => FactKind::Inventory,
            3 => FactKind::Availability,
            4 | 5 => FactKind::Budget,
            6 => FactKind::Context,
            7 | 8 => FactKind::Git,
            9 => FactKind::PrivateMaterialBacking,
            _ => FactKind::Registry,
        };
        let item = f
            .facts
            .iter_mut()
            .find(|x| x.body.value.kind() == kind)
            .unwrap();
        match &mut item.body.value {
            FactValue::Inventory { harness, model, .. } => match mode {
                0 => harness.cli_version = Text::new("different").unwrap(),
                1 => harness.environment = reference("different-env"),
                _ => model.reasoning = Text::new("different").unwrap(),
            },
            FactValue::Availability { state, .. } => *state = AvailabilityState::Unknown,
            FactValue::Budget {
                elapsed_ms,
                maximum_lineage_elapsed_ms,
                ..
            } => {
                *elapsed_ms = if mode == 4 {
                    None
                } else {
                    Some(*maximum_lineage_elapsed_ms)
                }
            }
            FactValue::Context { task, .. } => task.version += 1,
            FactValue::Git { base, policy, .. } => {
                if mode == 7 {
                    base.hex = "b".repeat(40)
                } else {
                    *policy = reference("different-policy")
                }
            }
            FactValue::PrivateMaterialBacking {
                installation_receipt_digest,
                ..
            } => *installation_receipt_digest = Digest::of(b"different installation"),
            FactValue::Registry {
                credential_revision,
                ..
            } => *credential_revision += 1,
            _ => unreachable!(),
        };
        resign(item);
        assert!(
            f.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err(),
            "mode {mode}"
        );
    }
}
#[test]
fn grant_issuance_must_be_inside_phase_and_current_budget() {
    let c = case();
    for mode in 0..3 {
        let mut g = c.execution.clone();
        match mode {
            0 => g.issued_at_ms = c.accepted.lease.expires_at_ms,
            1 => g.execution_deadline_ms += 1,
            _ => g.request_digest = Digest::of(b"other prepared request"),
        };
        assert!(
            g.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err()
        );
    }
}
#[test]
fn independent_execution_replay_never_extends_or_revives_old_sample() {
    let c = case();
    let old = c
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
    let mut next = c.delivery.clone();
    next.delivery.clock = DeliveryClock::new(45000, 50000).unwrap();
    let observed = next
        .sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            19_000_000_000,
            19_100_000_000,
            Some(&old),
        )
        .unwrap();
    assert_eq!(observed.deadline_ns(), old.deadline_ns());
    assert!(
        next.sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            21_000_000_000,
            21_100_000_000,
            Some(&old)
        )
        .is_err()
    );
    next.receipt.grant.grant_id = id(9999);
    assert!(
        next.sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            19_000_000_000,
            19_100_000_000,
            Some(&old)
        )
        .is_err()
    );
}
#[test]
fn historical_receipt_does_not_become_live_from_expired_delivery() {
    let c = case();
    c.execution
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    let mut bad = c.delivery.clone();
    bad.delivery.clock.server_time_ms = 50000;
    bad.delivery.clock.valid_for_ms = 0;
    assert!(
        bad.sample(
            &c.prepare,
            &c.accepted,
            &c.accept,
            &c.proposal,
            id(1000),
            1,
            2,
            None
        )
        .is_err()
    );
}
#[test]
fn serialization_and_decode_are_bounded_and_unknown_fields_refuse() {
    let c = case();
    for d in documents(&c) {
        let bytes = serde_json::to_vec(&d).unwrap();
        decode(&bytes).unwrap();
    }
    assert!(decode(&vec![b'x'; MAX_BYTES + 1]).is_err());
    let mut v = serde_json::to_value(Document::Prepare(Box::new(c.prepare))).unwrap();
    v["body"]["eligible"] = json!(true);
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
}
#[test]
fn structural_claims_are_not_authenticated_owner_truth() {
    let c = case();
    let _: () = c
        .execution
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    assert!(
        c.proposal
            .owners
            .iter()
            .all(|o| o.authority.id.as_str().starts_with("synthetic-owner/"))
    );
    assert!(
        schema()["$comment"]
            .as_str()
            .unwrap()
            .contains("no authentication")
    );
}
#[test]
fn generated_artifacts_and_independent_canonical_vectors_match() {
    let c = case();
    let load = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
    assert_eq!(
        load(include_str!("../artifacts/preparation-v2.fixture.json")),
        fixtures()
    );
    assert_eq!(
        load(include_str!("../artifacts/preparation-v2.schema.json")),
        schema()
    );
    assert_eq!(
        load(include_str!("../artifacts/preparation-v2.contract.json")),
        json!({"artifact":contract_artifact().unwrap(),"manifest":contract_manifest().unwrap()})
    );
    let v = load(include_str!(
        "../artifacts/preparation-v2.python-vectors.json"
    ));
    for (key, d) in [
        ("proposal", c.proposal.digest().unwrap()),
        ("accept", c.accept.digest().unwrap()),
        ("accepted", c.accepted.digest().unwrap()),
        ("prepare", c.prepare.digest().unwrap()),
        ("execution", c.execution.digest().unwrap()),
        ("confirmation", c.execution.confirmation.digest().unwrap()),
    ] {
        assert_eq!(d.as_str(), v[key], "{key}");
    }
    for (i, f) in c.execution.confirmation.facts.iter().enumerate() {
        assert_eq!(
            OwnerFact::record_digest(&f.body).unwrap().as_str(),
            v["facts"][i]
        );
    }
    let mut f = c.execution.confirmation.facts[0].body.clone();
    f.authority.id = Text::new("synthetic-owner/雪").unwrap();
    assert_eq!(
        OwnerFact::record_digest(&f).unwrap().as_str(),
        v["unicode_fact"]
    );
    assert_eq!(contract_artifact().unwrap().digest.as_str(), v["contract"]);
    assert_eq!(contract_manifest().unwrap()["schema_digest"], v["schema"]);
}
#[test]
fn exact_qualified_018_source_and_artifacts_are_preserved() {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../artifacts/preparation-v2.compatibility-sha256.json"
    ))
    .unwrap();
    for f in v["files"].as_array().unwrap() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f["path"].as_str().unwrap());
        let b = std::fs::read(path).unwrap();
        let check = if f["prefix"] == true {
            &b[..f["bytes"].as_u64().unwrap() as usize]
        } else {
            &b
        };
        assert_eq!(Digest::of(check).as_str(), f["sha256"], "{}", f["path"]);
    }
}
#[test]
fn v2_decoder_never_falls_back_to_legacy_or_different_schema() {
    let c = case();
    assert!(
        decode(
            &serde_json::to_vec(&aq_admission::transport::TransportDocument::Prepare(
                c.prepare.original.clone()
            ))
            .unwrap()
        )
        .is_err()
    );
    for d in documents(&c) {
        let mut v = serde_json::to_value(d).unwrap();
        v["body"]["schema_version"] = json!(1);
        assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }
}

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
