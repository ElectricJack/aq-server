use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn authority() -> Authority {
    Authority {
        actor: "alice".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}
fn ret(outcome: &str) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn artifact(
    id: &str,
    start: &str,
    steps: BTreeMap<String, Step>,
    registry: Registry,
    dependencies: BTreeMap<String, CallableDependency>,
) -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header: SourceHeader {
                format_version: 1,
                id: id.into(),
                scope: "project".into(),
                interfaces: set(&["main"]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: id.into(),
            purpose: "semantics test".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([
                        ("ok".into(), Schema::Null),
                        ("error".into(), Schema::Null),
                    ]),
                    start: start.into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: set(&["execute"]),
        },
        registry,
        dependencies,
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::default(), "test", "incarnation-1").unwrap()
}
fn install(e: &Engine<MemoryStorage>, a: Artifact, binding: &str) -> String {
    let d = a.digest.clone();
    e.install(a).unwrap();
    e.activate(binding, &d, authority()).unwrap();
    d
}
fn command(recovery: Recovery) -> CommandContract {
    CommandContract {
        name: "test.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null), ("retry".into(), Schema::Null)]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 100,
    }
}
fn invoke_artifact(c: CommandContract, retry: Option<RetryPolicy>) -> Artifact {
    let reference = ContractRef {
        name: c.name.clone(),
        fingerprint: c.fingerprint().unwrap(),
    };
    artifact(
        "invoke",
        "invoke",
        BTreeMap::from([
            (
                "invoke".into(),
                Step {
                    op: Operation::Invoke {
                        command: reference,
                        input: Expr::literal(Value::Null),
                        bind: "result".into(),
                        retry,
                    },
                    edges: edges(&[("ok", "ok"), ("retry", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        Registry {
            commands: BTreeMap::from([(c.name.clone(), c)]),
            events: BTreeMap::new(),
        },
        BTreeMap::new(),
    )
}
fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "test.completed".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["invocation"]),
        subject_field: Some("subject".into()),
        revision_field: Some("revision".into()),
        max_payload_bytes: 1024,
        retention_ms: 1000,
    }
}
fn wait_artifact() -> Artifact {
    let c = event_contract();
    artifact(
        "waiter",
        "wait",
        BTreeMap::from([
            (
                "wait".into(),
                Step {
                    op: Operation::Wait {
                        event: Some(ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        }),
                        correlation: BTreeMap::from([(
                            "invocation".into(),
                            Expr::literal("exact-invocation"),
                        )]),
                        minimum_revision: Some(Expr::literal(2)),
                        deadline_ms: 100,
                        bind: "completion".into(),
                    },
                    edges: edges(&[("received", "ok"), ("timeout", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(c.name.clone(), c)]),
        },
        BTreeMap::new(),
    )
}
fn event(id: &str, rev: u64) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), "exact-invocation".into())]),
        subject: Some("object".into()),
        revision: Some(rev),
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: Value::Null,
    }
}
#[test]
fn event_before_wait_restart_stale_revision_and_backlog_deadline() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.admit_event("host", event("stale", 1), 10).unwrap();
    let accepted = e.admit_event("host", event("eligible", 2), 99).unwrap();
    assert_eq!(accepted.destinations, 0);
    assert!(
        e.admit_event("host", event("eligible", 2), 101)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.metrics.snapshot().duplicate_events, 1);
    e.release().unwrap();
    let resumed = Engine::new(e.store.clone(), "test", "incarnation-2").unwrap();
    resumed.advance(&id, 0).unwrap();
    resumed.advance(&id, 150).unwrap();
    resumed.advance(&id, 151).unwrap();
    let s = resumed.snapshot().unwrap();
    assert_eq!(s.runs[&id].outcome.as_deref(), Some("ok"));
    assert_eq!(
        s.waits.values().next().unwrap().winner.as_deref(),
        Some(accepted.key.as_str())
    );
}
#[test]
fn expiry_wins_over_late_event_and_identity_conflicts() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.admit_event("host", event("late", 2), 101).unwrap();
    e.advance(&id, 110).unwrap();
    e.advance(&id, 111).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("error")
    );
    let mut changed = event("late", 3);
    assert!(matches!(
        e.admit_event("host", changed.clone(), 112),
        Err(Error::Conflict(_))
    ));
    changed.producer = "impostor".into();
    assert!(matches!(
        e.admit_event("host", changed, 113),
        Err(Error::Refused(_))
    ));
}
#[test]
fn durable_intent_replay_keeps_id_business_retry_creates_new_id() {
    let e = engine();
    install(
        &e,
        invoke_artifact(
            command(Recovery::KeyedIdempotent),
            Some(RetryPolicy {
                max_attempts: 2,
                backoff_ms: 10,
                outcomes: set(&["retry"]),
            }),
        ),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let first = e.snapshot().unwrap().intents.keys().next().unwrap().clone();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Unknown {
                    reason: "lost after host commit".into(),
                },
                DispatchResult::Completed {
                    outcome: "retry".into(),
                    output: Value::Null,
                },
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
            ],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, 0).unwrap();
    e.advance(&id, 0).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.recover(&mut host, 1).unwrap();
    e.dispatch_next(&mut host, 1).unwrap();
    e.advance(&id, 1).unwrap();
    e.advance(&id, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 1);
    e.drive(&mut host, 11, 30).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].outcome.as_deref(), Some("ok"));
    assert_eq!(s.intents.len(), 2);
    assert_eq!(host.dispatched[0], first);
    assert_eq!(host.dispatched[1], first);
    assert_ne!(host.dispatched[1], host.dispatched[2]);
}
#[test]
fn unsafe_ambiguity_is_never_automatically_repeated_or_cancelled() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    e.recover(&mut host, 1).unwrap();
    e.drive(&mut host, 1, 10).unwrap();
    assert_eq!(host.dispatched.len(), 1);
    e.cancel(&id, 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].status,
        RunStatus::Cancelling
    );
}
fn child_dependency(a: &Artifact) -> CallableDependency {
    CallableDependency {
        selection: DependencySelection::Exact,
        binding: a.content.playbook.id.clone(),
        artifact: a.digest.clone(),
        interfaces: a.content.playbook.entries.clone(),
        capabilities: set(&["execute"]),
        scope: "project".into(),
        dependencies: BTreeSet::new(),
    }
}
fn parent_artifact(child: &Artifact, mode: CallMode) -> Artifact {
    let synchronous = mode == CallMode::Wait;
    artifact(
        "parent",
        "call",
        BTreeMap::from([
            (
                "call".into(),
                Step {
                    op: Operation::Call {
                        dependency: "child".into(),
                        entry: "main".into(),
                        input: Expr::literal(Value::Null),
                        mode,
                        bind: "child_handle".into(),
                    },
                    edges: if synchronous {
                        edges(&[("ok", "ok"), ("error", "error")])
                    } else {
                        edges(&[("started", "ok")])
                    },
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        Registry::default(),
        BTreeMap::from([("child".into(), child_dependency(child))]),
    )
}
#[test]
fn all_three_child_modes_are_atomic_and_independent_cancellation_survives() {
    for mode in [CallMode::Wait, CallMode::OwnedAsync, CallMode::Independent] {
        let e = engine();
        let child = artifact(
            "child",
            "ok",
            BTreeMap::from([("ok".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        );
        e.install(child.clone()).unwrap();
        install(&e, parent_artifact(&child, mode.clone()), "parent");
        let id = e
            .admit("parent", "main", Value::Null, authority(), 0)
            .unwrap();
        e.advance(&id, 0).unwrap();
        let first = e.snapshot().unwrap();
        assert_eq!(first.runs.len(), 2);
        let child = first.runs.values().find(|r| r.id != id).unwrap().id.clone();
        e.advance(&id, 0).unwrap();
        assert_eq!(e.snapshot().unwrap().runs.len(), 2);
        if mode == CallMode::Independent {
            e.cancel(&id, 1).unwrap();
            e.advance(&id, 1).unwrap();
            assert_eq!(
                e.snapshot().unwrap().runs[&child].status,
                RunStatus::Admitted
            );
        }
        let mut host = FakeHost::default();
        e.drive(&mut host, 2, 20).unwrap();
        assert!(e.snapshot().unwrap().runs[&id].status.terminal());
        assert_eq!(
            e.snapshot().unwrap().runs[&child].status,
            RunStatus::Completed
        );
    }
}
#[test]
fn simulation_missing_mock_is_unknown_and_incomplete() {
    let mut simulation = Simulation::new(FakeHost::default()).unwrap();
    install(
        &simulation.engine,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    simulation
        .engine
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let report = simulation.run(20).unwrap();
    assert!(report.incomplete);
    assert_eq!(report.unknown_operations.len(), 1);
    assert_eq!(simulation.host.dispatched.len(), 1);
}
#[test]
fn memory_store_cas_and_owner_fence_full_boundary() {
    let store = MemoryStorage::default();
    store.acquire("n", "one").unwrap();
    assert_eq!(store.acquire("n", "two"), Err(StorageError::Ownership));
    let old = store.load("n").unwrap();
    let mut proposed = old.state.clone();
    proposed.sequence = 42;
    store.compare_and_swap("n", "one", 0, &proposed).unwrap();
    assert_eq!(
        store.compare_and_swap("n", "one", 0, &State::default()),
        Err(StorageError::Conflict)
    );
    assert_eq!(store.load("n").unwrap().state.sequence, 42);
    store.release("n", "one").unwrap();
    assert_eq!(
        store.compare_and_swap("n", "one", 1, &proposed),
        Err(StorageError::Ownership)
    );
}
fn async_artifact() -> (Artifact, EventContract) {
    let admission = Schema::object(BTreeMap::from([("handle".into(), Schema::string(256))]));
    let completion = Schema::object(BTreeMap::from([
        (
            "outcome".into(),
            Schema::String {
                max_length: 10,
                values: set(&["ok", "retry"]),
            },
        ),
        ("output".into(), Schema::Null),
    ]));
    let mut ev = event_contract();
    ev.name = "async.completed".into();
    ev.payload = completion.clone();
    ev.correlation_fields = set(&["operation"]);
    ev.subject_field = None;
    ev.revision_field = None;
    let mut c = command(Recovery::Reconcilable);
    c.cancellation = true;
    c.outcomes.insert("accepted".into(), admission);
    c.asynchronous = Some(AsyncContract {
        completion_event: ev.name.clone(),
        completion,
        correlation_field: "operation".into(),
        reconciliation_command: "operation.get".into(),
    });
    let mut get = command(Recovery::Observation);
    get.name = "operation.get".into();
    get.effect = Effect::Observation;
    let a = artifact(
        "async",
        "invoke",
        BTreeMap::from([
            (
                "invoke".into(),
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        },
                        input: Expr::literal(Value::Null),
                        bind: "handle".into(),
                        retry: None,
                    },
                    edges: edges(&[("accepted", "join"), ("ok", "ok"), ("retry", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            (
                "join".into(),
                Step {
                    op: Operation::Join {
                        member_keys: vec![],
                        members: vec![Expr::reference("handle")],
                        mode: JoinMode::All,
                        accepted_outcomes: set(&["ok"]),
                        deadline_ms: 100,
                        remaining: RemainingPolicy::Cancel,
                        bind: "joined".into(),
                    },
                    edges: edges(&[
                        ("completed", "ok"),
                        ("no_match", "error"),
                        ("timeout", "fallback"),
                    ]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
            ("fallback".into(), ret("error")),
        ]),
        Registry {
            commands: BTreeMap::from([(c.name.clone(), c), (get.name.clone(), get)]),
            events: BTreeMap::from([(ev.name.clone(), ev.clone())]),
        },
        BTreeMap::new(),
    );
    (a, ev)
}
fn async_event(c: &EventContract, id: &str) -> EventEnvelope {
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: json!({"outcome":"ok","output":null}),
    }
}
#[test]
fn owned_operation_join_uses_exact_pinned_completion_and_retained_admission() {
    let e = engine();
    let (a, c) = async_artifact();
    install(&e, a, "async");
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    let mut other = c.clone();
    other.version = "2".into();
    let alternate = artifact(
        "alternate",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(other.name.clone(), other.clone())]),
        },
        BTreeMap::new(),
    );
    e.install(alternate).unwrap();
    e.admit_event("host", async_event(&other, "wrong-contract"), 10)
        .unwrap();
    e.drive(&mut host, 10, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.admit_event("host", async_event(&c, "right-contract"), 20)
        .unwrap();
    e.drive(&mut host, 20, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("ok")
    );
}
#[test]
fn timeout_fallback_waits_for_truthful_owned_operation_cancellation() {
    let e = engine();
    let (a, _) = async_artifact();
    install(&e, a, "async");
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        cancellation_responses: BTreeMap::from([(
            "test.command".into(),
            vec![CancellationResult::Accepted],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    e.drive(&mut host, 101, 20).unwrap();
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&id].status, RunStatus::Finishing);
    let intent = state.intents.values().next().unwrap();
    assert!(intent.cancel_requested);
    assert_eq!(intent.status, IntentStatus::Unknown);
    assert_eq!(host.cancelled.len(), 1);
    // An authoritative completion after the workflow's deadline settles the
    // original operation, but cannot replace the recorded timeout/fallback.
    let c = async_artifact().1;
    e.admit_event("host", async_event(&c, "late"), 110).unwrap();
    e.drive(&mut host, 110, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("error")
    );
}

#[test]
fn exhausted_step_budget_finishes_once_without_reentering_error_graph() {
    let e = engine();
    let mut a = invoke_artifact(command(Recovery::KeyedIdempotent), None).content;
    a.playbook.limits.max_step_visits = 1;
    install(&e, Artifact::seal(a).unwrap(), "invoke");
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    let before = e.snapshot().unwrap();
    assert_eq!(before.runs[&id].status, RunStatus::Failed);
    assert_eq!(
        before
            .receipts
            .iter()
            .filter(|r| r.kind == "engine_failure")
            .count(),
        1
    );
    for _ in 0..10 {
        assert!(!e.advance(&id, 1).unwrap());
    }
    assert_eq!(before.receipts.len(), e.snapshot().unwrap().receipts.len());
}

#[test]
fn history_budget_stops_without_disposing_existing_records() {
    let e = engine();
    let mut a = artifact(
        "small",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    )
    .content;
    a.playbook.limits.max_payload_bytes = 32;
    a.playbook.limits.max_history_bytes = 128;
    let digest = install(&e, Artifact::seal(a).unwrap(), "small");
    let id = e
        .admit("small", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Failed);
    assert!(s.artifacts.contains_key(&digest));
    assert!(s.runs[&id].reason.as_ref().unwrap().contains("history"));
}

#[test]
fn invalid_host_output_blocks_with_a_safe_durable_receipt() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"not-public"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Blocked);
    assert!(s.runs[&id].reason.as_ref().unwrap().contains("invalid"));
    assert_eq!(
        s.intents.values().next().unwrap().status,
        IntentStatus::Unknown
    );
    assert!(s.receipts.iter().any(|r| r.kind == "invalid_output"));
    assert!(
        !serde_json::to_string(&s.receipts)
            .unwrap()
            .contains("not-public")
    );
}

#[test]
fn operation_join_deadline_uses_ingress_time_even_when_processing_is_late() {
    for (accepted_at, expected) in [(99, "ok"), (101, "error")] {
        let e = engine();
        let (a, c) = async_artifact();
        install(&e, a, "async");
        let id = e
            .admit("async", "main", Value::Null, authority(), 0)
            .unwrap();
        let mut host = FakeHost {
            responses: BTreeMap::from([(
                "test.command".into(),
                vec![DispatchResult::Accepted {
                    handle: "server-operation".into(),
                }],
            )]),
            ..Default::default()
        };
        e.drive(&mut host, 0, 10).unwrap();
        e.admit_event("host", async_event(&c, "completion"), accepted_at)
            .unwrap();
        e.drive(&mut host, 150, 30).unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.runs[&id].outcome.as_deref(), Some(expected));
        let branch = s
            .receipts
            .iter()
            .find(|r| r.step == "join" && r.kind == "branch")
            .unwrap();
        assert_eq!(
            branch.detail["outcome"],
            if expected == "ok" {
                "completed"
            } else {
                "timeout"
            }
        );
    }
}

#[test]
fn missing_dependency_destination_is_durably_blocked_then_resumes_exact_bytes() {
    let e = engine();
    let child = artifact(
        "missing-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    let c = event_contract();
    parent.registry.events.insert(c.name.clone(), c.clone());
    parent.playbook.triggers.push(Trigger {
        id: "trigger".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    install(&e, Artifact::seal(parent).unwrap(), "parent");
    let disposition = e
        .admit_event("host", event("missing-dependency", 2), 0)
        .unwrap();
    assert_eq!(disposition.destinations, 1);
    assert_eq!(e.fanout(&disposition.key, 1, 0).unwrap(), 0);
    let blocked = e.snapshot().unwrap();
    assert!(blocked.runs.is_empty());
    assert!(
        blocked
            .receipts
            .iter()
            .any(|r| r.kind == "destination_blocked")
    );
    e.install(child.clone()).unwrap();
    assert_eq!(e.fanout(&disposition.key, 1, 1).unwrap(), 1);
    let mut host = FakeHost::default();
    e.drive(&mut host, 1, 30).unwrap();
    assert!(
        e.snapshot()
            .unwrap()
            .runs
            .values()
            .all(|r| r.status == RunStatus::Completed)
    );
}

#[test]
fn iteration_failure_policy_skips_or_stops_without_resetting_captured_membership() {
    for (policy, expected, calls) in [
        (IterationFailure::Continue, "ok", 2),
        (IterationFailure::Stop, "error", 1),
    ] {
        let e = engine();
        let c = command(Recovery::KeyedIdempotent);
        let a = artifact(
            "iteration",
            "loop",
            BTreeMap::from([
                (
                    "loop".into(),
                    Step {
                        op: Operation::ForEach {
                            collection: Expr::literal(json!([null, null])),
                            item: "item".into(),
                            body: "invoke".into(),
                            next: "ok".into(),
                            max_items: 2,
                            concurrency: 1,
                            on_failure: policy,
                        },
                        edges: BTreeMap::new(),
                        on_error: Some("error".into()),
                    },
                ),
                (
                    "invoke".into(),
                    Step {
                        op: Operation::Invoke {
                            command: ContractRef {
                                name: c.name.clone(),
                                fingerprint: c.fingerprint().unwrap(),
                            },
                            input: Expr::literal(Value::Null),
                            bind: "result".into(),
                            retry: None,
                        },
                        edges: edges(&[("ok", "loop"), ("retry", "loop")]),
                        on_error: Some("error".into()),
                    },
                ),
                ("ok".into(), ret("ok")),
                ("error".into(), ret("error")),
            ]),
            Registry {
                commands: BTreeMap::from([(c.name.clone(), c)]),
                events: BTreeMap::new(),
            },
            BTreeMap::new(),
        );
        install(&e, a, "iteration");
        let id = e
            .admit("iteration", "main", Value::Null, authority(), 0)
            .unwrap();
        let mut host = FakeHost {
            responses: BTreeMap::from([(
                "test.command".into(),
                vec![
                    DispatchResult::Rejected {
                        reason: "denied".into(),
                    },
                    DispatchResult::Rejected {
                        reason: "denied".into(),
                    },
                ],
            )]),
            ..Default::default()
        };
        e.drive(&mut host, 0, 50).unwrap();
        let state = e.snapshot().unwrap();
        assert_eq!(state.runs[&id].outcome.as_deref(), Some(expected));
        assert_eq!(host.dispatched.len(), calls);
        assert!(state.runs[&id].loops.is_empty());
        assert!(!state.runs[&id].variables.contains_key("item"));
    }
}

#[test]
fn losing_namespace_ownership_prevents_reconciliation_handoff() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Reconcilable), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.release().unwrap();
    let _replacement = Engine::new(e.store.clone(), "test", "replacement").unwrap();
    assert!(matches!(
        e.recover(&mut host, 1),
        Err(Error::Storage(StorageError::Ownership))
    ));
    assert!(host.reconciliations.is_empty());
}

#[test]
fn command_deadline_refuses_unissued_effect_and_preserves_issued_uncertainty() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let first = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&first, 0).unwrap();
    let prepared = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(prepared.deadline_at_ms, 100);
    let mut host = FakeHost::default();
    e.dispatch_next(&mut host, 100).unwrap();
    assert!(host.dispatched.is_empty());
    e.drive(&mut host, 100, 10).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&first].outcome.as_deref(),
        Some("error")
    );
    let second = e
        .admit("invoke", "main", Value::Null, authority(), 200)
        .unwrap();
    e.drive(&mut host, 200, 10).unwrap();
    assert_eq!(host.dispatched.len(), 1);
    e.recover(&mut host, 301).unwrap();
    e.drive(&mut host, 301, 10).unwrap();
    let state = e.snapshot().unwrap();
    assert_eq!(host.dispatched.len(), 1);
    assert_eq!(state.runs[&second].status, RunStatus::Blocked);
    assert!(
        state
            .intents
            .values()
            .any(|i| i.status == IntentStatus::Unknown)
    );
}

#[derive(Default)]
struct FailOnceStore {
    inner: MemoryStorage,
    fail: std::sync::atomic::AtomicBool,
}
impl Storage for FailOnceStore {
    fn load(&self, ns: &str) -> std::result::Result<VersionedState, StorageError> {
        self.inner.load(ns)
    }
    fn acquire(&self, ns: &str, owner: &str) -> std::result::Result<(), StorageError> {
        self.inner.acquire(ns, owner)
    }
    fn release(&self, ns: &str, owner: &str) -> std::result::Result<(), StorageError> {
        self.inner.release(ns, owner)
    }
    fn compare_and_swap(
        &self,
        ns: &str,
        owner: &str,
        revision: u64,
        state: &State,
    ) -> std::result::Result<u64, StorageError> {
        if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Conflict);
        }
        self.inner.compare_and_swap(ns, owner, revision, state)
    }
}
#[test]
fn failed_transition_commit_has_no_partial_intent_and_never_dispatches() {
    let e = Engine::new(FailOnceStore::default(), "atomic", "owner").unwrap();
    let artifact = invoke_artifact(command(Recovery::KeyedIdempotent), None);
    let digest = artifact.digest.clone();
    e.install(artifact).unwrap();
    e.activate("invoke", &digest, authority()).unwrap();
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let before = e.snapshot().unwrap();
    e.store
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        e.advance(&id, 0),
        Err(Error::Storage(StorageError::Conflict))
    ));
    let after = e.snapshot().unwrap();
    assert!(after.intents.is_empty());
    assert_eq!(after.receipts.len(), before.receipts.len());
    assert_eq!(after.runs[&id].visits, 0);
    e.advance(&id, 0).unwrap();
    let intent = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .id
        .clone();
    let mut host = FakeHost::default();
    e.store
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(
        e.dispatch_next(&mut host, 0),
        Err(Error::Storage(StorageError::Conflict))
    ));
    assert!(host.dispatched.is_empty());
    assert_eq!(
        e.snapshot().unwrap().intents[&intent].status,
        IntentStatus::Prepared
    );
}

