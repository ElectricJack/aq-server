use aq_worker_protocol::*;
#[test]
fn generated_schema_has_no_drift() {
    assert_eq!(
        schema(),
        include_str!("../fixtures/execution-v2.schema.json")
    );
}
#[test]
fn wire_is_strict_and_payload_digest_is_canonical() {
    let a = serde_json::json!({"a":1,"b":2});
    let b = serde_json::json!({"b":2,"a":1});
    assert_eq!(payload_digest(&a), payload_digest(&b));
    assert!(serde_json::from_value::<Ack>(serde_json::json!({"operation_id":"00000000-0000-0000-0000-000000000001","surprise":true})).is_err());
    assert!(
        Ack {
            operation_id: uuid::Uuid::nil()
        }
        .validate()
        .is_err()
    );
}
#[cfg(feature = "test-support")]
mod scenarios {
    use super::*;
    use aq_worker_protocol::testing::*;
    #[test]
    fn lost_acceptance_replay_reduces_ttl_and_changed_bytes_conflict() {
        let offer = sample_offer();
        let req = sample_acceptance(&offer);
        let mut peer = FakeCoordinator::new(offer, 100);
        let first = peer.accept(&req).unwrap();
        peer.now_ms += 250;
        let replay = peer.accept(&req).unwrap();
        assert_eq!(replay.grant.grant_sequence, first.grant.grant_sequence);
        assert_eq!(replay.grant.valid_for_ms, 9750);
        let mut changed = req.clone();
        changed.reservation_deadline_ms += 1;
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::IdConflict
        );
        peer.now_ms = 10_100;
        assert_eq!(peer.accept(&req).unwrap_err().code, ErrorCode::GrantExpired);
    }
    #[test]
    fn expiry_stale_incarnation_and_generation_are_refused() {
        let offer = sample_offer();
        let req = sample_acceptance(&offer);
        let mut peer = FakeCoordinator::new(offer, 100_000);
        assert_eq!(peer.accept(&req).unwrap_err().code, ErrorCode::OfferExpired);
        let mut changed = req.clone();
        changed.identity.boot_id = uuid::Uuid::from_u128(99);
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::StaleBoot
        );
        changed = req;
        changed.attempt.generation += 1;
        assert_eq!(
            peer.accept(&changed).unwrap_err().code,
            ErrorCode::StaleGeneration
        );
    }
    #[test]
    fn observation_sequences_and_report_replays_are_immutable() {
        let offer = sample_offer();
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        peer.accept(&sample_acceptance(&offer)).unwrap();
        let mut obs = Observation {
            identity: offer.identity.clone(),
            operation_id: uuid::Uuid::from_u128(10),
            attempt: offer.attempt.clone(),
            sequence: 1,
            phase: Phase::Running,
            stop_certainty: StopCertainty::Running,
            detail: String::new(),
        };
        let ack = peer.observe(&obs).unwrap();
        assert_eq!(ack, peer.observe(&obs).unwrap());
        obs.phase = Phase::Stopping;
        assert_eq!(peer.observe(&obs).unwrap_err().code, ErrorCode::IdConflict);
        let mut report = sample_report(&offer);
        let result = peer.report(&report).unwrap();
        assert_eq!(result, peer.report(&report).unwrap());
        report.output.push('!');
        assert_eq!(
            peer.report(&report).unwrap_err().code,
            ErrorCode::IdConflict
        );
    }
    #[test]
    fn cancellation_race_and_late_generation_retain_evidence() {
        let offer = sample_offer();
        let report = sample_report(&offer);
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        peer.accept(&sample_acceptance(&offer)).unwrap();
        peer.cancelled = true;
        assert_eq!(
            peer.report(&report).unwrap().disposition,
            ResultDisposition::RetainedCancelled
        );
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        peer.accept(&sample_acceptance(&offer)).unwrap();
        let first = peer.report(&report).unwrap();
        peer.cancelled = true;
        assert_eq!(peer.report(&report).unwrap(), first);
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        peer.accept(&sample_acceptance(&offer)).unwrap();
        peer.report(&report).unwrap();
        let mut late = report;
        late.report_id = uuid::Uuid::from_u128(11);
        late.supersedes_report_id = Some(uuid::Uuid::from_u128(9));
        peer.active_identity.boot_id = uuid::Uuid::from_u128(99);
        assert_eq!(
            peer.report(&late).unwrap().disposition,
            ResultDisposition::RetainedStale
        );
    }
    #[test]
    fn result_requires_grant_binding_and_pending_operations_block_release() {
        let offer = sample_offer();
        let report = sample_report(&offer);
        let mut peer = FakeCoordinator::new(offer.clone(), 100);
        assert_eq!(
            peer.report(&report).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        peer.accept(&sample_acceptance(&offer)).unwrap();
        for field in [0, 1, 2] {
            let mut forged = report.clone();
            match field {
                0 => forged.identity.boot_id = uuid::Uuid::from_u128(99),
                1 => forged.attempt.generation += 1,
                _ => forged.resolved_spec_digest = "0".repeat(64),
            };
            assert_eq!(
                peer.report(&forged).unwrap_err().code,
                ErrorCode::StaleGeneration
            );
        }
        let mut pending = report.clone();
        pending
            .pending_operation_ids
            .push(uuid::Uuid::from_u128(20));
        assert!(!peer.report(&pending).unwrap().retention_release);
        let mut expired = FakeCoordinator::new(offer.clone(), 100);
        expired.accept(&sample_acceptance(&offer)).unwrap();
        expired.now_ms = 10_100;
        assert_eq!(
            expired.report(&report).unwrap().disposition,
            ResultDisposition::RetainedStale
        );
    }
    #[test]
    fn spec_tampering_and_response_overflow_are_invalid() {
        let mut offer = sample_offer();
        offer.validate().unwrap();
        offer.spec.fixture_output.push('!');
        assert!(offer.validate().is_err());
        assert!(
            PollResponse {
                server_time_ms: 100,
                offers: vec![sample_offer(); MAX_BATCH + 1],
                directives: vec![]
            }
            .validate()
            .is_err()
        );
        assert!(
            ReconcileRequest {
                identity: sample_offer().identity,
                operation_id: uuid::Uuid::from_u128(12),
                attempts: vec![],
                cursor: Some("x".repeat(257)),
                max_items: 1
            }
            .validate()
            .is_err()
        );
    }
}

