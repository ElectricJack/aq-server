#[path = "../examples/support/mod.rs"]
mod support;
use aq_lifecycle_api::*;
use support::*;
#[test]
fn fixtures_and_schema_drift() {
    let f = fixtures();
    assert_eq!(
        f,
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/fixtures.json"))
            .unwrap()
    );
    assert_eq!(
        request_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/request.schema.json"))
            .unwrap()
    );
    assert_eq!(
        response_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../artifacts/response.schema.json"
        ))
        .unwrap()
    );
}
#[test]
fn exact_replay_age_and_changed_payload() {
    let r = registration();
    let mut p = peer(&r);
    let q = Request::seal(id(30), Command::RegisterWait(r.clone())).unwrap();
    let first = p.receive(&q, 10).unwrap();
    assert_eq!(first, p.receive(&q, 100).unwrap());
    let fresh = Request::seal(id(31), Command::RegisterWait(r.clone())).unwrap();
    let again = p.receive(&fresh, 500).unwrap();
    assert_eq!(first.body, again.body);
    let mut changed = r;
    changed.preference = WaitPreference::Hibernate;
    let conflict = Request::seal(id(30), Command::RegisterWait(changed)).unwrap();
    assert!(p.receive(&conflict, 20).is_err());
}
#[test]
fn refuses_self_wait_and_duplicate_predicates() {
    let mut r = registration();
    r.predicate = Predicate::Single {
        member: Member {
            member_id: id(20),
            condition: Condition::Task {
                task_id: r.binding.task_id,
                revision: Text::new("1").unwrap(),
                requirement: TaskRequirement::Successful,
            },
        },
    };
    r.predicate_digest = canonical_digest(&r.predicate).unwrap();
    assert!(r.validate().is_err());
    let m = Member {
        member_id: id(20),
        condition: Condition::Timer { due_at_ms: 100 },
    };
    assert!(
        Predicate::All {
            members: vec![m.clone(), m]
        }
        .validate()
        .is_err()
    );
}
#[test]
fn capability_and_node_refusals() {
    let r = registration();
    let q = Request::seal(id(30), Command::RegisterWait(r.clone())).unwrap();
    let context = CallerContext {
        role: ActorRole::NodeRecovery,
        scope: r.scope.clone(),
        binding: Some(r.binding.clone()),
        current_operation_id: None,
        current_run_id: None,
    };
    let mut p = ConformanceApi::new(context, vec![q.body.required_capability.clone()]).unwrap();
    assert!(p.receive(&q, 10).is_err());
    assert!(negotiate_api(&q.body.required_capability, &[]).is_err());
}
fn verdicts(r: &WaitRegistration, state: MemberState) -> Vec<MemberOutcome> {
    r.predicate
        .members()
        .iter()
        .map(|m| MemberOutcome {
            member_id: m.member_id,
            state: state.clone(),
            evidence: Some(SourceEvidence {
                owner: m.condition.owner(),
                condition_digest: canonical_digest(&m.condition).unwrap(),
                revision: Text::new("committed-1").unwrap(),
                result: artifact_ref("source-verdict"),
            }),
        })
        .collect()
}
fn registered_peer() -> (WaitRegistration, ConformanceApi) {
    let r = registration();
    let mut p = peer(&r);
    let q = Request::seal(id(30), Command::RegisterWait(r.clone())).unwrap();
    p.receive(&q, 10).unwrap();
    (r, p)
}
#[test]
fn any_uses_declared_order_and_immutable_outcome() {
    let (r, mut p) = registered_peer();
    let outcome = p
        .resolve_from_fixture(r.wait_id, verdicts(&r, MemberState::Satisfied), id(40), 20)
        .unwrap();
    assert_eq!(outcome.selected_member_id, Some(id(20)));
    assert_eq!(outcome.kind, WaitOutcomeKind::Satisfied);
    assert!(
        p.resolve_from_fixture(r.wait_id, verdicts(&r, MemberState::Satisfied), id(41), 21)
            .is_err()
    );
    assert!(!p.bootstrap_acknowledged());
}
#[test]
fn rejects_wrong_source_owner() {
    let (r, mut p) = registered_peer();
    let mut v = verdicts(&r, MemberState::Satisfied);
    v[0].evidence.as_mut().unwrap().owner = SourceOwner::Mailbox;
    assert!(p.resolve_from_fixture(r.wait_id, v, id(40), 20).is_err());
}
#[test]
fn pending_any_cannot_become_failure() {
    let (r, mut p) = registered_peer();
    let mut v = verdicts(&r, MemberState::SourceFailed);
    v[1].state = MemberState::Pending;
    v[1].evidence = None;
    assert!(p.resolve_from_fixture(r.wait_id, v, id(40), 20).is_err());
}
#[test]
fn strict_decoding_and_envelope_bounds() {
    let r = registration();
    let q = Request::seal(id(30), Command::RegisterWait(r.clone())).unwrap();
    let bytes = serde_json::to_vec(&q).unwrap();
    assert_eq!(q, decode_request(&bytes).unwrap());
    let mut v = serde_json::to_value(q).unwrap();
    v["unexpected"] = true.into();
    assert!(decode_request(&serde_json::to_vec(&v).unwrap()).is_err());
    assert!(decode_request(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
    let mut too_many = r.predicate.members()[0].clone();
    too_many.member_id = id(100);
    assert!(
        Predicate::Any {
            members: vec![too_many; MAX_MEMBERS + 1]
        }
        .validate()
        .is_err()
    );
}
#[test]
fn transport_acceptance_does_not_answer_human_question() {
    let r = registration();
    let mut view = NotificationView {
        scope: r.scope,
        message: MessageRevision {
            message_id: id(60),
            revision: 1,
        },
        route_revision: Some(artifact_ref("route-1")),
        delivery: DeliveryState::AcceptedByTransport {
            receipt: artifact_ref("transport-only"),
        },
        human_acknowledgment: HumanAcknowledgment::Unacknowledged,
        decision: DecisionView::Pending {
            decision_id: id(61),
            subject_revision: Text::new("7").unwrap(),
            question_digest: Digest::of(b"question"),
        },
        digest_window: None,
    };
    view.validate().unwrap();
    assert_eq!(
        view.human_acknowledgment,
        HumanAcknowledgment::Unacknowledged
    );
    assert!(matches!(view.decision, DecisionView::Pending { .. }));
    view.route_revision = None;
    assert!(view.validate().is_err());
}
#[test]
fn no_ack_without_current_admitted_attachment() {
    let r = registration();
    let mut p = ConformanceApi::new(
        CallerContext {
            role: ActorRole::SessionAgent,
            scope: r.scope.clone(),
            binding: Some(r.binding.clone()),
            current_operation_id: None,
            current_run_id: None,
        },
        vec![Capability {
            id: Text::new(ACK_CAPABILITY).unwrap(),
            version: 1,
        }],
    )
    .unwrap();
    let continuation = worker_lifecycle::ContinuationRef {
        continuation_id: id(70),
        generation: 1,
        wait: r.wait_ref(),
    };
    let ack = AckRequest {
        admission_id: id(71),
        continuation,
        successor: r.binding.clone(),
        attachment_digest: Digest::of(b"unknown"),
        outcome: OutcomeRef {
            outcome_id: id(72),
            wait: r.wait_ref(),
            revision: 1,
            digest: Digest::of(b"outcome"),
        },
    };
    let q = Request::seal(id(73), Command::ContinuationAck(ack)).unwrap();
    assert!(p.receive(&q, 10).is_err());
    assert!(!p.bootstrap_acknowledged());
}
#[test]
fn invalid_source_digest_cannot_change_predicate() {
    let (r, mut p) = registered_peer();
    let mut v = verdicts(&r, MemberState::Satisfied);
    v[1].evidence.as_mut().unwrap().condition_digest = Digest::of(b"different task");
    assert!(p.resolve_from_fixture(r.wait_id, v, id(40), 20).is_err());
}
#[test]
fn exact_successor_ack_interoperates_with_public_attachment() {
    let attachment: ContinuationAttachment =
        serde_json::from_str(include_str!("../artifacts/public-continuation.json")).unwrap();
    attachment.validate().unwrap();
    let binding = attachment.successor.clone();
    let mut p = ConformanceApi::new(
        CallerContext {
            role: ActorRole::SessionAgent,
            scope: Scope {
                project_id: binding.project_id,
                workspace_id: id(88),
            },
            binding: Some(binding.clone()),
            current_operation_id: None,
            current_run_id: None,
        },
        vec![Capability {
            id: Text::new(ACK_CAPABILITY).unwrap(),
            version: 1,
        }],
    )
    .unwrap();
    p.install_attachment(attachment.clone()).unwrap();
    let ack = AckRequest {
        admission_id: attachment.admission_id,
        continuation: attachment.continuation.clone(),
        successor: binding,
        attachment_digest: canonical_digest(&attachment).unwrap(),
        outcome: attachment.outcome.clone(),
    };
    let mut stale = ack.clone();
    stale.continuation.generation += 1;
    assert!(
        p.receive(
            &Request::seal(id(91), Command::ContinuationAck(stale)).unwrap(),
            10
        )
        .is_err()
    );
    assert!(!p.bootstrap_acknowledged());
    let q = Request::seal(id(92), Command::ContinuationAck(ack)).unwrap();
    let response = p.receive(&q, 20).unwrap();
    response.validate_against(&q).unwrap();
    assert!(p.bootstrap_acknowledged());
    assert_eq!(response, p.receive(&q, 21).unwrap());
}
