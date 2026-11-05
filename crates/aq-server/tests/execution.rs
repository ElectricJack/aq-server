use aq_server::{
    execution::{AuthorityStatus, Error},
    store::{NewNode, Store},
    test_support::TestDb,
    tokens::{new_node_credential, verifier},
};
use aq_worker_protocol::*;
use chrono::{Duration, Utc};
use uuid::Uuid;

const NOW: u64 = 1_000_000;
struct Harness {
    store: Store,
    identity: NodeIdentity,
    credential: String,
    db: TestDb,
}
impl Harness {
    async fn new() -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let now = Utc::now();
        let owner = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await
            .unwrap()
            .unwrap();
        let credential = new_node_credential();
        store
            .create_enrollment("test-code", owner.id, now, now + Duration::minutes(5))
            .await
            .unwrap();
        let node = store
            .redeem_enrollment(
                "test-code",
                NewNode {
                    name: "test".into(),
                    credential_hash: verifier(&credential),
                    worker_version: "0.1.0".into(),
                    protocol_version: 1,
                },
                now,
            )
            .await
            .unwrap()
            .unwrap();
        let identity = NodeIdentity {
            node_id: node.id,
            boot_id: Uuid::new_v4(),
        };
        store
            .execution_register(&register(&identity))
            .await
            .unwrap();
        Self {
            store,
            identity,
            credential,
            db,
        }
    }
    async fn offer(&self, task_id: Uuid) -> Offer {
        let offer = make_offer(&self.identity);
        self.store.execution_offer(task_id, &offer).await.unwrap();
        offer
    }
}
fn register(identity: &NodeIdentity) -> RegisterRequest {
    RegisterRequest {
        identity: identity.clone(),
        operation_id: Uuid::new_v4(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        worker_version: "0.1.0".into(),
        capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
    }
}
fn make_offer(identity: &NodeIdentity) -> Offer {
    let spec = ExecutionSpec {
        bindings: None,
        project_id: Uuid::new_v4(),
        harness: FAKE_HARNESS_CAPABILITY.into(),
        resolved_spec_digest: String::new(),
        fixture_output: "fixture finished".into(),
        resources: Resources {
            slots: 1,
            cpu_millis: 100,
            memory_bytes: 1024,
            disk_bytes: 1024,
        },
        required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
    };
    let spec = spec.seal();
    Offer {
        offer_id: Uuid::new_v4(),
        identity: identity.clone(),
        attempt: AttemptRef {
            attempt_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        spec,
        offer_expires_at_ms: NOW + 30_000,
        reservation_ttl_ms: 30_000,
    }
}
fn acceptance(offer: &Offer) -> AcceptRequest {
    AcceptRequest {
        identity: offer.identity.clone(),
        operation_id: Uuid::new_v4(),
        offer_id: offer.offer_id,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        reservation_id: Uuid::new_v4(),
        reservation_deadline_ms: 500_000,
        reservation_remaining_ms: 20_000,
    }
}
fn report(offer: &Offer) -> ResultReport {
    ResultReport {
        identity: offer.identity.clone(),
        report_id: Uuid::new_v4(),
        supersedes_report_id: None,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        outcome: ProcessOutcome::Success,
        stop_certainty: StopCertainty::ConfirmedStopped,
        claimed_success: true,
        output: "finished".into(),
        artifacts: vec![],
        pending_operation_ids: vec![],
    }
}
fn is_error<T: std::fmt::Debug>(result: Result<T, Error>, code: ErrorCode) {
    assert!(
        matches!(result, Err(Error::Refused(c, _)) if c == code),
        "expected {code:?}, got {result:?}"
    );
}

#[tokio::test]
async fn acceptance_replay_is_durable_and_does_not_extend_authority() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    let accept = acceptance(&offer);
    let first = h.store.execution_accept(&accept, NOW).await.unwrap();
    let reconnected = Store::connect_with(h.db.options()).await.unwrap();
    let replay = reconnected
        .execution_accept(&accept, NOW + 10_000)
        .await
        .unwrap();
    assert_eq!(first.grant.grant_sequence, replay.grant.grant_sequence);
    assert_eq!(first.grant.reservation_id, replay.grant.reservation_id);
    assert_eq!(replay.grant.valid_for_ms, MAX_GRANT_MS - 10_000);
    is_error(
        reconnected
            .execution_accept(&accept, NOW + MAX_GRANT_MS)
            .await,
        ErrorCode::GrantExpired,
    );
    let mut changed = accept.clone();
    changed.reservation_id = Uuid::new_v4();
    is_error(
        reconnected.execution_accept(&changed, NOW + 10_000).await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn concurrent_claims_for_one_task_leave_one_unsettled_attempt() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let a = make_offer(&h.identity);
    let b = make_offer(&h.identity);
    let (a, b) = tokio::join!(
        h.store.execution_offer(task, &a),
        h.store.execution_offer(task, &b)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let offer = a.or(b).unwrap();
    let accept = acceptance(&offer);
    let (a, b) = tokio::join!(
        h.store.execution_accept(&accept, NOW),
        h.store.execution_accept(&accept, NOW)
    );
    assert_eq!(a.unwrap(), b.unwrap());
}

#[tokio::test]
async fn renewal_replay_preserves_expiry_and_rejects_stale_sequence() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let request = RenewRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        grant_sequence: 1,
    };
    let renewed = h
        .store
        .execution_renew(&request, NOW + 10_000)
        .await
        .unwrap();
    assert_eq!(renewed.grant.grant_sequence, 2);
    let replay = h
        .store
        .execution_renew(&request, NOW + 20_000)
        .await
        .unwrap();
    assert_eq!(replay.grant.valid_for_ms, MAX_GRANT_MS - 10_000);
    let mut stale = request.clone();
    stale.operation_id = Uuid::new_v4();
    is_error(
        h.store.execution_renew(&stale, NOW + 20_000).await,
        ErrorCode::StaleGeneration,
    );
    is_error(
        h.store
            .execution_renew(&request, NOW + 10_000 + MAX_GRANT_MS)
            .await,
        ErrorCode::GrantExpired,
    );
}

#[tokio::test]
async fn cancellation_wins_before_result_and_retains_late_output() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = h.offer(task).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let op = Uuid::new_v4();
    h.store
        .execution_cancel(offer.attempt.attempt_id, op, "changed outcome")
        .await
        .unwrap();
    let mut next = make_offer(&h.identity);
    is_error(
        h.store.execution_offer(task, &next).await,
        ErrorCode::IdConflict,
    );
    let result = report(&offer);
    let response = h.store.execution_result(&result, NOW + 1).await.unwrap();
    assert_eq!(response.disposition, ResultDisposition::RetainedCancelled);
    assert_eq!(
        h.store.execution_result(&result, NOW + 2).await.unwrap(),
        response
    );
    next.spec.fixture_output = "replacement".into();
    next.spec = next.spec.seal();
    h.store.execution_offer(task, &next).await.unwrap();
    is_error(
        h.store
            .execution_cancel(offer.attempt.attempt_id, op, "different reason")
            .await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn accepted_result_cannot_be_retroactively_cancelled() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let result = report(&offer);
    assert_eq!(
        h.store
            .execution_result(&result, NOW + 1)
            .await
            .unwrap()
            .disposition,
        ResultDisposition::Accepted
    );
    let state = h
        .store
        .execution_cancel(offer.attempt.attempt_id, Uuid::new_v4(), "too late")
        .await
        .unwrap();
    assert!(state.cancellation_id.is_none());
    assert_eq!(
        state.disposition.unwrap().disposition,
        ResultDisposition::Accepted
    );
}

#[tokio::test]
async fn expired_execution_remains_held_until_stopped_and_reported() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = h.offer(task).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let next = make_offer(&h.identity);
    is_error(
        h.store.execution_offer(task, &next).await,
        ErrorCode::IdConflict,
    );
    let mut result = report(&offer);
    result.stop_certainty = StopCertainty::StopUncertain;
    assert_eq!(
        h.store
            .execution_result(&result, NOW + MAX_GRANT_MS)
            .await
            .unwrap()
            .disposition,
        ResultDisposition::Rejected
    );
    is_error(
        h.store.execution_offer(task, &next).await,
        ErrorCode::IdConflict,
    );
    let mut corrected = report(&offer);
    corrected.supersedes_report_id = Some(result.report_id);
    assert_eq!(
        h.store
            .execution_result(&corrected, NOW + MAX_GRANT_MS + 1)
            .await
            .unwrap()
            .disposition,
        ResultDisposition::RetainedStale
    );
    h.store.execution_offer(task, &next).await.unwrap();
}