#[test]
fn retry_allowances_are_scoped_to_full_loop_invocation_paths() {
    let e = engine();
    let mut content = invoke_artifact(
        command(Recovery::KeyedIdempotent),
        Some(RetryPolicy {
            max_attempts: 2,
            backoff_ms: 10,
            outcomes: set(&["retry"]),
        }),
    )
    .content;
    content.playbook.entries.get_mut("main").unwrap().start = "loop".into();
    content
        .playbook
        .steps
        .get_mut("invoke")
        .unwrap()
        .edges
        .insert("ok".into(), "loop".into());
    content.playbook.steps.insert(
        "loop".into(),
        Step {
            op: Operation::ForEach {
                collection: Expr::literal(json!([null, null])),
                item: "item".into(),
                body: "invoke".into(),
                next: "ok".into(),
                max_items: 2,
                concurrency: 1,
                on_failure: IterationFailure::Stop,
            },
            edges: BTreeMap::new(),
            on_error: Some("error".into()),
        },
    );
    install(&e, Artifact::seal(content).unwrap(), "invoke");
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            ["retry", "ok", "retry", "ok"]
                .into_iter()
                .map(|outcome| DispatchResult::Completed {
                    outcome: outcome.into(),
                    output: Value::Null,
                })
                .collect(),
        )]),
        ..Default::default()
    };
    for now in [0, 10, 20] {
        e.drive(&mut host, now, 50).unwrap();
    }
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&id].outcome.as_deref(), Some("ok"));
    assert_eq!(host.dispatched.len(), 4);
    assert_eq!(state.runs[&id].attempts.len(), 2);
    assert!(state.runs[&id].attempts.values().all(|count| *count == 2));
    assert_eq!(state.budgets[&id].iterations, 2);
}

