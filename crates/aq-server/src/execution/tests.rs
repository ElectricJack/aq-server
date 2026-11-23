use super::*;

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
