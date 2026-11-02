mod preparation_refresh_common;
use aq_admission::{Digest, Text, preparation_refresh_v1::*};
use preparation_refresh_common::*;
use serde_json::{Value, json};
fn registry_fact(value: &mut FreshConfirmation) -> &mut OwnerFact {
    value
        .facts
        .iter_mut()
        .find(|f| f.body.value.kind() == FactKind::Registry)
        .unwrap()
}
#[test]
fn real_typed_refresh_chain_preserves_selected_pins_and_independent_phase_clocks() {
    let c = case();
    c.execution
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    assert_eq!(c.proposal.choice.registry, registry(1));
    assert_eq!(c.accepted.lease.registry_at_accept, registry(2));
    assert_eq!(c.proposal.original.offered.clock.expires_at_ms, 2000);
    assert_eq!(c.accepted.lease.expires_at_ms, 31500);
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
    assert_eq!(
        contract_manifest().unwrap()["publication"],
        "aq-admission/0.1.10"
    );
}
#[test]
fn unrelated_observation_revision_advances_do_not_reselect_or_invalidate_configuration() {
    let c = case();
    let mut confirmation = c.execution.confirmation.clone();
    let f = registry_fact(&mut confirmation);
    if let FactValue::Registry {
        current_registry, ..
    } = &mut f.body.value
    {
        *current_registry = registry(15);
    }
    f.body.revision = 15;
    f.record.version = Text::new("15").unwrap();
    resign(f);
    confirmation
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    assert_eq!(
        c.proposal.choice.selection_digest,
        c.proposal.spec.selection_digest
    );
    assert_eq!(c.proposal.choice.registry, registry(1));
    validate_registry_chain(&registry(7), &registry(7), &registry(7), &registry(7)).unwrap();
}
#[test]
fn all_known_registry_floors_reject_rollback_even_above_the_original_revision() {
    for values in [[2, 1, 3, 4], [1, 3, 2, 4], [1, 2, 5, 4]] {
        let r = values.map(registry);
        assert!(validate_registry_chain(&r[0], &r[1], &r[2], &r[3]).is_err());
    }
    let c = case();
    let mut a = c.accepted.clone();
    a.lease.registry_at_accept = registry(0);
    assert!(a.validate_for(&c.accept, &c.proposal).is_err());
}
#[test]
fn canonical_signed64_registry_revisions_and_exact_owner_configuration_are_required() {
    for text in [
        "0",
        "01",
        "+2",
        "-1",
        " 2",
        "9223372036854775808",
        "18446744073709551615",
    ] {
        if let Ok(version) = Text::new(text) {
            let mut value = registry(1);
            value.version = version;
            assert!(registry_revision(&value).is_err(), "{text}");
        }
    }
    registry_revision(&registry(i64::MAX as u64)).unwrap();
    for mode in 0..3 {
        let mut current = registry(4);
        match mode {
            0 => current.digest = Digest::of(b"changed configuration"),
            1 => current.id = Text::new("foreign-registry-owner").unwrap(),
            _ => current.version = Text::new("0").unwrap(),
        }
        assert!(
            validate_registry_chain(&registry(1), &registry(2), &registry(3), &current).is_err()
        );
    }
}
#[test]
fn actual_original_accepted_and_credential_associations_cannot_be_rehashed_away() {
    let c = case();
    for mode in 0..4 {
        let mut confirmation = c.execution.confirmation.clone();
        let f = registry_fact(&mut confirmation);
        if let FactValue::Registry {
            selected_registry,
            accepted_registry,
            credential_profile,
            credential_revision,
            ..
        } = &mut f.body.value
        {
            match mode {
                0 => *selected_registry = registry(2),
                1 => *accepted_registry = registry(3),
                2 => *credential_profile = reference("foreign-credential"),
                _ => *credential_revision += 1,
            }
        }
        resign(f);
        assert!(
            confirmation
                .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err()
        );
    }
}
#[test]
fn actual_fact_revision_is_the_current_registry_and_not_a_restamped_original() {
    let c = case();
    let mut confirmation = c.execution.confirmation.clone();
    let f = registry_fact(&mut confirmation);
    f.body.revision = 1;
    f.record.version = Text::new("1").unwrap();
    assert!(OwnerFact::record_digest(&f.body).is_err());
    let mut a = c.accepted.clone();
    a.lease.registry_at_accept.digest = Digest::of(b"rotated config");
    assert!(a.validate_for(&c.accept, &c.proposal).is_err());
}
#[test]
fn unavailable_cooldown_unknown_and_expired_facts_still_refuse_initial_execution() {
    let c = case();
    for mode in 0..5 {
        let mut confirmation = c.execution.confirmation.clone();
        let f = confirmation
            .facts
            .iter_mut()
            .find(|f| f.body.value.kind() == FactKind::Availability)
            .unwrap();
        if let FactValue::Availability { state, .. } = &mut f.body.value {
            match mode {
                0 => *state = AvailabilityState::Unavailable,
                1 => *state = AvailabilityState::Cooldown,
                2 => *state = AvailabilityState::Unknown,
                3 => f.body.expires_at_ms = 30000,
                _ => f.body.expires_at_ms += 1,
            }
        }
        resign(f);
        assert!(
            confirmation
                .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err()
        );
    }
}
#[test]
fn principal_inventory_context_and_private_backing_pins_remain_mandatory() {
    let c = case();
    for mode in 0..4 {
        let mut confirmation = c.execution.confirmation.clone();
        let kind = match mode {
            0 => FactKind::Principal,
            1 => FactKind::Inventory,
            2 => FactKind::Context,
            _ => FactKind::PrivateMaterialBacking,
        };
        let f = confirmation
            .facts
            .iter_mut()
            .find(|f| f.body.value.kind() == kind)
            .unwrap();
        match &mut f.body.value {
            FactValue::Principal { principal, .. } => *principal = reference("changed-principal"),
            FactValue::Inventory { harness, .. } => {
                harness.cli_version = Text::new("changed-version").unwrap()
            }
            FactValue::Context { task, .. } => task.version += 1,
            FactValue::PrivateMaterialBacking { backing_digest, .. } => {
                *backing_digest = Digest::of(b"changed-backing")
            }
            _ => unreachable!(),
        }
        resign(f);
        assert!(
            confirmation
                .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
                .is_err()
        );
    }
}
#[test]
fn profile_negotiation_is_exact_from_proposal_and_never_upgrades_old_history() {
    let c = case();
    for mode in 0..4 {
        let mut p = c.proposal.clone();
        match mode {
            0 => {
                p.negotiation.contract = aq_admission::preparation_v2::contract_artifact().unwrap()
            }
            1 => {
                p.negotiation.features.pop();
            }
            2 => p.negotiation.features.push(p.negotiation.features[0]),
            _ => p.negotiation.contract.version = Text::new("2").unwrap(),
        }
        assert!(p.validate().is_err());
    }
    let legacy = preparation_refresh_common::legacy_documents_for_refusal();
    for d in legacy {
        assert!(decode(&d).is_err());
    }
    for d in documents(&c) {
        let bytes = canonical(&d);
        decode(&bytes).unwrap();
        assert!(aq_admission::preparation_v2::decode(&bytes).is_err());
    }
}
#[test]
fn strict_new_wire_refuses_unknown_duplicate_oversized_or_wrong_family_documents() {
    let c = case();
    let d = Document::Prepare(Box::new(c.prepare));
    let mut value = serde_json::to_value(&d).unwrap();
    value["body"]["authority"] = json!(true);
    assert!(decode(&canonical(&value)).is_err());
    value = serde_json::to_value(&d).unwrap();
    value["body"]["schema_version"] = json!(3);
    assert!(decode(&canonical(&value)).is_err());
    let text = String::from_utf8(canonical(&d)).unwrap().replacen(
        "\"schema_version\":2",
        "\"schema_version\":2,\"schema_version\":2",
        1,
    );
    assert!(decode(text.as_bytes()).is_err());
    assert!(decode(&vec![b'x'; MAX_BYTES + 1]).is_err());
}
#[test]
fn fresh_profile_never_extends_original_accepted_or_replayed_execution_deadlines() {
    let c = case();
    let mut x = c.execution.clone();
    x.confirmation.checked_at_ms = c.accepted.lease.expires_at_ms;
    x.issued_at_ms = x.confirmation.checked_at_ms;
    assert!(
        x.validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
            .is_err()
    );
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
    let mut replay = c.delivery.clone();
    replay.delivery.clock = aq_admission::transport::DeliveryClock::new(45000, 50000).unwrap();
    let next = replay
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
    assert_eq!(old.deadline_ns(), next.deadline_ns());
    assert!(
        replay
            .sample(
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
}
#[test]
fn normative_schema_contract_and_facts_are_exact_and_never_claim_owner_authentication() {
    let generated = json!({"schema":schema(),"contract":{"artifact":contract_artifact().unwrap(),"manifest":contract_manifest().unwrap()},"fixture":fixtures()});
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts/preparation-refresh-v1.json");
    if std::env::var_os("AQ_UPDATE_PREPARATION_REFRESH_V1").is_some() {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&generated).unwrap()),
        )
        .unwrap();
    } else {
        let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(expected, generated);
    }
    assert_ne!(
        contract_artifact().unwrap(),
        aq_admission::preparation_v2::contract_artifact().unwrap()
    );
    assert_eq!(
        schema()["$id"],
        "urn:aq:admission:preparation-registry-refresh:1"
    );
    assert_eq!(generated["fixture"]["authority_admission"], false);
    assert!(
        schema()["$comment"]
            .as_str()
            .unwrap()
            .contains("no authentication")
    );
}