#[test]
fn bounded_driver_uses_persisted_fair_scope_queue_and_noop_does_not_commit() {
    let store = MemoryStorage::default();
    let e = Engine::new(store.clone(), "fair", "one").unwrap();
    for scope in ["alpha", "beta"] {
        let mut a = artifact(
            scope,
            "ok",
            BTreeMap::from([("ok".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        )
        .content;
        a.playbook.scope = scope.into();
        a.source.header.scope = scope.into();
        let a = Artifact::seal(a).unwrap();
        let hash = a.digest.clone();
        let mut auth = authority();
        auth.scope = scope.into();
        e.install(a).unwrap();
        e.activate(scope, &hash, auth.clone()).unwrap();
        for _ in 0..3 {
            e.admit(scope, "main", Value::Null, auth.clone(), 0)
                .unwrap();
        }
    }
    let mut host = FakeHost::default();
    assert_eq!(e.drive(&mut host, 5, 2).unwrap(), 2);
    let metrics = e.metrics.snapshot();
    assert_eq!(metrics.queue_latency.count, 2);
    assert_eq!(metrics.queue_latency.total_us, 10_000);
    assert_eq!(metrics.queue_latency.max_us, 5_000);
    let s = e.snapshot().unwrap();
    let terminal_scopes = s
        .runs
        .values()
        .filter(|r| r.status.terminal())
        .map(|r| r.authority.scope.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(terminal_scopes, set(&["alpha", "beta"]));
    assert!(s.indexes.ready_tokens.len() == 4);
    e.release().unwrap();
    let resumed = Engine::new(store.clone(), "fair", "two").unwrap();
    resumed.drive(&mut host, 5, 10).unwrap();
    let revision = store.load("fair").unwrap().revision;
    assert_eq!(resumed.drive(&mut host, 5, 10).unwrap(), 0);
    assert_eq!(store.load("fair").unwrap().revision, revision);
}

#[test]
fn activation_controls_separate_calls_events_and_current_execution() {
    let e = engine();
    let mut a = wait_artifact().content;
    let c = event_contract();
    a.playbook.triggers.push(Trigger {
        id: "start".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    install(&e, Artifact::seal(a).unwrap(), "waiter");
    e.set_activation_controls(
        "waiter",
        ActivationControls {
            calls_enabled: false,
            event_starts_enabled: true,
            execution_enabled: true,
        },
        0,
    )
    .unwrap();
    assert!(
        e.admit("waiter", "main", Value::Null, authority(), 0)
            .is_err()
    );
    let event = e.admit_event("host", event("trigger", 2), 0).unwrap();
    assert_eq!(event.destinations, 1);
    e.set_activation_controls(
        "waiter",
        ActivationControls {
            calls_enabled: true,
            event_starts_enabled: false,
            execution_enabled: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(
        e.admit_event("host", self::event("disabled", 2), 1)
            .unwrap()
            .destinations,
        0
    );
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 1)
        .unwrap();
    e.advance(&id, 1).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.set_activation_controls("waiter", ActivationControls::default(), 2)
        .unwrap();
    e.drive(&mut FakeHost::default(), 2, 10).unwrap();
    assert_ne!(
        e.snapshot().unwrap().runs[&id].reason.as_deref(),
        Some("execution authority revoked")
    );
}

#[test]
fn gc_preserves_bound_transitive_closure_then_collects_terminal_cohorts_in_pages() {
    let e = engine();
    let leaf = artifact(
        "leaf",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    );
    let middle = parent_artifact(&leaf, CallMode::Wait);
    let mut root = parent_artifact(&middle, CallMode::Wait).content;
    root.playbook.id = "root".into();
    root.source.header.id = "root".into();
    let root = Artifact::seal(root).unwrap();
    e.install(leaf.clone()).unwrap();
    e.install(middle.clone()).unwrap();
    install(&e, root, "root");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    assert_eq!(e.collect_garbage(100, 10).unwrap().artifacts, 0);
    let id = e
        .admit("root", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 50).unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 3);
    for _ in 0..3 {
        e.collect_garbage(100, 1).unwrap();
    }
    let state = e.snapshot().unwrap();
    assert!(state.runs.is_empty());
    assert!(!state.budgets.contains_key(&id));
    assert!(state.artifacts.contains_key(&leaf.digest));
    assert!(state.artifacts.contains_key(&middle.digest));
}

#[test]
fn gc_tombstones_expired_event_identity_and_epoch_fence_compacts_safely() {
    let e = engine();
    e.install(wait_artifact()).unwrap();
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
    })
    .unwrap();
    let envelope = event("expired", 2);
    let disposition = e.admit_event("host", envelope.clone(), 0).unwrap();
    assert_eq!(e.collect_garbage(2000, 10).unwrap().events, 1);
    assert!(
        e.snapshot()
            .unwrap()
            .retention
            .event_tombstones
            .contains_key(&disposition.key)
    );
    assert!(e.admit_event("host", envelope.clone(), 2001).is_err());
    e.advance_producer_epoch(
        "host",
        1,
        "operator",
        "producer retired all earlier deliveries",
        2002,
    )
    .unwrap();
    assert!(e.snapshot().unwrap().retention.event_tombstones.is_empty());
    assert!(e.admit_event("host", envelope, 2003).is_err());
}

#[test]
fn unsafe_recovery_requires_audited_request_and_preserves_original_authority() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let intent = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .clone();
    let request = e
        .request_recovery(
            &intent.id,
            "operator",
            "authoritative status available",
            vec!["evidence:1".into()],
            1,
        )
        .unwrap();
    assert_eq!(
        request,
        e.request_recovery(
            &intent.id,
            "operator",
            "authoritative status available",
            vec!["evidence:1".into()],
            2
        )
        .unwrap()
    );
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Completed {
            outcome: "ok".into(),
            output: Value::Null,
        }],
    );
    e.recover(&mut host, 2).unwrap();
    e.drive(&mut host, 2, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].outcome.as_deref(), Some("ok"));
    assert_eq!(s.intents[&intent.id].authority, authority());
    assert!(s.recovery_requests[&request].consumed);
}