#[test]
fn published_valid_and_invalid_vectors_execute() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/conformance-v2.json")).unwrap();
    for value in fixtures["valid"].as_array().unwrap() {
        serde_json::from_value::<WireMessage>(value.clone())
            .unwrap()
            .validate()
            .unwrap();
    }
    for value in fixtures["invalid"].as_array().unwrap() {
        let message = serde_json::from_value::<WireMessage>(value.clone());
        assert!(message.is_err() || message.unwrap().validate().is_err());
    }
}
#[cfg(feature = "test-support")]
#[test]
fn nil_optional_recovery_and_duplicate_pending_ids_are_refused() {
    use aq_worker_protocol::testing::*;
    let mut report = sample_report(&sample_offer());
    report.supersedes_report_id = Some(uuid::Uuid::nil());
    assert!(report.validate().is_err());
    report.supersedes_report_id = None;
    report.pending_operation_ids = vec![uuid::Uuid::nil()];
    assert!(report.validate().is_err());
    report.pending_operation_ids = vec![uuid::Uuid::from_u128(88); 2];
    assert!(report.validate().is_err());
    let mut attempt = ReconcileAttempt {
        attempt: sample_offer().attempt,
        previous_boot_id: uuid::Uuid::nil(),
        phase: Phase::Running,
        launch_id: None,
        stop_certainty: StopCertainty::Running,
        report_id: None,
    };
    assert!(attempt.validate().is_err());
    attempt.previous_boot_id = uuid::Uuid::from_u128(2);
    attempt.launch_id = Some(uuid::Uuid::nil());
    assert!(attempt.validate().is_err());
}