#[tokio::test]
async fn unresolved_external_operations_prevent_replacement_even_after_stop() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = h.offer(task).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let mut result = report(&offer);
    result.pending_operation_ids.push(Uuid::new_v4());
    assert!(
        !h.store
            .execution_result(&result, NOW + 1)
            .await
            .unwrap()
            .retention_release
    );
    is_error(
        h.store
            .execution_offer(task, &make_offer(&h.identity))
            .await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn boot_replacement_requires_stop_and_old_results_never_settle_new_authority() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let next = NodeIdentity {
        node_id: h.identity.node_id,
        boot_id: Uuid::new_v4(),
    };
    let replacement = register(&next);
    is_error(
        h.store.execution_register(&replacement).await,
        ErrorCode::StaleBoot,
    );
    is_error(
        h.store.execution_replace_incarnation(&replacement).await,
        ErrorCode::CapacityUnavailable,
    );
    let observation = Observation {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        sequence: 1,
        phase: Phase::Collecting,
        stop_certainty: StopCertainty::ConfirmedStopped,
        detail: "all writers stopped".into(),
    };
    h.store.execution_observe(&observation).await.unwrap();
    h.store
        .execution_replace_incarnation(&replacement)
        .await
        .unwrap();
    assert_eq!(
        h.store
            .execution_result(&report(&offer), NOW + 1)
            .await
            .unwrap()
            .disposition,
        ResultDisposition::RetainedStale
    );
}

#[tokio::test]
async fn reconciliation_does_not_extend_grants_or_take_over_old_incarnations() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let mut request = ReconcileRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempts: vec![ReconcileAttempt {
            attempt: offer.attempt.clone(),
            previous_boot_id: h.identity.boot_id,
            phase: Phase::Running,
            launch_id: Some(Uuid::new_v4()),
            stop_certainty: StopCertainty::Running,
            report_id: None,
        }],
        cursor: None,
        max_items: 1,
    };
    let response = h
        .store
        .execution_reconcile(&request, NOW + 10_000)
        .await
        .unwrap();
    assert_eq!(
        response.attempts[0].grant.as_ref().unwrap().valid_for_ms,
        MAX_GRANT_MS - 10_000
    );
    request.identity.boot_id = Uuid::new_v4();
    let response = h
        .store
        .execution_reconcile(&request, NOW + 11_000)
        .await
        .unwrap();
    assert_eq!(
        response.attempts[0].action,
        ReconcileAction::StopAndPreserve
    );
    assert!(response.attempts[0].grant.is_none());
}

