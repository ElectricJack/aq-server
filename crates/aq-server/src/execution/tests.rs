use super::*;

fn assert_refused<T: std::fmt::Debug>(result: Result<T>, expected: ErrorCode) {
    match result {
        Err(Error::Refused(code, _)) => assert_eq!(code, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

fn granted_attempt() -> AttemptState {
    let identity = NodeIdentity {
        node_id: Uuid::new_v4(),
        boot_id: Uuid::new_v4(),
    };
    let attempt = AttemptRef {
        attempt_id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        generation: 1,
    };
    let spec = ExecutionSpec {
        project_id: Uuid::new_v4(),
        harness: FAKE_HARNESS_CAPABILITY.into(),
        resolved_spec_digest: String::new(),
        fixture_output: "finished".into(),
        resources: Resources {
            slots: 1,
            cpu_millis: 100,
            memory_bytes: 1024,
            disk_bytes: 1024,
        },
        required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
        bindings: None,
    }
    .seal();
    let offer = Offer {
        offer_id: Uuid::new_v4(),
        identity: identity.clone(),
        attempt: attempt.clone(),
        spec,
        offer_expires_at_ms: 1000,
        reservation_ttl_ms: 1000,
    };
    let grant = Grant {
        identity,
        attempt,
        offer_id: offer.offer_id,
        reservation_id: Uuid::new_v4(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        grant_sequence: 1,
        valid_for_ms: 100,
        reservation_deadline_ms: 1000,
    };
    AttemptState {
        task_id: Uuid::new_v4(),
        offer,
        status: AuthorityStatus::Granted,
        grant: Some(grant),
        grant_expires_at_ms: 100,
        last_observation: None,
        report: None,
        disposition: None,
        cancellation_id: None,
        cancellation_reason: None,
        stopped: false,
        may_have_started: false,
    }
}

#[test]
fn result_disposition_requires_stop_retention_and_current_authority() {
    use ResultDisposition::*;
    let original = granted_attempt();
    let mut stale = original.offer.identity.clone();
    stale.boot_id = Uuid::new_v4();
    for (stopped, retained, cancelled, identity, now, expected) in [
        (true, true, false, &original.offer.identity, 99, Accepted),
        (
            true,
            true,
            false,
            &original.offer.identity,
            100,
            RetainedStale,
        ),
        (true, true, false, &stale, 99, RetainedStale),
        (true, true, true, &stale, 100, RetainedCancelled),
        (false, true, true, &stale, 100, Rejected),
        (true, false, true, &stale, 100, Rejected),
    ] {
        let mut state = original.clone();
        state.cancellation_id = cancelled.then(Uuid::new_v4);
        assert_eq!(
            state.result_disposition(identity, stopped, retained, now),
            expected
        );
    }
    for status in [
        AuthorityStatus::Offered,
        AuthorityStatus::Cancelling,
        AuthorityStatus::Finished,
        AuthorityStatus::Declined,
    ] {
        let mut state = original.clone();
        state.status = status;
        assert_eq!(
            state.result_disposition(&state.offer.identity, true, true, 99),
            RetainedStale
        );
    }
}

#[test]
fn reconciliation_never_extends_or_transfers_a_grant() {
    let state = granted_attempt();
    let local = ReconcileAttempt {
        attempt: state.offer.attempt.clone(),
        previous_boot_id: state.offer.identity.boot_id,
        phase: Phase::Running,
        launch_id: Some(Uuid::new_v4()),
        stop_certainty: StopCertainty::Running,
        report_id: None,
    };
    let request = ReconcileRequest {
        identity: state.offer.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempts: vec![local.clone()],
        cursor: None,
        max_items: 1,
    };
    let reconcile = |local: &ReconcileAttempt, request: &ReconcileRequest, now| {
        reconcile_attempt(
            Some(state.clone()),
            &state.offer.identity,
            request,
            local,
            &AdmissionClock::Frozen(now),
        )
        .unwrap()
    };
    let live = reconcile(&local, &request, 99);
    assert_eq!(live.action, ReconcileAction::Renewed);
    assert_eq!(live.grant.unwrap().valid_for_ms, 1);
    let expired = reconcile(&local, &request, 100);
    assert_eq!(expired.action, ReconcileAction::StopAndPreserve);
    assert!(expired.grant.is_none());
    let mut replacement = request.clone();
    replacement.identity.boot_id = Uuid::new_v4();
    assert_eq!(
        reconcile(&local, &replacement, 99).action,
        ReconcileAction::StopAndPreserve
    );
    let mut stopped = local.clone();
    stopped.stop_certainty = StopCertainty::ConfirmedStopped;
    assert_eq!(
        reconcile(&stopped, &replacement, 99).action,
        ReconcileAction::AwaitReport
    );
    let mut foreign = local.clone();
    foreign.attempt.generation += 1;
    assert_eq!(
        reconcile(&foreign, &request, 99).action,
        ReconcileAction::Unknown
    );
    foreign = local.clone();
    foreign.previous_boot_id = Uuid::new_v4();
    assert_eq!(
        reconcile(&foreign, &request, 99).action,
        ReconcileAction::Unknown
    );
    // An absent attempt never asks the clock to establish authority.
    let clock = AdmissionClock::Live(std::sync::Arc::new(|| panic!("unexpected clock read")));
    assert_eq!(
        reconcile_attempt(None, &request.identity, &request, &local, &clock)
            .unwrap()
            .action,
        ReconcileAction::Unknown
    );
}

#[test]
fn source_receipt_cannot_release_another_artifact_or_omit_bundle_bytes() {
    let offer = granted_attempt().offer;
    let source = SourceBinding {
        repository_id: Uuid::new_v4(),
        binding_generation: 1,
        logical_identity: "fixture/project".into(),
        base: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "1".repeat(40),
        },
    };
    let manifest = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint: ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "2".repeat(40),
        },
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        source: source.clone(),
        artifact_id: Uuid::new_v4(),
        manifest_digest: "a".repeat(64),
        bundle_digest: Some("b".repeat(64)),
        manifest_bytes: 64,
        bundle_bytes: 128,
    };
    let receipt = CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: manifest.transfer_id,
        repository_id: source.repository_id,
        binding_generation: source.binding_generation,
        source: manifest.checkpoint.clone(),
        retention_handle: manifest.artifact_id,
        manifest_digest: "c".repeat(64),
    };
    let artifact = ArtifactRef {
        artifact_id: manifest.artifact_id,
        digest: manifest.manifest_digest.clone(),
        bytes: manifest.manifest_bytes + manifest.bundle_bytes,
        central_receipt: Some(receipt.receipt_id.to_string()),
    };
    let report = ResultReport {
        identity: offer.identity,
        report_id: Uuid::new_v4(),
        supersedes_report_id: None,
        attempt: offer.attempt,
        resolved_spec_digest: offer.spec.resolved_spec_digest,
        outcome: ProcessOutcome::Success,
        stop_certainty: StopCertainty::ConfirmedStopped,
        claimed_success: true,
        output: "finished".into(),
        artifacts: vec![artifact.clone()],
        pending_operation_ids: vec![],
    };
    let check = |artifact: &ArtifactRef, receipt: &CentralRetentionReceipt| {
        validate_retained_artifact(&source, &report, artifact, &manifest, receipt)
    };
    check(&artifact, &receipt).unwrap();
    let mut foreign_receipt = receipt.clone();
    foreign_receipt.retention_handle = Uuid::new_v4();
    assert!(matches!(
        check(&artifact, &foreign_receipt),
        Err(Error::Refused(ErrorCode::InvalidRequest, _))
    ));
    let mut incomplete_artifact = artifact;
    incomplete_artifact.bytes = manifest.manifest_bytes;
    assert!(matches!(
        check(&incomplete_artifact, &receipt),
        Err(Error::Refused(ErrorCode::InvalidRequest, _))
    ));
}