#[test]
fn pruned_general_wait_history_refuses_registration_and_captured_replay_horizon_survives_policy_change()
 {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    let envelope = event("history", 2);
    e.admit_event("host", envelope.clone(), 0).unwrap();
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    assert!(e.admit_event("host", envelope, 1001).is_err());
    for _ in 0..10 {
        if !e.collect_garbage(2000, 1).unwrap().more {
            break;
        }
    }
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 2001)
        .unwrap();
    e.drive(&mut FakeHost::default(), 2001, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].outcome.as_deref(), Some("error"));
    assert!(s.waits.is_empty());
    assert!(s.receipts.iter().any(|r| r.run == id
        && r.kind == "error"
        && r.detail.to_string().contains("history expired")));
}

struct CrashOnCancel {
    host: FakeHost,
    after_send: bool,
    keys: Vec<String>,
}
impl Host for CrashOnCancel {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        self.host.authorize(a, c)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.host.dispatch(i)
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.host.reconcile(i)
    }
    fn cancel(&mut self, i: &Intent) -> CancellationResult {
        self.keys.push(cancellation_key(i).unwrap());
        if self.after_send {
            self.host.cancel(i);
        }
        panic!("simulated runtime exit before cancellation receipt")
    }
}
#[test]
fn cancellation_dispatch_crashes_reconcile_owner_before_same_key_delivery_retry() {
    for after_send in [false, true] {
        let e = engine();
        let (a, _) = async_artifact();
        install(&e, a, "async");
        let id = e
            .admit("async", "main", Value::Null, authority(), 0)
            .unwrap();
        let mut host = CrashOnCancel {
            host: FakeHost {
                responses: BTreeMap::from([(
                    "test.command".into(),
                    vec![DispatchResult::Accepted {
                        handle: "server-operation".into(),
                    }],
                )]),
                ..Default::default()
            },
            after_send,
            keys: vec![],
        };
        e.drive(&mut host, 0, 10).unwrap();
        e.cancel(&id, 1).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || e.dispatch_next(&mut host, 1)
            ))
            .is_err()
        );
        let state = e.snapshot().unwrap();
        let intent = state.intents.values().next().unwrap().clone();
        assert!(intent.cancel_dispatched);
        assert_eq!(state.runs[&id].status, RunStatus::Cancelling);
        assert_eq!(host.host.dispatched.len(), 1);
        host.host.responses.insert(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        );
        e.recover(&mut host.host, 2).unwrap();
        assert_eq!(host.host.reconciliations.len(), 1);
        assert!(!e.snapshot().unwrap().intents[&intent.id].cancel_dispatched);
        host.host
            .cancellation_responses
            .insert("test.command".into(), vec![CancellationResult::Confirmed]);
        e.dispatch_next(&mut host.host, 2).unwrap();
        e.drive(&mut host.host, 2, 10).unwrap();
        let state = e.snapshot().unwrap();
        assert_eq!(state.runs[&id].status, RunStatus::Cancelled);
        assert_eq!(host.host.dispatched.len(), 1);
        assert_eq!(
            host.keys,
            vec![cancellation_key(&state.intents[&intent.id]).unwrap()]
        );
    }
}

