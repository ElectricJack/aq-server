use aq_central_authority::*;
use serde_json::{Value, json};
use uuid::Uuid;

fn uuid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn artifact(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn examples() -> Value {
    serde_json::from_str(include_str!("../artifacts/examples.json")).unwrap()
}
fn plan() -> CentralPlan {
    match decode(&serde_json::to_vec(&examples()["plan"]).unwrap()).unwrap() {
        Document::Plan(p) => p,
        _ => panic!(),
    }
}
fn clock(ms: u64) -> Clock {
    Clock {
        machine_boot_id: uuid(9),
        boot_ms: ms,
    }
}
fn permit(p: &CentralPlan, sequence: u64, issued: u64, expires: u64) -> PermitData {
    PermitData {
        binding: p.binding().unwrap(),
        machine_boot_id: uuid(9),
        sequence,
        issued_boot_ms: issued,
        expires_boot_ms: expires,
    }
}
fn launch(p: &CentralPlan) -> HostLaunchRef {
    HostLaunchRef {
        binding: p.binding().unwrap(),
        launch_id: uuid(100 + p.generation as u128),
        intent: artifact("launch-intent"),
    }
}
fn observation(p: &CentralPlan) -> HostObservationRef {
    HostObservationRef {
        binding: p.binding().unwrap(),
        launch_id: launch(p).launch_id,
        guardian: artifact("guardian"),
        observation: artifact("host-report"),
    }
}
fn start(p: CentralPlan, at: u64) -> Run {
    let dispatch = p.dispatch(permit(&p, 1, at, at + 100_000)).unwrap();
    let launch = launch(&p);
    let state = Run::new(p, clock(at)).unwrap();
    let transition = state
        .transition(&Event::CommitStart { launch, dispatch }, clock(at))
        .unwrap();
    assert_eq!(transition.decision, Decision::Applied);
    transition.state
}
fn started() -> Run {
    start(plan(), 1000)
}
fn apply(state: &Run, event: Event, at: u64) -> Run {
    let transition = state.transition(&event, clock(at)).unwrap();
    assert_eq!(transition.decision, Decision::Applied);
    transition.state
}
fn stop(state: &Run, at: u64) -> Run {
    apply(
        state,
        Event::Stopped {
            evidence: StopEvidenceRef {
                host: observation(state.plan()),
                stopped_writers: artifact("stopped-writers"),
            },
        },
        at,
    )
}
fn evidence(state: &Run, disposition: HandoffDisposition) -> HandoffEvidence {
    HandoffEvidence {
        association: state.association().unwrap().clone(),
        disposition,
        receipt: artifact("input-receipt"),
    }
}
fn next_plan(previous: &CentralPlan, message: u128, cursor: u64) -> CentralPlan {
    let mut p = previous.clone();
    p.generation += 1;
    p.session = SessionIdentity::Central {
        authority_id: uuid(3),
        session_id: uuid(1000 + p.generation as u128),
        generation: p.generation,
    };
    p.dispatch_id = uuid(2000 + p.generation as u128 * 2);
    p.host_operation_id = uuid(2001 + p.generation as u128 * 2);
    p.batch.messages = vec![MailRef {
        message_id: uuid(message),
        cursor,
        payload_digest: Digest::of(b"other"),
    }];
    p.batch.through_cursor = cursor;
    p
}
fn history(p: &CentralPlan) -> MailHistoryFence {
    MailHistoryFence {
        schema_version: 1,
        actor_id: p.subject.actor_id(),
        conversation_id: p.subject.conversation_id(),
        through_generation: p.generation - 1,
        source_snapshot: artifact("indexed-actor-history"),
        entries: p
            .batch
            .messages
            .iter()
            .map(|m| MessageHistory {
                message: m.clone(),
                disposition: HistoryDisposition::NoPriorDispatch,
            })
            .collect(),
        operations: [p.dispatch_id, p.host_operation_id]
            .into_iter()
            .map(|operation_id| OperationHistory {
                operation_id,
                disposition: OperationDisposition::Unused,
            })
            .collect(),
    }
}
fn refuse(transition: Transition, reason: Refusal) -> Run {
    assert_eq!(transition.decision, Decision::Refused { reason });
    transition.state
}

#[test]
fn published_plan_and_permit_are_exact_roundtrippable_pins() {
    for key in examples().as_object().unwrap().keys() {
        let value = examples()[key].clone();
        let doc = decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&encode(&doc).unwrap()).unwrap(),
            value
        );
    }
    let Document::Permit(p) = decode(&serde_json::to_vec(&examples()["permit"]).unwrap()).unwrap()
    else {
        panic!()
    };
    assert_eq!(p.binding, plan().binding().unwrap());
    let mut changed = plan();
    changed.profiles.renderer = artifact("different-renderer");
    assert_ne!(changed.digest().unwrap(), p.binding.plan_digest);
}