#[tokio::test]
async fn expired_unaccepted_offer_and_explicit_decline_release_task_claim() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = h.offer(task).await;
    let poll = PollRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        inventory_sequence: 1,
        available: offer.spec.resources.clone(),
        max_items: 16,
    };
    assert!(
        h.store
            .execution_poll(&poll, offer.offer_expires_at_ms)
            .await
            .unwrap()
            .offers
            .is_empty()
    );
    let next = make_offer(&h.identity);
    h.store.execution_offer(task, &next).await.unwrap();
    let decline = DeclineRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        offer_id: next.offer_id,
        reason: "capacity changed".into(),
    };
    let ack = h.store.execution_decline(&decline).await.unwrap();
    assert_eq!(h.store.execution_decline(&decline).await.unwrap(), ack);
    assert_eq!(
        h.store
            .execution_get(next.attempt.attempt_id)
            .await
            .unwrap()
            .status,
        AuthorityStatus::Declined
    );
}

#[tokio::test]
async fn observation_replay_cannot_regress_or_resurrect_stopped_processes() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let observation = Observation {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        sequence: 1,
        phase: Phase::Collecting,
        stop_certainty: StopCertainty::ConfirmedStopped,
        detail: "done".into(),
    };
    let ack = h.store.execution_observe(&observation).await.unwrap();
    assert_eq!(h.store.execution_observe(&observation).await.unwrap(), ack);
    let renew = RenewRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        grant_sequence: 1,
    };
    is_error(
        h.store.execution_renew(&renew, NOW + 1).await,
        ErrorCode::Cancelled,
    );
    let mut regression = observation.clone();
    regression.operation_id = Uuid::new_v4();
    regression.sequence = 2;
    regression.phase = Phase::Running;
    regression.stop_certainty = StopCertainty::Running;
    is_error(
        h.store.execution_observe(&regression).await,
        ErrorCode::InvalidRequest,
    );
}

#[tokio::test]
async fn execution_http_authenticates_node_binding_and_keeps_routes_separate() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let h = Harness::new().await;
    let config: aq_server::config::Config = toml::from_str(
        r#"
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
enabled=true
owner_email="owner@localhost"
"#,
    )
    .unwrap();
    let state = aq_server::AppState {
        agent_access: None,
        source_retention: None,
        identity: aq_server::identity::verifier_from_config(&config.auth, reqwest::Client::new()),
        config: std::sync::Arc::new(config),
        store: h.store.clone(),
        clock: aq_server::system_clock(),
    };
    let app = aq_server::build_worker_router(state.clone());
    let valid = Request::builder()
        .method("POST")
        .uri("/v2/worker/register")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.credential))
        .body(Body::from(
            serde_json::to_vec(&register(&h.identity)).unwrap(),
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(valid).await.unwrap().status(),
        StatusCode::OK
    );
    let mut request = register(&h.identity);
    request.identity.node_id = Uuid::new_v4();
    let req = Request::builder()
        .method("POST")
        .uri("/v2/worker/register")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.credential))
        .body(Body::from(serde_json::to_vec(&request).unwrap()))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let req = Request::builder()
        .method("POST")
        .uri("/v2/worker/register")
        .header("x-aq-request", "1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        aq_server::build_admin_router(state.clone())
            .oneshot(req)
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    #[cfg(not(feature = "recovery-test-support"))]
    for path in ["/api/execution/test-control", "/api/execution/test-bulk"] {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("x-aq-request", "1")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        assert_eq!(
            aq_server::build_admin_router(state.clone())
                .oneshot(req)
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

fn recovery(identity: &NodeIdentity) -> ReconcileRequest {
    ReconcileRequest {
        identity: identity.clone(),
        operation_id: Uuid::new_v4(),
        attempts: vec![],
        cursor: None,
        max_items: 64,
    }
}

#[tokio::test]
async fn explicit_recovery_permits_only_exact_boot_after_all_writers_stop() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    let next = NodeIdentity {
        node_id: h.identity.node_id,
        boot_id: Uuid::new_v4(),
    };
    let registration = register(&next);
    is_error(
        h.store.execution_register(&registration).await,
        ErrorCode::StaleBoot,
    );
    // Unaccepted offers cannot have launched a writer. The recovery request
    // grants a one-boot permit; registration retires the old offers.
    h.store
        .execution_reconcile(&recovery(&next), NOW)
        .await
        .unwrap();
    let wrong = NodeIdentity {
        node_id: h.identity.node_id,
        boot_id: Uuid::new_v4(),
    };
    is_error(
        h.store.execution_register(&register(&wrong)).await,
        ErrorCode::StaleBoot,
    );
    h.store.execution_register(&registration).await.unwrap();
    assert_eq!(
        h.store
            .execution_get(offer.attempt.attempt_id)
            .await
            .unwrap()
            .status,
        AuthorityStatus::Declined
    );
    is_error(
        h.store.execution_accept(&acceptance(&offer), NOW).await,
        ErrorCode::StaleBoot,
    );
}

#[tokio::test]
async fn recovery_inventory_cannot_omit_a_writer_or_race_a_new_acceptance() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    let next = NodeIdentity {
        node_id: h.identity.node_id,
        boot_id: Uuid::new_v4(),
    };
    let registration = register(&next);
    h.store
        .execution_reconcile(&recovery(&next), NOW)
        .await
        .unwrap();
    // Old boot gets a grant after reconciliation, before replacement register.
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    is_error(
        h.store.execution_register(&registration).await,
        ErrorCode::StaleBoot,
    );
    // Neither omitting the writer nor waiting past expiry establishes stop.
    h.store
        .execution_reconcile(&recovery(&next), NOW + MAX_GRANT_MS)
        .await
        .unwrap();
    is_error(
        h.store.execution_register(&registration).await,
        ErrorCode::StaleBoot,
    );
    h.store
        .execution_result(&report(&offer), NOW + MAX_GRANT_MS)
        .await
        .unwrap();
    h.store
        .execution_reconcile(&recovery(&next), NOW + MAX_GRANT_MS)
        .await
        .unwrap();
    h.store.execution_register(&registration).await.unwrap();
}

#[tokio::test]
async fn corrected_reports_preserve_original_immutable_body_and_admission() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let mut original = report(&offer);
    original.stop_certainty = StopCertainty::StopUncertain;
    original.output = "first retained evidence".into();
    let first = h.store.execution_result(&original, NOW).await.unwrap();
    let mut correction = report(&offer);
    correction.supersedes_report_id = Some(original.report_id);
    correction.output = "corrected evidence".into();
    let second = h
        .store
        .execution_result(&correction, NOW + 1)
        .await
        .unwrap();
    let reconnected = Store::connect_with(h.db.options()).await.unwrap();
    assert_eq!(
        reconnected
            .execution_report(h.identity.node_id, original.report_id)
            .await
            .unwrap(),
        (original.clone(), first.clone())
    );
    assert_eq!(
        reconnected
            .execution_report(h.identity.node_id, correction.report_id)
            .await
            .unwrap(),
        (correction, second)
    );
    assert_eq!(
        reconnected
            .execution_result(&original, NOW + 2)
            .await
            .unwrap(),
        first
    );
    let mut changed = original;
    changed.output = "tampered".into();
    is_error(
        reconnected.execution_result(&changed, NOW + 3).await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn repeated_cancellations_coalesce_one_worker_directive() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let first_id = Uuid::new_v4();
    let first = h
        .store
        .execution_cancel(offer.attempt.attempt_id, first_id, "stop")
        .await
        .unwrap();
    for _ in 0..80 {
        let state = h
            .store
            .execution_cancel(offer.attempt.attempt_id, Uuid::new_v4(), "again")
            .await
            .unwrap();
        assert_eq!(state.cancellation_id, Some(first_id));
    }
    let poll = PollRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        inventory_sequence: 1,
        available: offer.spec.resources.clone(),
        max_items: 64,
    };
    let result = h.store.execution_poll(&poll, NOW).await.unwrap();
    assert_eq!(result.directives.len(), 1);
    assert_eq!(result.directives[0].directive_id, first_id);
    assert_eq!(
        h.store
            .execution_cancel(offer.attempt.attempt_id, first_id, "stop")
            .await
            .unwrap()
            .cancellation_id,
        first.cancellation_id
    );
}

