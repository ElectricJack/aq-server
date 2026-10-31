//! Cross-boundary regression tests added during independent implementation review.
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn authority() -> Authority {
    Authority {
        actor: "reviewer".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
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
fn artifact(id: &str, start: &str, steps: BTreeMap<String, Step>) -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "review-tests".into(),
        source: SourceBinding {
            digest: hash_bytes(id.as_bytes()),
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
            purpose: "Independent regressions".into(),
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
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn reseal(a: &mut Artifact) {
    *a = Artifact::seal(a.content.clone()).unwrap();
}
fn dependency(a: &Artifact) -> CallableDependency {
    CallableDependency {
        selection: DependencySelection::Exact,
        binding: a.content.playbook.id.clone(),
        artifact: a.digest.clone(),
        interfaces: a.content.playbook.entries.clone(),
        capabilities: a.content.playbook.required_capabilities.clone(),
        scope: "project".into(),
        dependencies: a
            .content
            .dependencies
            .values()
            .flat_map(|d| std::iter::once(d.artifact.clone()).chain(d.dependencies.iter().cloned()))
            .collect(),
    }
}
fn call(dep: &str, mode: CallMode, bind: &str, next: &str) -> Step {
    Step {
        op: Operation::Call {
            dependency: dep.into(),
            entry: "main".into(),
            input: Expr::literal(Value::Null),
            mode: mode.clone(),
            bind: bind.into(),
        },
        edges: if mode == CallMode::Wait {
            edges(&[("ok", next), ("error", "error")])
        } else {
            edges(&[("started", next)])
        },
        on_error: Some("error".into()),
    }
}
fn caller(id: &str, child: &Artifact) -> Artifact {
    let mut a = artifact(
        id,
        "call",
        BTreeMap::from([
            (
                "call".into(),
                call("child", CallMode::Wait, "child_result", "ok"),
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content
        .dependencies
        .insert("child".into(), dependency(child));
    reseal(&mut a);
    a
}
fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::default(), "review", "owner-one").unwrap()
}
fn install(e: &Engine<MemoryStorage>, a: &Artifact, binding: &str) {
    e.install(a.clone()).unwrap();
    e.activate(binding, &a.digest, authority()).unwrap();
}
fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "review.signal".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["invocation"]),
        subject_field: Some("subject".into()),
        revision_field: Some("revision".into()),
        max_payload_bytes: 1024,
        retention_ms: 10_000,
    }
}
fn envelope(id: &str) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), "one".into())]),
        subject: Some("subject-one".into()),
        revision: Some(1),
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: Value::Null,
    }
}
fn event_artifact(id: &str) -> Artifact {
    let mut a = artifact(id, "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c.clone());
    a.content.playbook.triggers.push(Trigger {
        id: "on-signal".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    reseal(&mut a);
    a
}
#[test]
fn t4_transitive_dependency_bytes_stay_pinned_after_activation_and_restart() {
    let e = engine();
    let old = artifact("leaf", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let mut new = old.clone();
    new.content.playbook.purpose = "edited source, newly activated".into();
    reseal(&mut new);
    let middle = caller("middle", &old);
    let root = caller("root", &middle);
    for a in [&old, &middle, &root] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("root", "main", Value::Null, authority(), 0)
        .unwrap();
    e.install(new.clone()).unwrap();
    e.activate("leaf", &new.digest, authority()).unwrap();
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-two").unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 1, 50).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Completed);
    assert!(s.runs.values().any(|r| r.artifact == old.digest));
    assert!(!s.runs.values().any(|r| r.artifact == new.digest));
    assert!(s.runs[&id].dependencies.values().any(|d| d == &old.digest));
}
#[test]
fn t6_partial_fanout_restarts_with_frozen_destinations_and_no_duplicate_runs() {
    let e = engine();
    let old = event_artifact("subscribed");
    install(&e, &old, "first");
    install(&e, &old, "second");
    let accepted = e.admit_event("host", envelope("event"), 0).unwrap();
    assert_eq!(accepted.destinations, 2);
    assert_eq!(e.fanout(&accepted.key, 1, 1).unwrap(), 1);
    let mut new = old.clone();
    new.content.playbook.purpose = "new activation".into();
    reseal(&mut new);
    install(&e, &new, "second");
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-two").unwrap();
    assert_eq!(e.fanout(&accepted.key, 1, 2).unwrap(), 1);
    assert_eq!(e.fanout(&accepted.key, 10, 3).unwrap(), 0);
    assert!(
        e.admit_event("host", envelope("event"), 4)
            .unwrap()
            .duplicate
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs.values().all(|r| r.artifact == old.digest));
}
fn joined_parent(children: &[Artifact], mode: JoinMode, remaining: RemainingPolicy) -> Artifact {
    let mut a = artifact(
        "joining",
        "first",
        BTreeMap::from([
            (
                "first".into(),
                call("one", CallMode::OwnedAsync, "one_handle", "second"),
            ),
            (
                "second".into(),
                call("two", CallMode::OwnedAsync, "two_handle", "join"),
            ),
            (
                "join".into(),
                Step {
                    op: Operation::Join {
                        member_keys: vec![],
                        members: vec![Expr::reference("one_handle"), Expr::reference("two_handle")],
                        mode,
                        accepted_outcomes: set(&["ok"]),
                        deadline_ms: 100,
                        remaining,
                        bind: "joined".into(),
                    },
                    edges: edges(&[
                        ("completed", "ok"),
                        ("no_match", "error"),
                        ("timeout", "error"),
                    ]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    for (key, child) in ["one", "two"].into_iter().zip(children) {
        a.content.dependencies.insert(key.into(), dependency(child));
    }
    reseal(&mut a);
    a
}
#[test]
fn t12_any_join_uses_recorded_acceptance_order_and_tracks_unselected_children() {
    let e = engine();
    let one = artifact("one", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let two = artifact("two", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::AnyAccepted,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    let s = e.snapshot().unwrap();
    let first = s.runs[&id].children[0].clone();
    let second = s.runs[&id].children[1].clone();
    e.advance(&second, 10).unwrap();
    e.advance(&first, 20).unwrap();
    e.advance(&id, 30).unwrap();
    let s = e.snapshot().unwrap();
    let joined = s.runs[&id].variables["joined"].as_object().unwrap();
    assert_eq!(joined.keys().collect::<Vec<_>>(), vec![&second]);
    assert_eq!(s.runs[&first].status, RunStatus::Completed);
}
#[test]
fn t13_independent_calls_consume_origin_budget_without_becoming_owned() {
    let e = engine();
    let child = artifact(
        "independent",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut parent = artifact(
        "limited",
        "first",
        BTreeMap::from([
            (
                "first".into(),
                call("child", CallMode::Independent, "first_handle", "second"),
            ),
            (
                "second".into(),
                call("child", CallMode::Independent, "second_handle", "ok"),
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    parent.content.playbook.limits.max_children = 1;
    parent
        .content
        .dependencies
        .insert("child".into(), dependency(&child));
    reseal(&mut parent);
    install(&e, &child, "independent");
    install(&e, &parent, "limited");
    let id = e
        .admit("limited", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert_eq!(s.budgets[&id].children, 1);
    assert_eq!(s.runs[&id].status, RunStatus::Failed);
    let child = s.runs.values().find(|r| r.id != id).unwrap();
    assert_eq!(child.parent, None);
    assert_eq!(child.root, id);
    assert_eq!(child.status, RunStatus::Admitted);
}
#[test]
fn t17_quota_refusal_retains_prior_event_artifacts_and_destinations() {
    let e = Engine::with_config(
        MemoryStorage::default(),
        "review",
        "owner-one",
        EngineConfig {
            max_events: 1,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    let a = event_artifact("retained");
    install(&e, &a, "retained");
    let first = e.admit_event("host", envelope("first"), 0).unwrap();
    let before = canonical_bytes(&e.snapshot().unwrap()).unwrap();
    assert!(e.admit_event("host", envelope("second"), 1).is_err());
    assert_eq!(canonical_bytes(&e.snapshot().unwrap()).unwrap(), before);
    assert!(e.snapshot().unwrap().artifacts.contains_key(&a.digest));
    assert_eq!(e.fanout(&first.key, 1, 2).unwrap(), 1);
}
fn command(input: Schema, output: Schema) -> CommandContract {
    CommandContract {
        name: "review.command".into(),
        version: "1".into(),
        input,
        outcomes: BTreeMap::from([("ok".into(), output)]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}
fn invoked(c: CommandContract, input: Expr) -> Artifact {
    let mut a = artifact(
        "invoked",
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
                        input,
                        bind: "result".into(),
                        retry: None,
                    },
                    edges: edges(&[("ok", "ok")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.registry.commands.insert(c.name.clone(), c);
    reseal(&mut a);
    a
}
#[test]
fn t18_receipts_default_deny_result_fields_and_refuse_protected_inputs_before_intent() {
    let e = engine();
    let c = command(
        Schema::Null,
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
    );
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    e.admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"must-not-appear-in-receipt"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !String::from_utf8(canonical_bytes(&s.receipts).unwrap())
            .unwrap()
            .contains("must-not-appear")
    );
    let e = engine();
    let mut c = command(
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
        Schema::Null,
    );
    c.sensitive_input_fields = set(&["secret"]);
    let a = invoked(
        c,
        Expr::literal(json!({"secret":"never-persist-as-intent"})),
    );
    install(&e, &a, "invoked");
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty());
    assert!(s.intents.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&s.receipts).unwrap())
            .unwrap()
            .contains("never-persist")
    );
}
#[test]
fn t26_nested_captured_iteration_consumes_shared_budget_and_restores_lexical_roots() {
    let e = engine();
    let mut a = artifact(
        "nested",
        "outer",
        BTreeMap::from([
            (
                "outer".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::literal(json!([[1, 2], [3]])),
                        item: "outer_item".into(),
                        body: "inner".into(),
                        next: "ok".into(),
                        max_items: 2,
                        concurrency: 1,
                        on_failure: IterationFailure::Stop,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "inner".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::reference("outer_item"),
                        item: "inner_item".into(),
                        body: "body".into(),
                        next: "outer".into(),
                        max_items: 2,
                        concurrency: 1,
                        on_failure: IterationFailure::Stop,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "body".into(),
                Step {
                    op: Operation::Decide {
                        predicate: Expr::literal(true),
                    },
                    edges: edges(&[("true", "inner"), ("false", "inner"), ("unknown", "error")]),
                    on_error: None,
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.playbook.limits.max_iterations = 5;
    reseal(&mut a);
    install(&e, &a, "nested");
    let id = e
        .admit("nested", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 50).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].outcome.as_deref(), Some("ok"));
    assert_eq!(s.budgets[&id].iterations, 5);
    assert!(!s.runs[&id].variables.contains_key("inner_item"));
    assert!(!s.runs[&id].variables.contains_key("outer_item"));
}
#[test]
fn ingress_rejects_missing_declared_subject_revision_and_correlation() {
    let e = engine();
    let a = event_artifact("typed-envelope");
    install(&e, &a, "typed-envelope");
    let mut event = envelope("missing-subject");
    event.subject = None;
    assert!(e.admit_event("host", event, 0).is_err());
    let mut event = envelope("missing-correlation");
    event.correlation.clear();
    assert!(e.admit_event("host", event, 0).is_err());
    let mut event = envelope("missing-revision");
    event.revision = None;
    assert!(e.admit_event("host", event, 0).is_err());
}

#[test]
fn aggregate_concurrent_children_include_independent_roots() {
    let e = engine();
    let child = artifact(
        "concurrent-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut parent = artifact(
        "concurrent-parent",
        "first",
        BTreeMap::from([
            (
                "first".into(),
                call("child", CallMode::Independent, "first_handle", "second"),
            ),
            (
                "second".into(),
                call("child", CallMode::Independent, "second_handle", "ok"),
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    parent.content.playbook.limits.max_concurrent_children = 1;
    parent
        .content
        .dependencies
        .insert("child".into(), dependency(&child));
    reseal(&mut parent);
    install(&e, &child, "concurrent-child");
    install(&e, &parent, "concurrent-parent");
    let id = e
        .admit("concurrent-parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert_eq!(s.budgets[&id].children, 1);
    assert_eq!(s.runs[&id].step, "error");
}
#[test]
fn command_payloads_obey_tighter_run_limits_on_both_sides() {
    let e = engine();
    let c = command(Schema::string(128), Schema::Null);
    let mut a = invoked(c, Expr::literal("too-large-for-run-limit"));
    a.content.playbook.limits.max_payload_bytes = 8;
    reseal(&mut a);
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    assert!(e.snapshot().unwrap().intents.is_empty());
    assert_eq!(e.snapshot().unwrap().runs[&id].step, "error");
    let e = engine();
    let c = command(Schema::Null, Schema::string(128));
    let mut a = invoked(c, Expr::literal(Value::Null));
    a.content.playbook.limits.max_payload_bytes = 8;
    reseal(&mut a);
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!("too-large-for-run-limit"),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Blocked);
    assert!(
        s.intents
            .values()
            .all(|i| i.result.is_none() && i.status == IntentStatus::Unknown)
    );
}

#[test]
fn t18_protected_output_contract_is_refused_before_external_dispatch() {
    let e = engine();
    let mut c = command(
        Schema::Null,
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
    );
    c.sensitive_output_fields = set(&["secret"]);
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"must-never-be-requested-without-protection"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    assert!(host.dispatched.is_empty());
    let s = e.snapshot().unwrap();
    assert!(s.intents.is_empty());
    assert!(s.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&s.receipts).unwrap())
            .unwrap()
            .contains("must-never")
    );
}

#[test]
fn named_heterogeneous_join_results_feed_a_typed_command_and_return() {
    let e = engine();
    let evidence = Schema::object(BTreeMap::from([("subject".into(), Schema::string(32))]));
    let mut review = artifact(
        "typed-review",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    review
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), evidence);
    review.content.playbook.steps.get_mut("ok").unwrap().op = Operation::Return {
        outcome: "ok".into(),
        output: Expr::literal(json!({"subject":"candidate-1"})),
    };
    reseal(&mut review);
    let mut check = artifact(
        "typed-check",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    check
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), Schema::Boolean);
    check.content.playbook.steps.get_mut("ok").unwrap().op = Operation::Return {
        outcome: "ok".into(),
        output: Expr::literal(true),
    };
    reseal(&mut check);
    let mut parent = joined_parent(
        &[review.clone(), check.clone()],
        JoinMode::All,
        RemainingPolicy::Track,
    );
    if let Operation::Join { member_keys, .. } =
        &mut parent.content.playbook.steps.get_mut("join").unwrap().op
    {
        *member_keys = vec!["review".into(), "check".into()];
    }
    let result_schema = Schema::object(BTreeMap::from([
        ("subject".into(), Schema::string(32)),
        ("checked".into(), Schema::Boolean),
    ]));
    let c = command(result_schema.clone(), result_schema.clone());
    parent
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), result_schema);
    parent.content.playbook.steps.get_mut("ok").unwrap().op = Operation::Invoke {
        command: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        input: Expr::Object {
            fields: BTreeMap::from([
                (
                    "subject".into(),
                    Expr::Ref {
                        root: "joined".into(),
                        path: vec!["review".into(), "output".into(), "subject".into()],
                    },
                ),
                (
                    "checked".into(),
                    Expr::Ref {
                        root: "joined".into(),
                        path: vec!["check".into(), "output".into()],
                    },
                ),
            ]),
        },
        bind: "published".into(),
        retry: None,
    };
    parent.content.playbook.steps.get_mut("ok").unwrap().edges = edges(&[("ok", "done")]);
    parent
        .content
        .playbook
        .steps
        .get_mut("ok")
        .unwrap()
        .on_error = Some("error".into());
    parent.content.playbook.steps.insert(
        "done".into(),
        Step {
            op: Operation::Return {
                outcome: "ok".into(),
                output: Expr::reference("published"),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    parent.content.registry.commands.insert(c.name.clone(), c);
    reseal(&mut parent);
    for a in [&review, &check, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    let expected = json!({"subject":"candidate-1","checked":true});
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: expected.clone(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 1, 100).unwrap();
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&id].status, RunStatus::Completed);
    assert_eq!(state.runs[&id].output, Some(expected.clone()));
    assert_eq!(host.dispatched.len(), 1);
    let intent = state
        .intents
        .values()
        .find(|intent| intent.run == id)
        .unwrap();
    assert_eq!(intent.input, expected);
    let joined = &state.runs[&id].variables["joined"];
    for key in ["review", "check"] {
        let participant = joined[key]["participant"].as_str().unwrap();
        assert!(state.runs[&id].children.iter().any(|id| id == participant));
    }
}

#[test]
fn catalog_dependencies_select_compatible_versions_once_then_pin_across_restart() {
    let e = engine();
    let old = artifact(
        "catalog-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut new = old.clone();
    new.content.playbook.purpose = "compatible newly activated implementation".into();
    reseal(&mut new);
    let mut parent = caller("catalog-parent", &old);
    parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection = DependencySelection::Catalog {
        allowed_artifacts: BTreeSet::new(),
    };
    reseal(&mut parent);
    install(&e, &old, "catalog-child");
    install(&e, &parent, "catalog-parent");
    let first = e
        .admit("catalog-parent", "main", Value::Null, authority(), 0)
        .unwrap();
    install(&e, &new, "catalog-child");
    let second = e
        .admit("catalog-parent", "main", Value::Null, authority(), 1)
        .unwrap();
    let key = format!("{}/child", parent.digest);
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&first].dependencies[&key], old.digest);
    assert_eq!(state.runs[&second].dependencies[&key], new.digest);
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-after-catalog-restart").unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 2, 100).unwrap();
    let state = e.snapshot().unwrap();
    for (root, expected) in [(&first, &old.digest), (&second, &new.digest)] {
        assert_eq!(state.runs[root].status, RunStatus::Completed);
        let child = state
            .runs
            .values()
            .find(|run| run.parent.as_ref() == Some(root))
            .unwrap();
        assert_eq!(&child.artifact, expected);
    }
}

#[test]
fn catalog_dependencies_reject_incompatible_versions_new_capabilities_and_digest_exclusions() {
    let e = engine();
    let old = artifact(
        "checked-catalog-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut parent = caller("checked-catalog-parent", &old);
    parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection = DependencySelection::Catalog {
        allowed_artifacts: BTreeSet::new(),
    };
    reseal(&mut parent);
    install(&e, &old, "checked-catalog-child");
    install(&e, &parent, "checked-catalog-parent");
    let mut incompatible = old.clone();
    incompatible
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), Schema::string(32));
    incompatible
        .content
        .playbook
        .steps
        .get_mut("ok")
        .unwrap()
        .op = Operation::Return {
        outcome: "ok".into(),
        output: Expr::literal("incompatible result"),
    };
    reseal(&mut incompatible);
    install(&e, &incompatible, "checked-catalog-child");
    assert!(
        e.admit(
            "checked-catalog-parent",
            "main",
            Value::Null,
            authority(),
            0
        )
        .is_err()
    );
    assert!(e.snapshot().unwrap().runs.is_empty());
    let mut privileged = old.clone();
    privileged
        .content
        .playbook
        .required_capabilities
        .insert("additional".into());
    reseal(&mut privileged);
    e.install(privileged.clone()).unwrap();
    let mut grant = authority();
    grant.capabilities.insert("additional".into());
    e.activate("checked-catalog-child", &privileged.digest, grant)
        .unwrap();
    assert!(
        e.admit(
            "checked-catalog-parent",
            "main",
            Value::Null,
            authority(),
            1
        )
        .is_err()
    );
    assert!(e.snapshot().unwrap().runs.is_empty());
    install(&e, &old, "checked-catalog-child");
    if let DependencySelection::Catalog { allowed_artifacts } = &mut parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection
    {
        allowed_artifacts.insert(hash_bytes(b"some-other-approved-version"));
    }
    reseal(&mut parent);
    install(&e, &parent, "checked-catalog-parent");
    assert!(
        e.admit(
            "checked-catalog-parent",
            "main",
            Value::Null,
            authority(),
            2
        )
        .is_err()
    );
    assert!(e.snapshot().unwrap().runs.is_empty());
}

#[test]
fn tiny_driver_batches_fairly_interleave_runnable_steps_and_prepared_effects() {
    let e = engine();
    let busy = artifact(
        "busy",
        "loop",
        BTreeMap::from([
            (
                "loop".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::literal(json!(vec![0; 100])),
                        item: "busy_item".into(),
                        body: "body".into(),
                        next: "ok".into(),
                        max_items: 100,
                        concurrency: 1,
                        on_failure: IterationFailure::Stop,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "body".into(),
                Step {
                    op: Operation::Decide {
                        predicate: Expr::literal(true),
                    },
                    edges: edges(&[("true", "loop"), ("false", "loop"), ("unknown", "error")]),
                    on_error: None,
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    let invoked = invoked(
        command(Schema::Null, Schema::Null),
        Expr::literal(Value::Null),
    );
    install(&e, &busy, "busy");
    install(&e, &invoked, "invoked");
    let busy_id = e
        .admit("busy", "main", Value::Null, authority(), 0)
        .unwrap();
    let effect_id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&effect_id, 0).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    for _ in 0..12 {
        e.drive(&mut host, 0, 1).unwrap();
    }
    assert_eq!(
        host.dispatched.len(),
        1,
        "a busy ready queue must not starve prepared effects under limit one"
    );
    assert!(!e.snapshot().unwrap().runs[&busy_id].status.terminal());
}

#[test]
fn recovery_obeys_current_engine_authority_revocation() {
    let e = engine();
    let mut c = command(Schema::Null, Schema::Null);
    c.recovery = Recovery::Reconcilable;
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.set_execution_authority("reviewer", "project", set(&["execute"]), 1)
        .unwrap();
    host.responses.insert(
        "reconcile:review.command".into(),
        vec![DispatchResult::Completed {
            outcome: "ok".into(),
            output: Value::Null,
        }],
    );
    e.recover(&mut host, 2).unwrap();
    assert!(host.reconciliations.is_empty());
    assert!(
        e.snapshot()
            .unwrap()
            .intents
            .values()
            .all(|intent| intent.status == IntentStatus::Unknown)
    );
}

#[test]
fn unresolved_recovery_rotates_participants_within_one_scope() {
    let e = engine();
    let mut c = command(Schema::Null, Schema::Null);
    c.recovery = Recovery::Reconcilable;
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    for _ in 0..2 {
        e.admit("invoked", "main", Value::Null, authority(), 0)
            .unwrap();
    }
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 20).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 2);
    e.recover(&mut host, 1).unwrap();
    e.recover(&mut host, 2).unwrap();
    assert_eq!(
        host.reconciliations.iter().collect::<BTreeSet<_>>().len(),
        2,
        "one unresolved operation must not starve the next in its scope"
    );
}

#[test]
fn garbage_collection_bounds_preserved_candidate_examination() {
    let e = engine();
    let a = artifact("live", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    install(&e, &a, "live");
    for _ in 0..3 {
        e.admit("live", "main", Value::Null, authority(), 0)
            .unwrap();
    }
    let page = e.collect_garbage(10, 1).unwrap();
    assert!(
        page.preserved <= 1,
        "page limit bounds examined retained candidates as well as disposals"
    );
    assert!(page.more, "bounded scan must report remaining candidates");
}

#[test]
fn garbage_collection_preserves_transitively_bound_artifacts_without_runs() {
    let e = engine();
    let leaf = artifact(
        "retained-leaf",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let middle = caller("retained-middle", &leaf);
    let root = caller("retained-root", &middle);
    let orphan = artifact(
        "unreferenced",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    for a in [&leaf, &middle, &orphan] {
        e.install(a.clone()).unwrap();
    }
    install(&e, &root, "retained-root");
    e.collect_garbage(1_000_000_000, 100).unwrap();
    let s = e.snapshot().unwrap();
    for a in [&leaf, &middle, &root] {
        assert!(s.artifacts.contains_key(&a.digest));
    }
    assert!(!s.artifacts.contains_key(&orphan.digest));
    assert!(s.runs.is_empty());
}

#[test]
fn retired_producer_epochs_need_no_new_tombstones_for_late_gc() {
    let e = engine();
    let mut a = artifact(
        "epoch-catalog",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "epoch-catalog");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
    })
    .unwrap();
    e.admit_event("host", envelope("old-epoch"), 0).unwrap();
    e.advance_producer_epoch(
        "host",
        1,
        "operator",
        "producer changed durable replay epoch",
        1,
    )
    .unwrap();
    e.collect_garbage(20_000, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.events.is_empty());
    assert!(
        s.retention.event_tombstones.is_empty(),
        "retired epoch fence already refuses these replay identities"
    );
    assert!(
        e.admit_event("host", envelope("old-epoch"), 20_001)
            .is_err()
    );
    assert!(
        e.admit_event_in_epoch("host", envelope("explicit-new-epoch"), 1, 20_001)
            .is_ok()
    );
}

#[test]
fn expired_replay_tombstones_and_retired_epochs_refuse_old_identities() {
    let e = engine();
    let mut a = artifact(
        "replay-catalog",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "replay-catalog");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
    })
    .unwrap();
    e.admit_event("host", envelope("original"), 0).unwrap();
    e.collect_garbage(20_000, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.event_tombstones.len(), 1);
    assert!(e.admit_event("host", envelope("original"), 20_001).is_err());
    e.advance_producer_epoch("host", 1, "operator", "new explicit replay epoch", 20_002)
        .unwrap();
    assert!(e.admit_event("host", envelope("original"), 20_003).is_err());
    let replay = e
        .admit_event_in_epoch("host", envelope("original"), 1, 20_003)
        .unwrap();
    assert!(!replay.duplicate);
    assert!(
        e.admit_event_in_epoch("host", envelope("original"), 1, 20_004)
            .unwrap()
            .duplicate
    );
}

#[test]
fn persisted_protection_flag_cannot_override_the_actual_storage_provider() {
    let e = engine();
    let mut c = command(
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(64))])),
        Schema::Null,
    );
    c.sensitive_input_fields = set(&["secret"]);
    let a = invoked(c, Expr::literal(json!({"secret":"synthetic-only"})));
    install(&e, &a, "invoked");
    let mut saved = e.store.load(&e.namespace).unwrap();
    saved.state.protection_available = true;
    e.store
        .compare_and_swap(&e.namespace, &e.owner, saved.revision, &saved.state)
        .unwrap();
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    // The spoofed transient flag remains in the test store's old snapshot;
    // admission used the actual provider=false and committed no input.
    assert!(s.runs.is_empty());
    assert!(s.intents.is_empty());
    assert!(host.dispatched.is_empty());
}

#[test]
fn retained_event_before_wait_is_driven_after_restart_without_manual_wakeup() {
    let e = engine();
    let c = event_contract();
    let mut a = artifact(
        "indexed-wait",
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
                        correlation: BTreeMap::from([("invocation".into(), Expr::literal("one"))]),
                        minimum_revision: Some(Expr::literal(1)),
                        deadline_ms: 100,
                        bind: "observation".into(),
                    },
                    edges: edges(&[("received", "ok"), ("timeout", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "indexed-wait");
    e.admit_event("host", envelope("before-wait"), 10).unwrap();
    let id = e
        .admit("indexed-wait", "main", Value::Null, authority(), 20)
        .unwrap();
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "event-restart").unwrap();
    e.drive(&mut FakeHost::default(), 20, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("ok")
    );
    assert!(
        e.query(&IndexQuery::Ready {
            scope_after: None,
            limit: 10
        })
        .unwrap()
        .items
        .is_empty()
    );
}

#[test]
fn completed_children_before_join_remain_drivable_after_restart() {
    let e = engine();
    let one = artifact(
        "completed-one",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let two = artifact(
        "completed-two",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::All,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    for child in e.snapshot().unwrap().runs[&id].children.clone() {
        e.advance(&child, 1).unwrap();
    }
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "child-restart").unwrap();
    e.drive(&mut FakeHost::default(), 2, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("ok")
    );
}

#[test]
fn completed_cohort_gc_removes_ready_timer_and_dependency_index_residue() {
    let e = engine();
    let one = artifact("gc-one", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let two = artifact("gc-two", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::All,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 50).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Completed);
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
    })
    .unwrap();
    for _ in 0..20 {
        if !e.collect_garbage(100, 1).unwrap().more {
            break;
        }
    }
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty());
    assert!(s.indexes.runs.is_empty());
    assert!(s.indexes.ready_tokens.is_empty());
    assert!(s.indexes.timers.is_empty());
    assert!(
        s.indexes.dependents.is_empty(),
        "deleted members must not leave empty durable dependency keys"
    );
    assert!(s.indexes.waiters.is_empty());
    assert!(s.indexes.run_dependencies.is_empty());
    assert!(s.indexes.run_subscriptions.is_empty());
    assert!(
        e.query(&IndexQuery::Ready {
            scope_after: None,
            limit: 10
        })
        .unwrap()
        .items
        .is_empty()
    );
}

#[test]
fn execution_revocation_blocks_prepared_dispatch_without_discarding_the_intent() {
    let e = engine();
    let a = invoked(
        command(Schema::Null, Schema::Null),
        Expr::literal(Value::Null),
    );
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let original = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .id
        .clone();
    e.set_activation_controls(
        "invoked",
        ActivationControls {
            calls_enabled: true,
            event_starts_enabled: true,
            execution_enabled: false,
        },
        1,
    )
    .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, 1).unwrap();
    assert!(host.dispatched.is_empty());
    assert!(e.snapshot().unwrap().intents.contains_key(&original));
    e.set_activation_controls("invoked", ActivationControls::default(), 2)
        .unwrap();
    e.recover(&mut host, 2).unwrap();
    e.drive(&mut host, 2, 20).unwrap();
    assert_eq!(host.dispatched, vec![original]);
    assert_eq!(
        e.snapshot().unwrap().runs[&id].outcome.as_deref(),
        Some("ok")
    );
}