#[test]
fn explicit_interactive_policy_three_extends_only_the_new_mode_to_one_hour() {
    let c = case();
    let mut policy = c.proposal.policy.clone();
    policy.maximum_execution_lease_ms = 120000;
    assert!(policy.validate().is_ok());
    policy.maximum_execution_lease_ms += 1;
    assert!(
        policy.validate().is_err(),
        "old policy2 keeps its exact two-minute bound"
    );
    policy.schema_version = 3;
    policy.maximum_execution_lease_ms = 3_600_000;
    assert!(policy.validate().is_ok());
    let mut manifest = c.proposal.original.offered.offer.manifest.clone();
    assert!(policy.validate_for_manifest(&manifest).is_err());
    manifest.terminal.input_enabled = true;
    manifest.harness.harness = aq_session_protocol::Text::new("codex").unwrap();
    manifest.harness.cli_version = aq_session_protocol::Text::new("0.166.0").unwrap();
    assert!(
        policy.validate_for_manifest(&manifest).is_err(),
        "input flag alone is not this factory"
    );
    manifest.harness.adapter.id =
        aq_session_protocol::Text::new("aq.codex-result-local-adapter").unwrap();
    manifest.harness.adapter.version = aq_session_protocol::Text::new("1").unwrap();
    // Correlation only; the actual server checks the whole installed definition.
    assert!(policy.validate_for_manifest(&manifest).is_ok());
    manifest.harness.adapter.version = aq_session_protocol::Text::new("2").unwrap();
    assert!(
        policy.validate_for_manifest(&manifest).is_ok(),
        "definition revision is not schema/recipe version"
    );
    policy.maximum_execution_lease_ms += 1;
    assert!(policy.validate_for_manifest(&manifest).is_err());
}