#[test]
fn closed_wire_rejects_unknown_duplicates_oversize_and_fabricated_task_scope() {
    for path in ["project_id", "task_id", "node_id"] {
        let mut value = examples()["plan"].clone();
        value["data"]["subject"][path] = json!(uuid(50));
        assert!(decode(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    let bytes = serde_json::to_string(&examples()["plan"]).unwrap();
    let duplicate = bytes.replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    assert_ne!(duplicate, bytes);
    assert!(decode(duplicate.as_bytes()).is_err());
    assert!(decode(&vec![b' '; MAX_WIRE_BYTES + 1]).is_err());
    let mut value = examples()["plan"].clone();
    value["data"]["session"] = json!({"kind":"remote","identity":{}});
    assert!(decode(&serde_json::to_vec(&value).unwrap()).is_err());
}

#[test]
fn validates_real_identity_generations_batches_and_bounded_turns() {
    let p = plan();
    let mut bad = p.clone();
    bad.wall_time_ms = MAX_WALL_MS + 1;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.generation = 2;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.session = SessionIdentity::Central {
        authority_id: Uuid::nil(),
        session_id: uuid(1),
        generation: 1,
    };
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.dispatch_id = Uuid::nil();
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.host_operation_id = bad.dispatch_id;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.max_permit_ms = MAX_PERMIT_MS + 1;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.batch.messages.clear();
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.batch.messages.swap(0, 1);
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.batch.messages[1].message_id = bad.batch.messages[0].message_id;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.batch.through_cursor = 1;
    assert!(bad.validate().is_err());
    let mut bad = p.clone();
    bad.batch.messages = (1..=17)
        .map(|n| MailRef {
            message_id: uuid(n as u128),
            cursor: n,
            payload_digest: Digest::of(b"x"),
        })
        .collect();
    bad.batch.through_cursor = 17;
    assert!(bad.validate().is_err());
    let p2 = next_plan(&p, 30, 3);
    assert!(Run::new(p2, clock(1000)).is_err());
    assert!(Run::new(p, clock(u64::MAX)).is_err());
}

#[test]
fn start_is_bound_to_exact_plan_permit_batch_and_launch() {
    let p = plan();
    let state = Run::new(p.clone(), clock(1000)).unwrap();
    let expected = p.dispatch(permit(&p, 1, 1000, 101000)).unwrap();
    let mut wrong = expected.clone();
    wrong.batch_digest = Digest::of(b"wrong-batch");
    let t = state
        .transition(
            &Event::CommitStart {
                launch: launch(&p),
                dispatch: wrong,
            },
            clock(1000),
        )
        .unwrap();
    assert_eq!(refuse(t, Refusal::Binding), state);
    let mut wrong = expected.clone();
    wrong.host_operation_id = uuid(77);
    assert_eq!(
        refuse(
            state
                .transition(
                    &Event::CommitStart {
                        launch: launch(&p),
                        dispatch: wrong
                    },
                    clock(1000)
                )
                .unwrap(),
            Refusal::Binding
        ),
        state
    );
    let mut wrong = launch(&p);
    wrong.binding.plan_digest = Digest::of(b"different-plan");
    refuse(
        state
            .transition(
                &Event::CommitStart {
                    launch: wrong,
                    dispatch: expected.clone(),
                },
                clock(1000),
            )
            .unwrap(),
        Refusal::Binding,
    );
    let running = started();
    assert_eq!(
        running
            .transition(
                &Event::CommitStart {
                    launch: launch(&p),
                    dispatch: expected
                },
                clock(1100)
            )
            .unwrap()
            .decision,
        Decision::Replayed
    );
    let mut altered = launch(&p);
    altered.launch_id = uuid(200);
    refuse(
        running
            .transition(
                &Event::CommitStart {
                    launch: altered,
                    dispatch: running.association().unwrap().clone(),
                },
                clock(1100),
            )
            .unwrap(),
        Refusal::Phase,
    );
}

#[test]
fn renewals_are_monotonic_bounded_and_do_not_reset_wall_deadline() {
    let state = started();
    let deadline = state.original_deadline_boot_ms();
    let p = permit(state.plan(), 2, 50000, 170000);
    let renewed = apply(&state, Event::Renew { permit: p.clone() }, 50000);
    assert_eq!(renewed.original_deadline_boot_ms(), deadline);
    assert_eq!(renewed.association(), state.association());
    assert_eq!(
        renewed
            .transition(&Event::Renew { permit: p.clone() }, clock(50001))
            .unwrap()
            .decision,
        Decision::Replayed
    );
    let mut changed = p.clone();
    changed.expires_boot_ms += 1;
    changed.issued_boot_ms += 1;
    refuse(
        renewed
            .transition(&Event::Renew { permit: changed }, clock(50001))
            .unwrap(),
        Refusal::Conflict,
    );
    for seq in [1, 4] {
        let wrong = permit(renewed.plan(), seq, 60000, 180000);
        refuse(
            renewed
                .transition(&Event::Renew { permit: wrong }, clock(60000))
                .unwrap(),
            Refusal::Sequence,
        );
    }
    let p3 = permit(renewed.plan(), 3, 160000, 280000);
    let last = apply(&renewed, Event::Renew { permit: p3 }, 160000);
    let over = permit(last.plan(), 4, 270000, 390000);
    let kept = refuse(
        last.transition(&Event::Renew { permit: over }, clock(270000))
            .unwrap(),
        Refusal::Deadline,
    );
    let final_permit = permit(kept.plan(), 4, 270000, 301000);
    let final_live = apply(
        &kept,
        Event::Renew {
            permit: final_permit,
        },
        270000,
    );
    let elapsed = apply(&final_live, Event::Tick, 301000);
    assert_eq!(elapsed.phase(), Phase::StopRequested);
    assert_eq!(elapsed.cancellation(), Some(CancelReason::WallDeadline));
    assert_eq!(deadline, 301000);
}

#[test]
fn expired_permit_cannot_revive_and_refused_event_preserves_expiry() {
    let state = started();
    let p = permit(state.plan(), 2, 101000, 221000);
    let stopped = refuse(
        state
            .transition(&Event::Renew { permit: p }, clock(101000))
            .unwrap(),
        Refusal::Inactive,
    );
    assert_eq!(stopped.phase(), Phase::StopRequested);
    assert_eq!(stopped.cancellation(), Some(CancelReason::PermitExpired));
    assert!(matches!(stopped.dispatch(), DispatchState::Unknown { .. }));
    let late = apply(
        &stopped,
        Event::Running {
            observation: observation(stopped.plan()),
        },
        101001,
    );
    assert_eq!(late.phase(), Phase::StopRequested);
    assert!(
        late.successor(
            next_plan(late.plan(), 30, 3),
            clock(102000),
            &history(&next_plan(late.plan(), 30, 3))
        )
        .is_err()
    );
}

#[test]
fn foreign_boot_and_clock_regression_cannot_mutate_state() {
    let state = started();
    for wrong in [
        Clock {
            machine_boot_id: uuid(90),
            boot_ms: 2000,
        },
        clock(999),
    ] {
        assert_eq!(
            refuse(
                state.transition(&Event::Tick, wrong).unwrap(),
                Refusal::Clock
            ),
            state
        );
    }
}

#[test]
fn cancellation_is_not_stopped_proof_and_late_observations_do_not_revive() {
    let p = plan();
    let initial = Run::new(p.clone(), clock(1000)).unwrap();
    let cancelled = apply(
        &initial,
        Event::Cancel {
            reason: CancelReason::AuthorityRevoked,
        },
        1001,
    );
    assert_eq!(cancelled.phase(), Phase::CancelledBeforeStart);
    assert!(cancelled.association().is_none());
    let state = started();
    let cancelled = apply(
        &state,
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        1001,
    );
    assert_eq!(cancelled.phase(), Phase::StopRequested);
    let lost = apply(
        &cancelled,
        Event::HostLost {
            binding: p.binding().unwrap(),
            launch_id: launch(&p).launch_id,
            report: artifact("guardian-unreachable"),
        },
        1002,
    );
    assert_eq!(lost.phase(), Phase::StopUncertain);
    let np = next_plan(&p, 30, 3);
    assert_eq!(
        lost.successor(np.clone(), clock(2000), &history(&np)),
        Err(Error::Refused(Refusal::Phase))
    );
    let late = apply(
        &lost,
        Event::Running {
            observation: observation(&p),
        },
        1003,
    );
    assert_eq!(late.phase(), Phase::StopUncertain);
    let stopped = stop(&late, 1004);
    assert!(matches!(stopped.dispatch(), DispatchState::Unknown { .. }));
    assert!(
        stopped
            .successor(np.clone(), clock(2000), &history(&np))
            .is_ok()
    );
    assert_eq!(
        apply(
            &stopped,
            Event::Running {
                observation: HostObservationRef {
                    observation: artifact("later-running-report"),
                    ..observation(&p)
                }
            },
            1005
        )
        .phase(),
        Phase::StoppedReported
    );
}

#[test]
fn uncertain_input_needs_exact_explicit_reconciliation_and_never_changes_known_outcome() {
    let state = started();
    let assoc = state.association().unwrap().clone();
    let unknown = apply(&state, Event::ReportUnknown { association: assoc }, 1100);
    let receipt = evidence(&unknown, HandoffDisposition::NotHandedOff);
    refuse(
        unknown
            .transition(
                &Event::Handoff {
                    evidence: receipt.clone(),
                },
                clock(1101),
            )
            .unwrap(),
        Refusal::NeedsReconciliation,
    );
    let mut wrong = receipt.clone();
    wrong.association.permit.sequence = 2;
    refuse(
        unknown
            .transition(&Event::ReconcileHandoff { evidence: wrong }, clock(1101))
            .unwrap(),
        Refusal::Binding,
    );
    let known = apply(
        &unknown,
        Event::ReconcileHandoff {
            evidence: receipt.clone(),
        },
        1102,
    );
    let stopped = stop(&known, 1200);
    assert_eq!(
        stopped.dispatch(),
        &DispatchState::Resolved {
            evidence: Box::new(receipt.clone())
        }
    );
    assert_eq!(
        stopped
            .transition(
                &Event::Handoff {
                    evidence: receipt.clone()
                },
                clock(1201)
            )
            .unwrap()
            .decision,
        Decision::Replayed
    );
    let mut contradictory = receipt.clone();
    contradictory.disposition = HandoffDisposition::HandedOff;
    refuse(
        stopped
            .transition(
                &Event::ReconcileHandoff {
                    evidence: contradictory,
                },
                clock(1201),
            )
            .unwrap(),
        Refusal::Conflict,
    );
    let mut np = next_plan(stopped.plan(), 5, 1);
    np.batch.messages = vec![plan().batch.messages[0].clone()];
    let mut h = history(&np);
    h.entries[0].disposition = HistoryDisposition::Retryable { evidence: receipt };
    assert!(stopped.successor(np.clone(), clock(1300), &h).is_ok());
    h.entries[0].disposition = HistoryDisposition::NoPriorDispatch;
    assert_eq!(
        stopped.successor(np, clock(1300), &h),
        Err(Error::Refused(Refusal::Conflict))
    );
}

#[test]
fn all_generation_history_blocks_gen1_replay_after_disjoint_gen2() {
    let first = stop(&started(), 1100); // Physical stop says nothing about mail handoff.
    let p2 = next_plan(first.plan(), 30, 3);
    let second = first
        .successor(p2.clone(), clock(1200), &history(&p2))
        .unwrap();
    let second = apply(
        &second,
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        1201,
    );
    let mut p3 = next_plan(&p2, 5, 1);
    p3.batch.messages = vec![first.plan().batch.messages[0].clone()];
    let mut h = history(&p3);
    h.entries[0].disposition = HistoryDisposition::Held {
        association: first.association().unwrap().clone(),
        reason: MailHoldReason::Unknown,
    };
    assert_eq!(
        second.successor(p3.clone(), clock(1300), &h),
        Err(Error::Refused(Refusal::BatchHeld))
    );
    h.entries[0].disposition = HistoryDisposition::Held {
        association: first.association().unwrap().clone(),
        reason: MailHoldReason::HandedOff,
    };
    assert_eq!(
        second.successor(p3, clock(1300), &h),
        Err(Error::Refused(Refusal::BatchHeld))
    );
}

#[test]
fn history_requires_exact_cursor_identity_and_all_operation_roles() {
    let first = stop(&started(), 1100);
    let p2 = next_plan(first.plan(), 30, 3);
    let second = first
        .successor(p2.clone(), clock(1200), &history(&p2))
        .unwrap();
    let second = apply(
        &second,
        Event::Cancel {
            reason: CancelReason::Operator,
        },
        1201,
    );
    let p3 = next_plan(&p2, 40, 1); // Different UUID aliases generation1's authoritative cursor.
    let mut h = history(&p3);
    h.entries[0].message = first.plan().batch.messages[0].clone();
    assert_eq!(
        second.successor(p3, clock(1300), &h),
        Err(Error::Refused(Refusal::Conflict))
    );
    let mut p3 = next_plan(&p2, 40, 4);
    p3.host_operation_id = first.plan().dispatch_id;
    let mut h = history(&p3);
    h.operations[1].disposition = OperationDisposition::Used {
        generation: 1,
        role: OperationRole::Dispatch,
        association_digest: Digest::of(b"retained-original-association"),
    };
    assert_eq!(
        second.successor(p3, clock(1300), &h),
        Err(Error::Refused(Refusal::Conflict))
    );
    let mut swapped = next_plan(&p2, 40, 4);
    swapped.host_operation_id = p2.dispatch_id;
    assert_eq!(
        second.successor(swapped.clone(), clock(1300), &history(&swapped)),
        Err(Error::Refused(Refusal::Binding))
    );
}

#[test]
fn history_refuses_missing_duplicate_foreign_and_stale_coverage() {
    let first = stop(&started(), 1100);
    let p2 = next_plan(first.plan(), 30, 3);
    let h = history(&p2);
    let mut bad = h.clone();
    bad.entries.clear();
    assert!(first.successor(p2.clone(), clock(1200), &bad).is_err());
    let mut bad = h.clone();
    bad.operations.pop();
    assert!(first.successor(p2.clone(), clock(1200), &bad).is_err());
    let mut bad = h.clone();
    bad.operations[1] = bad.operations[0].clone();
    assert!(first.successor(p2.clone(), clock(1200), &bad).is_err());
    let mut bad = h.clone();
    bad.actor_id = uuid(80);
    assert_eq!(
        first.successor(p2.clone(), clock(1200), &bad),
        Err(Error::Refused(Refusal::Binding))
    );
    let mut bad = h.clone();
    bad.through_generation = 2;
    assert_eq!(
        first.successor(p2.clone(), clock(1200), &bad),
        Err(Error::Refused(Refusal::Binding))
    );
    let mut bad = h.clone();
    bad.operations[0].operation_id = uuid(80);
    assert_eq!(
        first.successor(p2.clone(), clock(1200), &bad),
        Err(Error::Refused(Refusal::Binding))
    );
    let mut bad = h;
    bad.entries[0].message.payload_digest = Digest::of(b"changed");
    assert_eq!(
        first.successor(p2, clock(1200), &bad),
        Err(Error::Refused(Refusal::Conflict))
    );
}

#[test]
fn snapshot_decode_checks_cross_associations_and_deadline_not_only_shape() {
    let state = stop(&started(), 1100);
    let wire = encode(&Document::State(state.clone())).unwrap();
    assert_eq!(decode(&wire).unwrap(), Document::State(state));
    let value: Value = serde_json::from_slice(&wire).unwrap();
    let mut changed = value.clone();
    changed["data"]["original_deadline_boot_ms"] = json!(999999);
    assert!(decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let mut changed = value.clone();
    changed["data"]["association"]["batch_digest"] = json!(Digest::of(b"other"));
    assert!(decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let mut changed = value.clone();
    changed["data"]["stop"]["host"]["launch_id"] = json!(uuid(88));
    assert!(decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let mut changed = value;
    changed["data"]["dispatch"] = json!({"kind":"intent_committed"});
    assert!(decode(&serde_json::to_vec(&changed).unwrap()).is_err());
    let prepared = Run::new(plan(), clock(1000)).unwrap();
    let mut changed = serde_json::to_value(Document::State(prepared)).unwrap();
    changed["data"]["last_clock"]["boot_ms"] = json!(301000);
    assert!(decode(&serde_json::to_vec(&changed).unwrap()).is_err());
}

#[test]
fn runtime_is_always_unavailable_and_export_admits_only_central_identity() {
    let caps = runtime_capabilities();
    assert!(!caps.execution() && !caps.mail_dispatch() && !caps.replies());
    assert_eq!(
        serde_json::to_value(caps).unwrap(),
        json!({"execution":false,"mail_dispatch":false,"replies":false})
    );
    let s = schema();
    let branches = s["$defs"]["SessionIdentity"]["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0]["properties"]["kind"]["const"], "central");
    assert_eq!(s["x-execution-authority"], false);
}

#[test]
fn exported_schema_stays_frozen() {
    let published: Value = serde_json::from_str(include_str!("../artifacts/schema.json")).unwrap();
    assert_eq!(schema(), published);
}

#[test]
fn known_handed_off_mail_and_changed_original_receipt_cannot_be_reused() {
    let state = started();
    let receipt = evidence(&state, HandoffDisposition::HandedOff);
    let known = apply(
        &state,
        Event::Handoff {
            evidence: receipt.clone(),
        },
        1001,
    );
    let stopped = stop(&known, 1002);
    let mut next = next_plan(stopped.plan(), 5, 1);
    next.batch.messages = vec![stopped.plan().batch.messages[0].clone()];
    // The immediate original state protects even a dishonest all-history unseen row.
    let h = history(&next);
    assert_eq!(
        stopped.successor(next, clock(1003), &h),
        Err(Error::Refused(Refusal::BatchHeld))
    );
    let mut wrong = StopEvidenceRef {
        host: observation(stopped.plan()),
        stopped_writers: artifact("other-writer-proof"),
    };
    refuse(
        stopped
            .transition(
                &Event::Stopped {
                    evidence: wrong.clone(),
                },
                clock(1003),
            )
            .unwrap(),
        Refusal::Conflict,
    );
    wrong.host.guardian = artifact("different-guardian");
    refuse(
        stopped
            .transition(&Event::Stopped { evidence: wrong }, clock(1003))
            .unwrap(),
        Refusal::Binding,
    );
    let late = apply(&stopped, Event::Tick, 400000);
    assert_eq!(late.phase(), Phase::StoppedReported);
    assert_eq!(
        late.dispatch(),
        &DispatchState::Resolved {
            evidence: Box::new(receipt)
        }
    );
    assert_eq!(late.cancellation(), None);
}
