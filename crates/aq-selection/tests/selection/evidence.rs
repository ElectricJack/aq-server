use super::*;

#[test]
fn observations_scope_account_endpoint_model_node_and_credential_revision_exactly() {
    let (catalog, mut input) = fixture();
    let mut unrelated = input.observations[0].clone();
    unrelated.observation_id = Uuid::from_u128(55);
    unrelated.state = Availability::QuotaUnavailable;
    unrelated.scope.account_reference = "unrelated-account".into();
    input.observations.push(unrelated);
    assert_eq!(
        select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap()
            .choice
            .model,
        "model-a"
    );
    for field in ["endpoint", "model", "credential_revision"] {
        let mut input = input.clone();
        let o = input.observations.last_mut().unwrap();
        o.scope.account_reference = "billing-a".into();
        match field {
            "endpoint" => o.scope.endpoint_reference = "other-endpoint".into(),
            "model" => o.scope.model = Some("other-model".into()),
            _ => o.scope.credential_revision = 2,
        };
        assert_eq!(
            select(&catalog, &input)
                .unwrap()
                .resolved
                .unwrap()
                .choice
                .model,
            "model-a"
        );
    }
}
#[test]
fn worker_failure_cannot_claim_account_wide_availability_or_survive_boot_reuse() {
    let (catalog, mut input) = fixture();
    let o = &mut input.observations[0];
    o.source = ObservationSource::Worker {
        node_id: Uuid::from_u128(4),
        boot_id: Uuid::from_u128(5),
        attempt_id: Uuid::from_u128(6),
        selection_digest: aq_session_protocol::Digest::of(b"sealed"),
    };
    o.state = Availability::QuotaUnavailable;
    assert!(select(&catalog, &input).is_err());
    input.observations[0].scope.node_id = Some(Uuid::from_u128(4));
    assert_eq!(
        select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap()
            .choice
            .model,
        "model-b"
    );
    input.node.target = ExecutionTarget::Worker {
        node_id: Uuid::from_u128(4),
        boot_id: Uuid::from_u128(7),
    };
    input.project.target = input.node.target.clone();
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.traces[0].state, CandidateState::StaleUnknown);
}
#[test]
fn classified_worker_failure_is_bound_to_exact_sealed_choice_and_current_boot() {
    let (catalog, input) = fixture();
    let selected = select(&catalog, &input).unwrap().resolved.unwrap();
    let ExecutionTarget::Worker { node_id, boot_id } = selected.target else {
        panic!()
    };
    let attempt = Uuid::from_u128(9001);
    let mut report = StructuredFailure {
        schema_version: 1,
        operation_id: Uuid::from_u128(9002),
        attempt_id: attempt,
        selection_digest: digest(&selected).unwrap(),
        node_id,
        boot_id,
        classifier: selected.choice.failure_classifier.clone().unwrap(),
        sequence: 1,
        failure: ProviderFailure::Quota,
        lifecycle_event_id: None,
    };
    let context = |current_boot, sealed_attempt| aq_selection::FailureContext {
        observation_id: Uuid::from_u128(9005),
        authenticated_node: node_id,
        current_boot,
        sealed_attempt,
        sealed: &selected,
        revision: 1,
        now_ms: input.now_ms,
        ttl_ms: 5000,
        cooldown_ms: 1000,
    };
    let qualified = context(boot_id, attempt);
    let observation = derive_worker_observation(&report, &qualified).unwrap();
    assert_eq!(
        observation.scope.account_reference,
        selected.choice.account_reference
    );
    assert_eq!(observation.scope.model, Some(selected.choice.model.clone()));
    assert_eq!(observation.scope.node_id, Some(node_id));
    for wrong_context in [
        context(Uuid::from_u128(9003), attempt),
        context(boot_id, Uuid::from_u128(9004)),
    ] {
        assert!(derive_worker_observation(&report, &wrong_context).is_err());
    }
    report.classifier = artifact("unqualified-classifier");
    assert!(derive_worker_observation(&report, &qualified).is_err());
    report.classifier = selected.choice.failure_classifier.clone().unwrap();
    report.selection_digest = Digest::of(b"foreign-choice");
    assert!(derive_worker_observation(&report, &qualified).is_err());
}
#[test]
fn independent_matching_source_cannot_clear_another_sources_live_restriction() {
    let (catalog, mut input) = fixture();
    let mut restricted = input.observations[0].clone();
    restricted.observation_id = Uuid::from_u128(9101);
    restricted.source = ObservationSource::Operator {
        principal_id: Uuid::from_u128(9102),
    };
    restricted.state = Availability::QuotaUnavailable;
    restricted.received_at_ms = input.now_ms - 10;
    restricted.expires_at_ms = input.now_ms + 1000;
    restricted.cooldown_until_ms = input.now_ms + 1000;
    let mut available = restricted.clone();
    available.observation_id = Uuid::from_u128(9103);
    available.source = ObservationSource::Operator {
        principal_id: Uuid::from_u128(9104),
    };
    available.state = Availability::Available;
    available.received_at_ms = input.now_ms - 1;
    input.observations.extend([restricted, available]);
    let result = select(&catalog, &input).unwrap();
    assert_eq!(result.resolved.unwrap().choice.provider, "fixture-b");
}
#[test]
fn source_sequence_precedes_wall_clock_when_history_contains_clock_correction() {
    let (catalog, mut input) = fixture();
    let mut later = input.observations[0].clone();
    later.observation_id = Uuid::from_u128(9201);
    later.sequence += 1;
    later.state = Availability::QuotaUnavailable;
    later.received_at_ms = input.observations[0].received_at_ms - 1;
    input.observations.push(later);
    assert_eq!(
        select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap()
            .choice
            .provider,
        "fixture-b"
    );
}
