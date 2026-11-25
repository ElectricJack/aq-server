use super::*;
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