#[test]
fn long_delivery_is_explicit_and_keeps_original_clock_bounds_and_contract() {
    use aq_admission::transport::DeliveryClock;
    assert!(DeliveryClock::new(1000, 3_601_000).is_err());
    let clock = DeliveryClock::new_interactive_session(1000, 3_601_000).unwrap();
    assert!(clock.validate().is_err());
    assert!(
        clock
            .client_deadline_ns(10_000_000_000, 10_100_000_000)
            .is_err()
    );
    assert_eq!(
        clock
            .client_interactive_deadline_ns(10_000_000_000, 10_100_000_000)
            .unwrap(),
        3_610_000_000_000
    );
    assert!(
        clock
            .client_interactive_deadline_ns(10_000_000_000, 3_610_000_000_000)
            .is_err()
    );
    assert!(DeliveryClock::new_interactive_session(1000, 3_601_001).is_err());
    let old = contract_artifact().unwrap();
    let long = long_contract_artifact().unwrap();
    assert_ne!(old, long);
    assert_eq!(
        schema()["$defs"]["PreparationRefreshV1PhasePolicy"]["properties"]["schema_version"]["const"],
        json!(2)
    );
    assert_eq!(
        long_schema()["$defs"]["PreparationRefreshV1PhasePolicy"]["properties"]["schema_version"]["const"],
        json!(3)
    );
    Negotiation {
        contract: long.clone(),
        features: required_features(),
    }
    .validate()
    .unwrap();
    println!(
        "LONG_REFRESH_CONTRACT {}",
        serde_json::to_string(&long).unwrap()
    );
    println!(
        "LONG_FROZEN_CONTRACT {}",
        serde_json::to_string(&aq_admission::preparation_v2::long_contract_artifact().unwrap())
            .unwrap()
    );
}