#[test]
fn renewal_checks_authority_before_expiry_and_sequence_without_changing_state() {
    let state = granted_attempt();
    let request = RenewRequest {
        identity: state.offer.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: state.offer.attempt.clone(),
        grant_sequence: 1,
    };
    let before = encode(&state).unwrap();
    let grant = state.grant_for_renewal(&request, 99).unwrap();
    assert_eq!(grant.grant_sequence, 2);
    assert_eq!(encode(&state).unwrap(), before);

    let mut stale = request.clone();
    stale.grant_sequence = 0;
    assert_refused(
        state.grant_for_renewal(&stale, 99),
        ErrorCode::StaleGeneration,
    );
    assert_refused(
        state.grant_for_renewal(&stale, 100),
        ErrorCode::GrantExpired,
    );
    stale.identity.boot_id = Uuid::new_v4();
    assert_refused(state.grant_for_renewal(&stale, 100), ErrorCode::Cancelled);

    for status in [
        AuthorityStatus::Offered,
        AuthorityStatus::Cancelling,
        AuthorityStatus::Finished,
        AuthorityStatus::Declined,
    ] {
        let mut inactive = state.clone();
        inactive.status = status;
        assert_refused(
            inactive.grant_for_renewal(&request, 99),
            ErrorCode::Cancelled,
        );
    }
    let mut stopped = state.clone();
    stopped.stopped = true;
    assert_refused(
        stopped.grant_for_renewal(&request, 99),
        ErrorCode::Cancelled,
    );

    let mut missing = state.clone();
    missing.grant = None;
    assert_refused(
        missing.grant_for_renewal(&request, 99),
        ErrorCode::InvalidRequest,
    );
    let mut exhausted = state;
    exhausted.grant.as_mut().unwrap().grant_sequence = u64::MAX;
    let mut request = request;
    request.grant_sequence = u64::MAX;
    assert_refused(
        exhausted.grant_for_renewal(&request, 99),
        ErrorCode::InvalidRequest,
    );
}