#[tokio::test]
async fn fixture_assignment_requires_owner_dev_mode_and_sealed_inputs() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let h = Harness::new().await;
    let mut config: aq_server::config::Config = toml::from_str(
        r#"
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
enabled=true
owner_email="owner@localhost"
"#,
    )
    .unwrap();
    // Use an already authenticated test identity to reach the production-mode
    // guard; JWT verification itself has its own foundation suite.
    let identity = aq_server::identity::verifier_from_config(&config.auth, reqwest::Client::new());
    let clock: aq_server::Clock =
        std::sync::Arc::new(|| chrono::DateTime::from_timestamp_millis(NOW as i64).unwrap());
    let app = |config| {
        aq_server::build_admin_router(aq_server::AppState {
            agent_access: None,
            source_retention: None,
            config: std::sync::Arc::new(config),
            identity: identity.clone(),
            store: h.store.clone(),
            clock: clock.clone(),
        })
    };
    let offer = make_offer(&h.identity);
    let request = |offer: &Offer, email: &str| {
        Request::builder()
            .method("POST")
            .uri("/api/execution/fixtures")
            .header("content-type", "application/json")
            .header("x-aq-request", "1")
            .header("x-aq-dev-email", email)
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({"task_id":Uuid::new_v4(), "offer":offer}))
                    .unwrap(),
            ))
            .unwrap()
    };
    // An unknown development visitor is not an owner.
    assert_eq!(
        app(config.clone())
            .oneshot(request(&offer, "visitor@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut changed = offer.clone();
    changed.spec.fixture_output = "unsealed edit".into();
    assert_eq!(
        app(config.clone())
            .oneshot(request(&changed, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app(config.clone())
            .oneshot(request(&offer, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    config.auth = aq_server::config::AuthConfig::CloudflareAccess {
        team_domain: "https://example.cloudflareaccess.com".into(),
        audience: "fixture".into(),
    };
    assert_eq!(
        app(config)
            .oneshot(request(&make_offer(&h.identity), "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn cancelled_unaccepted_offer_never_queues_an_unknown_process_directive() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let old = h.offer(task).await;
    h.store
        .execution_cancel(old.attempt.attempt_id, Uuid::new_v4(), "not needed")
        .await
        .unwrap();
    let next = h.offer(task).await;
    let poll = PollRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        inventory_sequence: 1,
        available: next.spec.resources.clone(),
        max_items: 64,
    };
    let response = h.store.execution_poll(&poll, NOW).await.unwrap();
    assert!(response.directives.is_empty());
    assert_eq!(response.offers, vec![next]);
    is_error(
        h.store.execution_accept(&acceptance(&old), NOW).await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn conflicting_task_claims_on_different_nodes_share_one_central_fence() {
    let h = Harness::new().await;
    let now = Utc::now();
    let owner = h
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let credential = new_node_credential();
    h.store
        .create_enrollment("second-node", owner.id, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let node = h
        .store
        .redeem_enrollment(
            "second-node",
            NewNode {
                name: "second".into(),
                credential_hash: verifier(&credential),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let other = NodeIdentity {
        node_id: node.id,
        boot_id: Uuid::new_v4(),
    };
    h.store.execution_register(&register(&other)).await.unwrap();
    let task = Uuid::new_v4();
    let a = make_offer(&h.identity);
    let b = make_offer(&other);
    let (a, b) = tokio::join!(
        h.store.execution_offer(task, &a),
        h.store.execution_offer(task, &b)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let losing = if a.is_err() { a } else { b };
    is_error(losing, ErrorCode::IdConflict);
}

#[cfg(feature = "recovery-test-support")]
fn recovery_state(h: &Harness) -> aq_server::AppState {
    let config: aq_server::config::Config = toml::from_str(
        r#"
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
enabled=true
owner_email="owner@localhost"
"#,
    )
    .unwrap();
    aq_server::AppState {
        agent_access: None,
        source_retention: None,
        identity: aq_server::identity::verifier_from_config(&config.auth, reqwest::Client::new()),
        config: std::sync::Arc::new(config),
        store: h.store.clone(),
        clock: std::sync::Arc::new(|| chrono::DateTime::from_timestamp_millis(NOW as i64).unwrap()),
    }
}

#[cfg(feature = "recovery-test-support")]
async fn recovery_call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: serde_json::Value,
    credential: Option<&str>,
) -> (axum::http::StatusCode, serde_json::Value) {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = Request::builder()
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aq-request", "1");
    if let Some(c) = credential {
        request = request.header("authorization", format!("Bearer {c}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        },
    )
}

#[cfg(feature = "recovery-test-support")]
#[tokio::test]
async fn control_auth_and_operations_survive_full_general_pool_and_bulk_saturation() {
    use axum::http::StatusCode;
    use serde_json::{json, to_value};
    use std::time::Duration;
    let mut h = Harness::new().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(h.db.options())
        .await
        .unwrap();
    h.store = Store::from_pool(pool.clone()).await.unwrap();
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let state = recovery_state(&h);
    let admin = aq_server::build_admin_router(state.clone());
    let worker = aq_server::build_worker_router(state);
    let set = recovery_call(
        &admin,
        "POST",
        "/api/execution/test-control",
        json!({"bulk_blocked":true}),
        None,
    )
    .await;
    assert_eq!(set.0, StatusCode::OK);
    let mut bulk = Vec::new();
    for _ in 0..2 {
        let app = admin.clone();
        bulk.push(tokio::spawn(async move {
            recovery_call(&app, "POST", "/api/execution/test-bulk", json!({}), None).await
        }));
    }
    // Wait for the observable barrier, not a guessed sleep interval.
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let (status, value) = recovery_call(
                &admin,
                "GET",
                "/api/execution/test-control",
                json!({}),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            if value["bulk_active"] == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        pool.try_acquire().is_none(),
        "both general connections must actually be held"
    );
    assert_eq!(
        recovery_call(&admin, "GET", "/readyz", json!({}), None)
            .await
            .0,
        StatusCode::OK
    );
    let (status, _) =
        recovery_call(&admin, "POST", "/api/execution/test-bulk", json!({}), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let renew = RenewRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        grant_sequence: 1,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        recovery_call(
            &worker,
            "POST",
            "/v2/worker/renew",
            to_value(&renew).unwrap(),
            Some(&h.credential),
        ),
    )
    .await
    .unwrap();
    assert_eq!(result.0, StatusCode::OK);
    assert_eq!(result.1["grant"]["grant_sequence"], 2);
    let poll = PollRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        inventory_sequence: 1,
        available: offer.spec.resources.clone(),
        max_items: 64,
    };
    assert_eq!(
        recovery_call(
            &worker,
            "POST",
            "/v2/worker/poll",
            to_value(poll).unwrap(),
            Some(&h.credential)
        )
        .await
        .0,
        StatusCode::OK
    );
    let cancel = format!(
        "/api/execution/attempts/{}/cancel",
        offer.attempt.attempt_id
    );
    assert_eq!(
        recovery_call(
            &admin,
            "POST",
            &cancel,
            json!({"operation_id":Uuid::new_v4(),"reason":"control survives load"}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        recovery_call(
            &admin,
            "POST",
            "/api/execution/test-control",
            json!({"bulk_blocked":false}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    for task in bulk {
        assert_eq!(task.await.unwrap().0, StatusCode::OK);
    }
    let (_, value) = recovery_call(
        &admin,
        "GET",
        "/api/execution/test-control",
        json!({}),
        None,
    )
    .await;
    assert_eq!(value["bulk_active"], 0);
}

#[cfg(feature = "recovery-test-support")]
#[tokio::test]
async fn recovery_clock_controls_do_not_extend_prior_receipts_and_refuse_production() {
    use axum::http::StatusCode;
    use serde_json::{json, to_value};
    let h = Harness::new().await;
    let state = recovery_state(&h);
    let admin = aq_server::build_admin_router(state.clone());
    let worker = aq_server::build_worker_router(state.clone());
    assert_eq!(
        recovery_call(
            &admin,
            "POST",
            "/api/execution/test-control",
            json!({"grant_ttl_ms":1000}),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    let offer = h.offer(Uuid::new_v4()).await;
    let request = acceptance(&offer);
    let (status, result) = recovery_call(
        &worker,
        "POST",
        "/v2/worker/accept",
        to_value(&request).unwrap(),
        Some(&h.credential),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["grant"]["valid_for_ms"], 1000);
    recovery_call(
        &admin,
        "POST",
        "/api/execution/test-control",
        json!({"advance_ms":1000,"grant_ttl_ms":120000}),
        None,
    )
    .await;
    let (status, result) = recovery_call(
        &worker,
        "POST",
        "/v2/worker/accept",
        to_value(request).unwrap(),
        Some(&h.credential),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(result["code"], "grant_expired");
    let mut production = state;
    let mut config = production.config.as_ref().clone();
    config.auth = aq_server::config::AuthConfig::CloudflareAccess {
        team_domain: "https://example.cloudflareaccess.com".into(),
        audience: "test".into(),
    };
    production.config = std::sync::Arc::new(config);
    // Authenticated test owner reaches the handler; production-mode guard still refuses.
    let app = aq_server::build_admin_router(production);
    assert_eq!(
        recovery_call(
            &app,
            "POST",
            "/api/execution/test-control",
            json!({"advance_ms":1}),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        recovery_call(&app, "POST", "/api/execution/test-bulk", json!({}), None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn released_report_correction_cannot_reacquire_a_successors_task_claim() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = h.offer(task).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let original = report(&offer);
    h.store.execution_result(&original, NOW).await.unwrap();
    let next = h.offer(task).await;
    h.store
        .execution_accept(&acceptance(&next), NOW)
        .await
        .unwrap();
    let mut correction = report(&offer);
    correction.supersedes_report_id = Some(original.report_id);
    correction.stop_certainty = StopCertainty::StopUncertain;
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    correction.stop_certainty = StopCertainty::ConfirmedStopped;
    correction.pending_operation_ids.push(Uuid::new_v4());
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    assert_eq!(
        h.store
            .execution_report(h.identity.node_id, original.report_id)
            .await
            .unwrap()
            .0,
        original
    );
    assert_eq!(
        h.store
            .execution_get(next.attempt.attempt_id)
            .await
            .unwrap()
            .status,
        AuthorityStatus::Granted
    );
}

#[cfg(feature = "recovery-test-support")]
#[tokio::test]
async fn maintenance_preserves_authorized_cancellation_but_blocks_new_fixture_work() {
    use axum::http::StatusCode;
    use serde_json::json;
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    let mut state = recovery_state(&h);
    let spool = tempfile::tempdir().unwrap();
    std::fs::create_dir(spool.path().join("status")).unwrap();
    std::fs::write(spool.path().join("status/maintenance"), "").unwrap();
    let mut config = state.config.as_ref().clone();
    config.updates = Some(aq_server::config::UpdatesConfig {
        spool_dir: spool.path().to_path_buf(),
    });
    state.config = std::sync::Arc::new(config);
    let admin = aq_server::build_admin_router(state);
    let path = format!(
        "/api/execution/attempts/{}/cancel",
        offer.attempt.attempt_id
    );
    let request = json!({"operation_id":Uuid::new_v4(),"reason":"drain during update"});
    assert_eq!(
        recovery_call(&admin, "POST", &path, request, None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        recovery_call(&admin, "POST", "/api/execution/fixtures", json!({}), None)
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn no_start_decline_can_reconcile_terminal_unaccepted_old_boot_offers() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    let next = NodeIdentity {
        node_id: h.identity.node_id,
        boot_id: Uuid::new_v4(),
    };
    h.store
        .execution_reconcile(&recovery(&next), NOW)
        .await
        .unwrap();
    h.store.execution_register(&register(&next)).await.unwrap();
    let decline = DeclineRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        offer_id: offer.offer_id,
        reason: "recovered before any launch".into(),
    };
    let ack = h.store.execution_decline(&decline).await.unwrap();
    assert_eq!(h.store.execution_decline(&decline).await.unwrap(), ack);
    let mut wrong = decline.clone();
    wrong.identity = next;
    wrong.operation_id = Uuid::new_v4();
    is_error(
        h.store.execution_decline(&wrong).await,
        ErrorCode::StaleBoot,
    );
    assert_eq!(
        h.store
            .execution_get(offer.attempt.attempt_id)
            .await
            .unwrap()
            .status,
        AuthorityStatus::Declined
    );
    // No-grant results still cannot fabricate accepted execution evidence.
    let mut never_started = report(&offer);
    never_started.stop_certainty = StopCertainty::NotStarted;
    is_error(
        h.store.execution_result(&never_started, NOW).await,
        ErrorCode::InvalidRequest,
    );
}

#[tokio::test]
async fn decline_racing_acceptance_either_fences_launch_or_requires_result() {
    let h = Harness::new().await;
    let offer = h.offer(Uuid::new_v4()).await;
    let decline = DeclineRequest {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        offer_id: offer.offer_id,
        reason: "local reservation expired before start".into(),
    };
    let acceptance = acceptance(&offer);
    let (accepted, declined) = tokio::join!(
        h.store.execution_accept(&acceptance, NOW),
        h.store.execution_decline(&decline)
    );
    assert_ne!(accepted.is_ok(), declined.is_ok());
    if accepted.is_ok() {
        is_error(declined, ErrorCode::IdConflict);
        is_error(
            h.store.execution_decline(&decline).await,
            ErrorCode::IdConflict,
        );
        let mut stopped = report(&offer);
        stopped.stop_certainty = StopCertainty::NotStarted;
        stopped.outcome = ProcessOutcome::Expired;
        stopped.claimed_success = false;
        assert!(
            h.store
                .execution_result(&stopped, NOW)
                .await
                .unwrap()
                .retention_release
        );
    } else {
        is_error(accepted, ErrorCode::IdConflict);
        assert_eq!(
            h.store
                .execution_get(offer.attempt.attempt_id)
                .await
                .unwrap()
                .status,
            AuthorityStatus::Declined
        );
    }
}

#[cfg(feature = "recovery-test-support")]
#[tokio::test]
async fn authority_decisions_use_time_after_database_lock_wait() {
    use axum::http::StatusCode;
    use serde_json::{json, to_value};
    for operation in ["accept", "renew", "result"] {
        let h = Harness::new().await;
        let state = recovery_state(&h);
        let admin = aq_server::build_admin_router(state.clone());
        let worker = aq_server::build_worker_router(state);
        assert_eq!(
            recovery_call(
                &admin,
                "POST",
                "/api/execution/test-control",
                json!({"grant_ttl_ms":1000}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        let offer = h.offer(Uuid::new_v4()).await;
        if operation != "accept" {
            h.store
                .execution_accept(&acceptance(&offer), NOW)
                .await
                .unwrap();
        }
        let before = h
            .store
            .execution_get(offer.attempt.attempt_id)
            .await
            .unwrap();
        let body = match operation {
            "accept" => to_value(acceptance(&offer)).unwrap(),
            "renew" => to_value(RenewRequest {
                identity: h.identity.clone(),
                operation_id: Uuid::new_v4(),
                attempt: offer.attempt.clone(),
                grant_sequence: 1,
            })
            .unwrap(),
            _ => to_value(report(&offer)).unwrap(),
        };
        let pool = h.db.pool().await;
        let mut blocker = pool.begin().await.unwrap();
        let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(h.identity.node_id)
            .fetch_one(&mut *blocker)
            .await
            .unwrap();
        let credential = h.credential.clone();
        let path = format!("/v2/worker/{operation}");
        let pending = tokio::spawn(async move {
            recovery_call(&worker, "POST", &path, body, Some(&credential)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1 = ANY(pg_blocking_pids(pid)))")
                    .bind(blocker_pid).fetch_one(&pool).await.unwrap();
                if blocked { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("request must observably wait on held node lock");
        let elapsed = if operation == "accept" { 30_001 } else { 1_001 };
        assert_eq!(
            recovery_call(
                &admin,
                "POST",
                "/api/execution/test-control",
                json!({"advance_ms":elapsed}),
                None
            )
            .await
            .0,
            StatusCode::OK
        );
        blocker.commit().await.unwrap();
        let (status, response) = pending.await.unwrap();
        if operation == "result" {
            assert_eq!(status, StatusCode::OK, "{response}");
            assert_eq!(response["disposition"], "retained_stale");
        } else {
            assert_eq!(status, StatusCode::CONFLICT, "{response}");
            assert_eq!(
                response["code"],
                if operation == "accept" {
                    "offer_expired"
                } else {
                    "grant_expired"
                }
            );
            let after = h
                .store
                .execution_get(offer.attempt.attempt_id)
                .await
                .unwrap();
            assert_eq!(after.grant, before.grant);
            assert_eq!(after.grant_expires_at_ms, before.grant_expires_at_ms);
        }
    }
}

async fn source_offer(h: &Harness, task: Uuid) -> Offer {
    let mut registration = register(&h.identity);
    registration.capabilities.push(SOURCE_CAPABILITY.into());
    h.store.execution_register(&registration).await.unwrap();
    let mut offer = make_offer(&h.identity);
    offer
        .spec
        .required_capabilities
        .push(SOURCE_CAPABILITY.into());
    offer.spec.bindings = Some(ExecutionBindings {
        source: Some(SourceBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
            logical_identity: "fixture/project".into(),
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "1".repeat(40),
            },
        }),
        graph: None,
    });
    offer.spec = offer.spec.seal();
    h.store.execution_offer(task, &offer).await.unwrap();
    h.store
        .execution_accept(&acceptance(&offer), NOW)
        .await
        .unwrap();
    offer
}

async fn source_receipt(h: &Harness, offer: &Offer, retained: bool) -> ArtifactRef {
    let source = offer
        .spec
        .bindings
        .as_ref()
        .unwrap()
        .source
        .as_ref()
        .unwrap()
        .clone();
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
        bundle_digest: None,
        manifest_bytes: 64,
        bundle_bytes: 0,
    };
    let receipt = CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: manifest.transfer_id,
        repository_id: source.repository_id,
        binding_generation: 1,
        source: manifest.checkpoint.clone(),
        retention_handle: manifest.artifact_id,
        manifest_digest: "b".repeat(64),
    };
    let pool = h.db.pool().await;
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt) VALUES($1,$2,$3,$4,$5,$6,$7::jsonb,$8,$9::jsonb)")
        .bind(manifest.transfer_id).bind(offer.attempt.attempt_id).bind(h.identity.node_id)
        .bind(source.repository_id).bind(manifest.artifact_id).bind(Uuid::new_v4())
        .bind(serde_json::to_string(&manifest).unwrap())
        .bind(if retained {"retained"} else {"import_pending"})
        .bind(serde_json::to_string(&receipt).unwrap()).execute(&pool).await.unwrap();
    ArtifactRef {
        artifact_id: manifest.artifact_id,
        digest: manifest.manifest_digest,
        bytes: 64,
        central_receipt: Some(receipt.receipt_id.to_string()),
    }
}

#[tokio::test]
async fn source_results_require_owned_retained_receipts_and_cannot_erase_launch_evidence() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = source_offer(&h, task).await;
    let initial = report(&offer);
    let refused = h.store.execution_result(&initial, NOW).await.unwrap();
    assert_eq!(refused.disposition, ResultDisposition::Rejected);
    assert!(!refused.retention_release);
    is_error(
        h.store
            .execution_offer(task, &make_offer(&h.identity))
            .await,
        ErrorCode::IdConflict,
    );
    let mut downgrade = report(&offer);
    downgrade.supersedes_report_id = Some(initial.report_id);
    downgrade.stop_certainty = StopCertainty::NotStarted;
    is_error(
        h.store.execution_result(&downgrade, NOW).await,
        ErrorCode::InvalidRequest,
    );
    let artifact = source_receipt(&h, &offer, true).await;
    let mut correction = report(&offer);
    correction.supersedes_report_id = Some(initial.report_id);
    correction.artifacts.push(artifact.clone());
    correction.artifacts[0].central_receipt = Some(Uuid::new_v4().to_string());
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    correction.artifacts[0] = artifact.clone();
    correction.artifacts[0].bytes += 1;
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    correction.artifacts[0] = artifact.clone();
    correction.artifacts[0].digest = "c".repeat(64);
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    correction.artifacts = vec![artifact.clone(), artifact.clone()];
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    correction.artifacts = vec![artifact];
    let accepted = h.store.execution_result(&correction, NOW).await.unwrap();
    assert!(accepted.retention_release);
    assert_eq!(
        h.store
            .execution_result(&correction, NOW + 100_000)
            .await
            .unwrap(),
        accepted
    );
    assert_eq!(
        h.store.execution_result(&initial, NOW).await.unwrap(),
        refused
    );
    h.store
        .execution_offer(task, &make_offer(&h.identity))
        .await
        .unwrap();
    // Another attempt cannot borrow this retained artifact.
    let other = source_offer(&h, Uuid::new_v4()).await;
    let mut borrowed = report(&other);
    borrowed.artifacts = correction.artifacts;
    is_error(
        h.store.execution_result(&borrowed, NOW).await,
        ErrorCode::InvalidRequest,
    );
}

#[tokio::test]
async fn source_pending_operations_hold_claim_even_if_report_omits_them() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = source_offer(&h, task).await;
    let retained = source_receipt(&h, &offer, true).await;
    let _unknown = source_receipt(&h, &offer, false).await;
    let mut r = report(&offer);
    r.artifacts.push(retained);
    let response = h.store.execution_result(&r, NOW).await.unwrap();
    assert!(!response.retention_release);
    is_error(
        h.store
            .execution_offer(task, &make_offer(&h.identity))
            .await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn source_nonstart_exception_requires_no_prior_execution_evidence() {
    let h = Harness::new().await;
    let offer = source_offer(&h, Uuid::new_v4()).await;
    let mut r = report(&offer);
    r.stop_certainty = StopCertainty::NotStarted;
    r.outcome = ProcessOutcome::Expired;
    r.claimed_success = false;
    assert!(
        h.store
            .execution_result(&r, NOW)
            .await
            .unwrap()
            .retention_release
    );
    let offer = source_offer(&h, Uuid::new_v4()).await;
    h.store
        .execution_observe(&Observation {
            identity: h.identity.clone(),
            operation_id: Uuid::new_v4(),
            attempt: offer.attempt.clone(),
            sequence: 1,
            phase: Phase::Running,
            stop_certainty: StopCertainty::Running,
            detail: "started".into(),
        })
        .await
        .unwrap();
    let mut r = report(&offer);
    r.stop_certainty = StopCertainty::NotStarted;
    r.outcome = ProcessOutcome::Expired;
    r.claimed_success = false;
    is_error(
        h.store.execution_result(&r, NOW).await,
        ErrorCode::InvalidRequest,
    );
}

#[tokio::test]
async fn launch_uncertain_evidence_cannot_be_downgraded_to_nonstart_release() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = source_offer(&h, task).await;
    let observation = Observation {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        sequence: 1,
        phase: Phase::LaunchUncertain,
        stop_certainty: StopCertainty::NotStarted,
        detail: "launch acknowledgement missing despite caller nonstart assertion".into(),
    };
    h.store.execution_observe(&observation).await.unwrap();
    let mut downgrade = report(&offer);
    downgrade.stop_certainty = StopCertainty::NotStarted;
    downgrade.claimed_success = false;
    is_error(
        h.store.execution_result(&downgrade, NOW).await,
        ErrorCode::InvalidRequest,
    );
    let pool = h.db.pool().await;
    let (holds,marked):(bool,bool)=sqlx::query_as("SELECT holds_task,(state->>'may_have_started')::boolean FROM execution_attempts WHERE attempt_id=$1").bind(offer.attempt.attempt_id).fetch_one(&pool).await.unwrap();
    assert!(holds && marked);
    // A legacy projection lacking the marker still retains its latest uncertain
    // observation, which must be sufficient to preserve the same hold.
    sqlx::query("UPDATE execution_attempts SET state=state-'may_have_started' WHERE attempt_id=$1")
        .bind(offer.attempt.attempt_id)
        .execute(&pool)
        .await
        .unwrap();
    is_error(
        h.store.execution_result(&downgrade, NOW).await,
        ErrorCode::InvalidRequest,
    );
    is_error(
        h.store
            .execution_offer(task, &make_offer(&h.identity))
            .await,
        ErrorCode::IdConflict,
    );
}

#[tokio::test]
async fn legacy_report_history_prevents_nonstart_release_after_projection_downgrade() {
    let h = Harness::new().await;
    let task = Uuid::new_v4();
    let offer = source_offer(&h, task).await;
    let original = report(&offer);
    assert!(
        !h.store
            .execution_result(&original, NOW)
            .await
            .unwrap()
            .retention_release
    );
    // Reproduce a pre-marker projection whose latest correction erased the
    // earlier stopped evidence; the immutable execution_reports row survives.
    let mut latest = report(&offer);
    latest.stop_certainty = StopCertainty::NotStarted;
    latest.claimed_success = false;
    latest.supersedes_report_id = Some(original.report_id);
    let pool = h.db.pool().await;
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state-'may_have_started','{report}',$2) WHERE attempt_id=$1").bind(offer.attempt.attempt_id).bind(serde_json::to_value(&latest).unwrap()).execute(&pool).await.unwrap();
    let mut correction = report(&offer);
    correction.stop_certainty = StopCertainty::NotStarted;
    correction.claimed_success = false;
    correction.supersedes_report_id = Some(latest.report_id);
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    let observation = Observation {
        identity: h.identity.clone(),
        operation_id: Uuid::new_v4(),
        attempt: offer.attempt.clone(),
        sequence: 1,
        phase: Phase::Reporting,
        stop_certainty: StopCertainty::NotStarted,
        detail: "legacy reconciliation cannot erase historical start evidence".into(),
    };
    h.store.execution_observe(&observation).await.unwrap();
    let marked: bool = sqlx::query_scalar(
        "SELECT (state->>'may_have_started')::boolean FROM execution_attempts WHERE attempt_id=$1",
    )
    .bind(offer.attempt.attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        marked,
        "next successful save upgrades the legacy projection"
    );
    is_error(
        h.store.execution_result(&correction, NOW).await,
        ErrorCode::InvalidRequest,
    );
    is_error(
        h.store
            .execution_offer(task, &make_offer(&h.identity))
            .await,
        ErrorCode::IdConflict,
    );
}
