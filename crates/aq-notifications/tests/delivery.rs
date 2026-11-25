use aq_lifecycle_api::Scope;
use aq_notifications::*;
use uuid::Uuid;
fn delivery() -> Delivery {
    let scope = Scope {
        project_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
    };
    let recipient = Uuid::new_v4();
    let message = Message {
        message_id: Uuid::new_v4(),
        revision: 1,
        scope: scope.clone(),
        recipient_user_id: recipient,
        summary: "Review required @everyone".into(),
        created_at_ms: 1000,
        expires_at_ms: 100_000,
    };
    let route = Route {
        route_id: Uuid::new_v4(),
        revision: 1,
        scope,
        recipient_user_id: recipient,
        endpoint_key: "test-discord".into(),
        endpoint_revision: 1,
        enabled: true,
        retry: RetryPolicy {
            max_attempts: 3,
            max_elapsed_ms: 30_000,
            initial_delay_ms: 250,
            max_delay_ms: 2000,
            allow_duplicate_risk: false,
        },
    };
    let payload = Payload::for_message(&message, "https://aq.example").unwrap();
    Delivery::queued(Uuid::new_v4(), message, route, payload).unwrap()
}
fn begin(d: &Delivery, id: Uuid, now: u64) -> Delivery {
    d.begin(
        DispatchAuthority {
            route: &d.route,
            recipient_can_view: true,
        },
        id,
        now,
    )
    .unwrap()
}
#[test]
fn lost_dispatch_response_never_automatically_requeues_or_acknowledges_a_human() {
    let d = delivery();
    let id = Uuid::new_v4();
    let committed = begin(&d, id, 1001);
    let restored: Delivery =
        serde_json::from_slice(&serde_json::to_vec(&committed).unwrap()).unwrap();
    let recovered = restored.recover(1002).unwrap();
    assert!(matches!(recovered.status,Status::DeliveryUnknown{attempt_id,..} if attempt_id==id));
    assert!(
        recovered
            .begin(
                DispatchAuthority {
                    route: &d.route,
                    recipient_can_view: true
                },
                Uuid::new_v4(),
                1003
            )
            .is_err()
    );
    assert!(recovered.retry_unknown(1003).is_err());
    let accepted = committed
        .observe(
            AttemptOutcome::new(
                id,
                TransportOutcome::Accepted {
                    message_id: "12345".into(),
                },
            ),
            1002,
            0,
        )
        .unwrap();
    assert!(matches!(
        accepted.status,
        Status::AcceptedByTransport { .. }
    ));
    assert_eq!(accepted, accepted.recover(1003).unwrap());
    assert!(accepted.retry_unknown(1003).is_err());
    assert!(
        !serde_json::to_string(&accepted)
            .unwrap()
            .contains("acknowledged")
    );
}
#[test]
fn explicit_duplicate_risk_retry_preserves_identity_and_cumulative_budget() {
    let mut original = delivery();
    original.route.retry.allow_duplicate_risk = true;
    let d = Delivery::queued(
        original.delivery_id,
        original.message,
        original.route,
        original.payload,
    )
    .unwrap();
    let a = begin(&d, Uuid::new_v4(), 1001).recover(1002).unwrap();
    let retry = a.retry_unknown(1003).unwrap();
    assert_eq!(retry.delivery_id, d.delivery_id);
    assert_eq!(retry.immutable_digest, d.immutable_digest);
    assert_eq!(retry.duplicate_risk_retries, 1);
    assert!(retry.retry_unknown(1003).is_err());
    let b = begin(&retry, Uuid::new_v4(), 1003).recover(1004).unwrap();
    let c = begin(&b.retry_unknown(1005).unwrap(), Uuid::new_v4(), 1005)
        .recover(1006)
        .unwrap();
    assert_eq!(c.attempts, 3);
    assert_eq!(c.first_attempt_ms, Some(1001));
    assert_eq!(c.retry_unknown(1007), Err(Error::Exhausted));
}
#[test]
fn current_route_visibility_and_time_are_required_at_dispatch() {
    let d = delivery();
    let mut route = d.route.clone();
    route.revision += 1;
    assert_eq!(
        d.begin(
            DispatchAuthority {
                route: &route,
                recipient_can_view: true
            },
            Uuid::new_v4(),
            1001
        ),
        Err(Error::ScopeChanged)
    );
    route = d.route.clone();
    route.enabled = false;
    assert!(
        d.begin(
            DispatchAuthority {
                route: &route,
                recipient_can_view: true
            },
            Uuid::new_v4(),
            1001
        )
        .is_err()
    );
    assert!(
        d.begin(
            DispatchAuthority {
                route: &d.route,
                recipient_can_view: false
            },
            Uuid::new_v4(),
            1001
        )
        .is_err()
    );
    assert!(
        d.begin(
            DispatchAuthority {
                route: &d.route,
                recipient_can_view: true
            },
            Uuid::new_v4(),
            100_000
        )
        .is_err()
    );
    assert!(
        d.begin(
            DispatchAuthority {
                route: &d.route,
                recipient_can_view: true
            },
            Uuid::new_v4(),
            999
        )
        .is_err()
    );
    let mut changed = d.clone();
    changed.payload.content.push('!');
    assert!(changed.validate().is_err());
    changed = d;
    changed.delivery_id = Uuid::new_v4();
    assert!(changed.validate().is_err());
}
#[test]
fn provider_delays_are_never_shortened_and_expired_budgets_refuse() {
    let d = delivery();
    let id = Uuid::new_v4();
    let sent = begin(&d, id, 1001);
    let result = sent
        .observe(
            AttemptOutcome::new(
                id,
                TransportOutcome::RateLimited {
                    retry_after_ms: 10_000,
                    global: false,
                },
            ),
            1002,
            0,
        )
        .unwrap();
    assert_eq!(
        result.status,
        Status::RetryScheduled {
            next_attempt_ms: 11_002
        }
    );
    assert!(
        result
            .begin(
                DispatchAuthority {
                    route: &d.route,
                    recipient_can_view: true
                },
                Uuid::new_v4(),
                11_001
            )
            .is_err()
    );
    let restored: Delivery = serde_json::from_slice(&serde_json::to_vec(&result).unwrap()).unwrap();
    assert_eq!(begin(&restored, Uuid::new_v4(), 11_002).attempts, 2);
    let over = sent
        .observe(
            AttemptOutcome::new(
                id,
                TransportOutcome::RateLimited {
                    retry_after_ms: 86_400_001,
                    global: true,
                },
            ),
            1002,
            0,
        )
        .unwrap();
    assert_eq!(
        over.status,
        Status::PermanentlyFailed {
            reason: FailureReason::ProviderDelayTooLong
        }
    );
    assert!(
        sent.observe(
            AttemptOutcome::new(Uuid::new_v4(), TransportOutcome::Unknown),
            1002,
            0
        )
        .is_err()
    );
}
#[test]
fn payload_disables_mentions_and_embeds_and_refuses_unbounded_or_foreign_scope() {
    let d = delivery();
    let value = d.payload.wire().unwrap();
    assert_eq!(value["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(value["flags"], 4);
    let mut huge = d.message.clone();
    huge.summary = "😀".repeat(801);
    assert!(huge.validate().is_err());
    let mut route = d.route.clone();
    route.recipient_user_id = Uuid::new_v4();
    assert_eq!(
        Delivery::queued(Uuid::new_v4(), d.message.clone(), route, d.payload),
        Err(Error::ScopeChanged)
    );
    for origin in [
        "http://aq.example",
        "https://x:secret@aq.example",
        "https://aq.example/path",
        "https://aq.example?secret=x",
    ] {
        assert!(Payload::for_message(&d.message, origin).is_err());
    }
}