#[test]
fn observations_keep_start_and_stop_evidence_and_refuse_regressions_without_mutation() {
    let mut state = granted_attempt();
    let mut observation = Observation {
        identity: state.offer.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: state.offer.attempt.clone(),
        sequence: 1,
        phase: Phase::LaunchUncertain,
        stop_certainty: StopCertainty::NotStarted,
        detail: "launch outcome unknown".into(),
    };
    state.observe(&observation).unwrap();
    assert!(state.may_have_started);
    assert!(!state.stopped);
    observation.sequence = 2;
    observation.phase = Phase::Collecting;
    observation.stop_certainty = StopCertainty::ConfirmedStopped;
    state.observe(&observation).unwrap();
    assert!(state.stopped);
    assert!(state.may_have_started);

    let before = encode(&state).unwrap();
    let mut invalid = observation.clone();
    invalid.identity.boot_id = Uuid::new_v4();
    assert_refused(state.observe(&invalid), ErrorCode::StaleBoot);
    assert_eq!(encode(&state).unwrap(), before);
    assert_refused(state.observe(&observation), ErrorCode::IdConflict);
    assert_eq!(encode(&state).unwrap(), before);
    invalid = observation.clone();
    invalid.sequence += 1;
    invalid.phase = Phase::Running;
    assert_refused(state.observe(&invalid), ErrorCode::InvalidRequest);
    assert_eq!(encode(&state).unwrap(), before);
    invalid.phase = Phase::Reporting;
    invalid.stop_certainty = StopCertainty::Running;
    assert_refused(state.observe(&invalid), ErrorCode::InvalidRequest);
    assert_eq!(encode(&state).unwrap(), before);

    observation.sequence += 1;
    observation.phase = Phase::Reporting;
    observation.stop_certainty = StopCertainty::NotStarted;
    state.observe(&observation).unwrap();
    assert!(state.may_have_started);
    assert!(state.stopped);
    assert_eq!(state.last_observation, Some(observation));
}