#[test]
fn gc_preserves_possible_async_completion_while_lost_admission_handle_is_unknown() {
    let e = engine();
    let (a, event_contract) = async_artifact();
    install(&e, a, "async");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let event = e
        .admit_event(
            "host",
            async_event(&event_contract, "lost-admission-completion"),
            1,
        )
        .unwrap();
    for _ in 0..10 {
        if !e.collect_garbage(4000, 1).unwrap().more {
            break;
        }
    }
    assert!(e.snapshot().unwrap().events.contains_key(&event.key));
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 4001).unwrap();
    e.drive(&mut host, 4001, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("ok")
    );
}

#[test]
fn producer_epoch_compaction_keeps_history_fences_for_existing_invocations() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.admit_event("host", event("before-delayed-wait", 2), 0)
        .unwrap();
    for _ in 0..10 {
        if !e.collect_garbage(2000, 1).unwrap().more {
            break;
        }
    }
    e.advance_producer_epoch("host", 1, "operator", "delivery horizon retired", 2000)
        .unwrap();
    assert!(
        !e.snapshot()
            .unwrap()
            .retention
            .expired_correlations
            .is_empty()
    );
    e.drive(&mut FakeHost::default(), 2001, 10).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("error")
    );
}

fn secret_input_artifact() -> Artifact {
    let mut c = command(Recovery::KeyedIdempotent);
    c.input = Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))]));
    c.sensitive_input_fields = set(&["secret"]);
    let input_schema = c.input.clone();
    let mut content = invoke_artifact(c, None).content;
    content.playbook.entries.get_mut("main").unwrap().input = input_schema;
    if let Operation::Invoke { input, .. } =
        &mut content.playbook.steps.get_mut("invoke").unwrap().op
    {
        *input = Expr::reference("input");
    }
    Artifact::seal(content).unwrap()
}
#[test]
fn protected_root_and_transitive_inputs_are_refused_before_any_snapshot_write() {
    let e = engine();
    let child = secret_input_artifact();
    install(&e, child.clone(), "secret");
    let before = e.store.load(&e.namespace).unwrap();
    let secret = json!({"secret":"never-persist-this-root-value"});
    assert!(matches!(
        e.admit("secret", "main", secret.clone(), authority(), 0),
        Err(Error::Refused(_))
    ));
    let after = e.store.load(&e.namespace).unwrap();
    assert_eq!(before.revision, after.revision);
    assert!(after.state.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&after.state).unwrap())
            .unwrap()
            .contains("never-persist-this-root-value")
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    parent.playbook.entries.get_mut("main").unwrap().input =
        child.content.playbook.entries["main"].input.clone();
    if let Operation::Call { input, .. } = &mut parent.playbook.steps.get_mut("call").unwrap().op {
        *input = Expr::reference("input");
    }
    let parent = Artifact::seal(parent).unwrap();
    install(&e, parent, "parent");
    let before = e.store.load(&e.namespace).unwrap().revision;
    assert!(matches!(
        e.admit("parent", "main", secret, authority(), 1),
        Err(Error::Refused(_))
    ));
    let state = e.store.load(&e.namespace).unwrap();
    assert_eq!(before, state.revision);
    assert!(state.state.runs.is_empty());
}
#[test]
fn protected_event_fields_and_protected_trigger_closures_refuse_payload_persistence() {
    let e = engine();
    let mut c = event_contract();
    c.payload = Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))]));
    c.sensitive_payload_fields = set(&["secret"]);
    let a = artifact(
        "event-registry",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(c.name.clone(), c.clone())]),
        },
        BTreeMap::new(),
    );
    e.install(a).unwrap();
    let mut envelope = event("protected-event", 2);
    envelope.contract.fingerprint = c.fingerprint().unwrap();
    envelope.payload = json!({"secret":"never-persist-this-event-value"});
    let before = e.store.load(&e.namespace).unwrap().revision;
    assert!(matches!(
        e.admit_event("host", envelope.clone(), 0),
        Err(Error::Refused(_))
    ));
    assert_eq!(before, e.store.load(&e.namespace).unwrap().revision);
    assert!(e.snapshot().unwrap().events.is_empty());
    // Even when the event itself is public, a subscribed workflow's protected
    // pinned closure prevents its mapped root input from being stored in plaintext.
    let e = engine();
    let mut target = secret_input_artifact().content;
    let mut public_event = c.clone();
    public_event.sensitive_payload_fields.clear();
    target
        .registry
        .events
        .insert(public_event.name.clone(), public_event.clone());
    target.playbook.triggers.push(Trigger {
        id: "secret-trigger".into(),
        event: ContractRef {
            name: public_event.name.clone(),
            fingerprint: public_event.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::reference("event"),
    });
    install(&e, Artifact::seal(target).unwrap(), "target");
    envelope.contract.fingerprint = public_event.fingerprint().unwrap();
    assert!(matches!(
        e.admit_event("host", envelope, 0),
        Err(Error::Refused(_))
    ));
    let state = e.snapshot().unwrap();
    assert!(state.events.is_empty());
    assert!(state.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&state).unwrap())
            .unwrap()
            .contains("never-persist-this-event-value")
    );
}
