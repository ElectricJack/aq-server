//! D10 Task 2: the bounded `transition_v1` path is observably equivalent to the
//! full load/CAS reference path, step by step.
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

const NS: &str = "equivalence";
fn authority(scope: &str) -> Authority {
    Authority {
        actor: "d10".into(),
        scope: scope.into(),
        capabilities: BTreeSet::new(),
    }
}
fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "d10.signal".into(),
        version: "1".into(),
        payload: Schema::string(512),
        trusted_producers: BTreeSet::from(["d10".into()]),
        scopes: BTreeSet::from(["a".into(), "b".into(), "c".into()]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1_000,
    }
}
fn contract_ref() -> ContractRef {
    let c = event_contract();
    ContractRef {
        name: c.name.clone(),
        fingerprint: c.fingerprint().unwrap(),
    }
}
fn event(id: &str, payload: &str) -> EventEnvelope {
    EventEnvelope {
        producer: "d10".into(),
        event_id: id.into(),
        contract: contract_ref(),
        scope: "a".into(),
        correlation: BTreeMap::new(),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: json!(payload),
    }
}
fn command() -> CommandContract {
    CommandContract {
        name: "d10.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: BTreeSet::new(),
        scopes: BTreeSet::from(["a".into()]),
        effect: Effect::Mutation,
        recovery: Recovery::Reconcilable,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 100,
    }
}
fn ret() -> Step {
    Step {
        op: Operation::Return {
            outcome: "ok".into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn artifact(id: &str, scope: &str, mut first: Step) -> Artifact {
    let returns = matches!(&first.op, Operation::Return { .. });
    if !returns {
        first.on_error = Some("return".into());
    }
    let mut steps = BTreeMap::from([("first".into(), first)]);
    if !returns {
        steps.insert("return".into(), ret());
    }
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "d10-equivalence".into(),
        source: SourceBinding {
            digest: hash_bytes(id.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: id.into(),
                scope: scope.into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: id.into(),
            purpose: "transition equivalence".into(),
            scope: scope.into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "first".into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::from([(command().name, command())]),
            events: BTreeMap::from([("d10.signal".into(), event_contract())]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn wait(deadline_ms: u64) -> Step {
    Step {
        op: Operation::Wait {
            event: None,
            correlation: BTreeMap::new(),
            minimum_revision: None,
            deadline_ms,
            bind: "wake".into(),
        },
        edges: BTreeMap::from([("timeout".into(), "return".into())]),
        on_error: None,
    }
}
fn artifacts() -> Vec<(&'static str, Artifact)> {
    let mut out = vec![];
    for scope in ["a", "b", "c"] {
        let binding = match scope {
            "a" => "a",
            "b" => "b",
            _ => "c",
        };
        out.push((binding, artifact(&format!("return-{scope}"), scope, ret())));
    }
    let child = artifact("child-wait", "a", wait(1_000_000));
    out.push(("child", child.clone()));
    let mut parent = artifact(
        "parent",
        "a",
        Step {
            op: Operation::Call {
                dependency: "child".into(),
                entry: "main".into(),
                input: Expr::literal(Value::Null),
                mode: CallMode::OwnedAsync,
                bind: "handle".into(),
            },
            edges: BTreeMap::from([("started".into(), "return".into())]),
            on_error: None,
        },
    );
    parent.content.dependencies.insert(
        "child".into(),
        CallableDependency {
            binding: "child".into(),
            selection: DependencySelection::Exact,
            artifact: child.digest.clone(),
            interfaces: child.content.playbook.entries.clone(),
            capabilities: BTreeSet::new(),
            scope: "a".into(),
            dependencies: BTreeSet::new(),
        },
    );
    out.push(("parent", Artifact::seal(parent.content).unwrap()));
    out.push(("deadline", artifact("deadline-wait", "a", wait(10))));
    let mut triggered = artifact("triggered", "a", ret());
    triggered.content.playbook.triggers.push(Trigger {
        id: "on-signal".into(),
        event: contract_ref(),
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    out.push(("triggered", Artifact::seal(triggered.content).unwrap()));
    let reference = ContractRef {
        name: command().name,
        fingerprint: command().fingerprint().unwrap(),
    };
    out.push((
        "invoke",
        artifact(
            "invoke",
            "a",
            Step {
                op: Operation::Invoke {
                    command: reference,
                    input: Expr::literal(Value::Null),
                    bind: "result".into(),
                    retry: None,
                },
                edges: BTreeMap::from([("ok".into(), "return".into())]),
                on_error: None,
            },
        ),
    ));
    out
}
fn host() -> FakeHost {
    let mut h = FakeHost::default();
    h.responses.insert(
        command().name,
        vec![
            DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            },
            DispatchResult::Accepted {
                handle: "h-1".into(),
            },
            DispatchResult::Unknown {
                reason: "lost".into(),
            },
        ],
    );
    h
}

/// The same operation sequence against the reference (MemoryStorage, full
/// load/CAS) and the record backend; every observable is compared per step.
struct Pair {
    m: Engine<MemoryStorage>,
    r: Engine<RecordStorage>,
    steps: usize,
}
macro_rules! both {
    ($p:expr, |$e:ident| $body:expr) => {{
        let reference = {
            let $e = &$p.m;
            format!("{:?}", $body)
        };
        let candidate = {
            let $e = &$p.r;
            format!("{:?}", $body)
        };
        $p.check(stringify!($body), &reference, &candidate);
        reference
    }};
}
/// Result-only comparison for bulk setup steps; the next `both!` checks state.
macro_rules! quiet {
    ($p:expr, |$e:ident| $body:expr) => {{
        let reference = {
            let $e = &$p.m;
            format!("{:?}", $body)
        };
        let candidate = {
            let $e = &$p.r;
            format!("{:?}", $body)
        };
        assert_eq!(reference, candidate, "result of {}", stringify!($body));
        reference
    }};
}
impl Pair {
    fn new(r: RecordStorage) -> Self {
        let mut p = Self {
            m: Engine::new(MemoryStorage::retaining_settled(), NS, "owner").unwrap(),
            r: Engine::new(r, NS, "owner").unwrap(),
            steps: 0,
        };
        p.check("new", "", "");
        for (binding, a) in artifacts() {
            let scope = a.content.playbook.scope.clone();
            both!(p, |e| e.install(a.clone()));
            both!(p, |e| e.activate_at(
                binding,
                &a.digest,
                authority(&scope),
                0
            ));
        }
        p
    }
    fn check(&mut self, step: &str, reference: &str, candidate: &str) {
        self.steps += 1;
        assert_eq!(
            reference, candidate,
            "step {}: result of {step}",
            self.steps
        );
        let m = self.m.store.load(NS).unwrap();
        let r = self.r.store.load(NS).unwrap();
        assert_eq!(
            m.revision, r.revision,
            "step {}: revision of {step}",
            self.steps
        );
        let (mb, rb) = (
            canonical_bytes(&m.state).unwrap(),
            canonical_bytes(&r.state).unwrap(),
        );
        if mb != rb {
            let (mv, rv) = (
                serde_json::to_value(&m.state).unwrap(),
                serde_json::to_value(&r.state).unwrap(),
            );
            for (k, v) in mv.as_object().unwrap() {
                assert_eq!(
                    Some(v),
                    rv.get(k),
                    "step {}: state.{k} after {step}",
                    self.steps
                );
            }
            panic!("step {}: canonical state differs after {step}", self.steps);
        }
        // Settled cohorts leave both States: the history (and so every
        // inspection page over live state plus history) is the same too.
        for kind in [
            InspectionKind::Runs,
            InspectionKind::Effects,
            InspectionKind::Receipts,
        ] {
            let query = InspectionQuery {
                kind,
                run: None,
                after: None,
                limit: 1000,
            };
            assert_eq!(
                self.m.inspect(&query).unwrap().items,
                self.r.inspect(&query).unwrap().items,
                "step {}: {kind:?} inspection after {step}",
                self.steps
            );
        }
        // The maintained record counts (what a bound container's `len`
        // reads) are those of the complete State.
        if self.r.store.stats.transitions.load(Ordering::Relaxed) > 0 {
            let mut plain = m.state.clone();
            let patch = aq_playbooks_runner::transition::take_patch(&mut plain).unwrap();
            let counts: BTreeMap<&str, usize> = patch
                .changes
                .iter()
                .filter_map(|(path, change)| match change {
                    aq_playbooks_runner::lazy::ContainerChange::Replace(rows)
                        if !rows.is_empty() =>
                    {
                        Some((path.field, rows.len()))
                    }
                    _ => None,
                })
                .collect();
            let maintained: BTreeMap<&str, usize> = self
                .r
                .store
                .accounting(NS)
                .unwrap()
                .fields
                .into_iter()
                .filter(|(_, count)| *count > 0)
                .collect();
            assert_eq!(
                maintained, counts,
                "step {}: maintained record counts after {step}",
                self.steps
            );
        }
        for query in [
            IndexQuery::Ready {
                scope_after: None,
                limit: 128,
            },
            IndexQuery::DueTimers {
                now_ms: i64::MAX,
                limit: 128,
            },
            IndexQuery::Effects {
                kind: EffectQueueKind::Dispatch,
                scope_after: None,
                limit: 128,
            },
        ] {
            assert_eq!(
                format!("{:?}", self.m.query(&query).unwrap()),
                format!("{:?}", self.r.query(&query).unwrap()),
                "step {}: index choice {query:?} after {step}",
                self.steps
            );
        }
    }
    #[track_caller]
    fn first(&self, f: impl Fn(&State) -> Option<String>) -> String {
        f(&self.m.snapshot().unwrap()).expect("fixture item")
    }
}

fn scenario(p: &mut Pair) {
    // History: completed admissions and accepted events.
    for i in 0..12i64 {
        let id = both!(p, |e| e.admit("a", "main", Value::Null, authority("a"), i));
        let id = id
            .trim_start_matches("Ok(\"")
            .trim_end_matches("\")")
            .to_string();
        both!(p, |e| e.advance(&id, i));
        both!(p, |e| e.admit_event(
            "d10",
            event(&format!("history-{i}"), "p"),
            i
        ));
    }
    // Duplicate and changed events.
    both!(p, |e| e.admit_event("d10", event("history-3", "p"), 20));
    both!(p, |e| e.admit_event(
        "d10",
        event("history-3", "changed"),
        20
    ));
    both!(p, |e| e.admit_event("intruder", event("bad", "p"), 20));
    // Pending child.
    let parent = both!(p, |e| e.admit(
        "parent",
        "main",
        Value::Null,
        authority("a"),
        30
    ));
    let parent = parent
        .trim_start_matches("Ok(\"")
        .trim_end_matches("\")")
        .to_string();
    both!(p, |e| e.advance(&parent, 30));
    let child = p.first(|s| s.runs[&parent].children.first().cloned());
    both!(p, |e| e.advance(&child, 30));
    both!(p, |e| e.advance(&parent, 30));
    // Fanout of a triggered event's queued destination.
    both!(p, |e| e.admit_event("d10", event("fan-1", "p"), 40));
    let key = p.first(|s| {
        s.events
            .iter()
            .find(|(_, v)| v.destinations.iter().any(|d| d.run.is_none()))
            .map(|(k, _)| k.clone())
    });
    both!(p, |e| e.fanout(&key, 1, 41));
    both!(p, |e| e.fanout(&key, 1, 42));
    both!(p, |e| e.fanout("missing-event", 1, 42));
    // Cancel.
    let victim = both!(p, |e| e.admit(
        "deadline",
        "main",
        Value::Null,
        authority("a"),
        50
    ));
    let victim = victim
        .trim_start_matches("Ok(\"")
        .trim_end_matches("\")")
        .to_string();
    both!(p, |e| e.cancel(&victim, 51));
    both!(p, |e| e.cancel(&victim, 52));
    // Dispatch, accepted async result, and an unknown outcome.
    for t in 0..3i64 {
        both!(p, |e| e.admit(
            "invoke",
            "main",
            Value::Null,
            authority("a"),
            60 + t
        ));
    }
    both!(p, |e| {
        let mut h = host();
        e.drive(&mut h, 70, 16)
    });
    let accepted = p.first(|s| {
        s.intents
            .iter()
            .find(|(_, i)| format!("{:?}", i.status) != "Completed")
            .map(|(k, _)| k.clone())
    });
    let done = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    both!(p, |e| e.record_result(&accepted, done.clone(), 80));
    both!(p, |e| e.record_result(&accepted, done.clone(), 81));
    // Deadlines around zero: due = admitted + 10.
    for due in [-100i64, -10, -2, -1, 0, 9, 10, 100] {
        let id = both!(p, |e| e.admit(
            "deadline",
            "main",
            Value::Null,
            authority("a"),
            due - 10
        ));
        let id = id
            .trim_start_matches("Ok(\"")
            .trim_end_matches("\")")
            .to_string();
        both!(p, |e| e.advance(&id, due - 10));
    }
    for now in [-101i64, -100, -3, -1, 0, 9, 10, 99, 100] {
        both!(p, |e| e.drive(&mut host(), now, 4));
    }
    // Ready turns: 9/10 and 99/100 queued admissions across scopes.
    for (n, limit) in [(9usize, 10usize), (10, 9), (99, 100), (100, 99)] {
        for i in 0..n {
            let scope = ["a", "b", "c"][i % 3];
            quiet!(p, |e| e.admit(
                scope,
                "main",
                Value::Null,
                authority(scope),
                200
            ));
        }
        both!(p, |e| e.drive(&mut host(), 201, limit));
        both!(p, |e| e.drive(&mut host(), 202, 1_000));
    }
    // Keyed admission: new, replay, changed.
    let revision = p.m.snapshot().unwrap().bindings["a"].revision;
    let request = |input: Value| KeyedAdmissionRequest {
        key: "request-one".into(),
        binding: "a".into(),
        expected_activation_revision: revision,
        entry: "main".into(),
        input,
        authority: authority("a"),
    };
    both!(p, |e| e.admit_once(request(Value::Null), 300));
    both!(p, |e| e.admit_once(request(Value::Null), 301));
    both!(p, |e| e.admit_once(request(json!(1)), 302));
    // Producer epochs and expiry through retention.
    both!(p, |e| e
        .advance_producer_epoch("d10", 1, "operator", "rotate", 400));
    both!(p, |e| e.admit_event_in_epoch(
        "d10",
        event("old-epoch", "p"),
        0,
        401
    ));
    both!(p, |e| e.admit_event_in_epoch(
        "d10",
        event("new-epoch", "p"),
        1,
        402
    ));
    both!(p, |e| e.advance_producer_epoch(
        "d10",
        u64::MAX,
        "operator",
        "max",
        403
    ));
    both!(p, |e| e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 4,
        ..RetentionPolicy::default()
    }));
    for now in [10_000i64, 10_001, 20_000] {
        both!(p, |e| e.collect_garbage(now, 8));
    }
    both!(p, |e| e.admit_event("d10", event("history-3", "p"), 20_001));
    both!(p, |e| e.drive(&mut host(), i64::MAX, 64));
    both!(p, |e| e.collect_garbage(i64::MAX, 1_000));
    both!(p, |e| e.admit(
        "a",
        "main",
        Value::Null,
        authority("a"),
        i64::MAX
    ));
    both!(p, |e| e.admit(
        "a",
        "main",
        Value::Null,
        authority("a"),
        i64::MIN
    ));
    // The clock is pinned at i64::MAX. Rebase it, then a window drop and a
    // deferral (a keyed receipt whose run is still live) on both paths.
    both!(p, |e| e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    }));
    let rebased = both!(p, |e| e.rebase_retention_clock(
        30_000,
        30_000,
        "operator",
        "pinned at i64::MAX",
        30_000
    ));
    assert!(rebased.starts_with("Ok("), "{rebased}");
    let keyed = |binding: &str, key: &str, p: &Pair| KeyedAdmissionRequest {
        key: key.into(),
        binding: binding.into(),
        expected_activation_revision: p.m.snapshot().unwrap().bindings[binding].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority("a"),
    };
    for i in 0..3 {
        let request = keyed("a", &format!("after-rebase-{i}"), p);
        both!(p, |e| e.admit_once(request.clone(), 30_000 + i));
    }
    let held = keyed("child", "held", p);
    both!(p, |e| e.admit_once(held.clone(), 30_003));
    both!(p, |e| e.drive(&mut host(), 30_010, 64));
    let request = keyed("a", "after-window", p);
    both!(p, |e| e.admit_once(request.clone(), 31_500));
    let s = p.m.snapshot().unwrap();
    assert!(
        !s.keyed_admissions.contains_key("after-rebase-0"),
        "dropped"
    );
    assert!(s.keyed_admissions.contains_key("held"), "deferred");
    // No namespace limit refuses anything, on either path (spec 0003, "no
    // limits"): with every deprecated limit at its smallest, root
    // admissions, keyed admissions, an event, a child start and a cancel
    // land.
    for config in [&mut p.m.config, &mut p.r.config] {
        *config = EngineConfig {
            max_runs: 1,
            max_events: 1,
            max_queued_destinations: 1,
            max_state_bytes: 1,
            max_receipts: 1,
        };
    }
    for i in 0..8 {
        let request = keyed("a", &format!("fill-{i}"), p);
        let landed = both!(p, |e| e.admit_once(request.clone(), 31_750));
        assert!(landed.starts_with("Ok("), "{landed}");
    }
    let parent = both!(p, |e| e.admit(
        "parent",
        "main",
        Value::Null,
        authority("a"),
        31_751
    ));
    assert!(parent.starts_with("Ok("), "{parent}");
    let landed = both!(p, |e| e.admit_event_in_epoch(
        "d10",
        event("unlimited", "p"),
        u64::MAX,
        31_752
    ));
    assert!(landed.starts_with("Ok("), "{landed}");
    let landed = both!(p, |e| e.drive(&mut host(), 31_760, 64));
    assert!(landed.starts_with("Ok("), "{landed}");
    let s = p.m.snapshot().unwrap();
    let held_run = s.keyed_admissions["held"].run_id.clone();
    assert!(s.runs.contains_key(&held_run));
    let landed = both!(p, |e| e.cancel(&held_run, 31_802));
    assert!(landed.starts_with("Ok("), "{landed}");
    for config in [&mut p.m.config, &mut p.r.config] {
        *config = EngineConfig::default();
    }
    // Repeat protection is a rolling set (Task 8b R1): lowering
    // `replay_keep` evicts the oldest unheld entries (at most 64 a commit);
    // each finished keyed run and each event then evicts the oldest, on
    // both paths, compared at every step.
    let unheld = p.m.snapshot().unwrap().retention.replay_order.len();
    assert!(unheld > 3, "{unheld}");
    // History is a rolling log too (R2): the history both paths keep
    // (compared at every step) is trimmed to 6 finished runs.
    both!(p, |e| e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        replay_keep: 3,
        history_keep: 6,
        ..RetentionPolicy::default()
    }));
    while p.m.snapshot().unwrap().retention.replay_order.len() > 3 {
        both!(p, |e| e.drive(&mut host(), 31_900, 1));
    }
    for i in 0..5 {
        let request = keyed("a", &format!("rolling-{i}"), p);
        let admitted = both!(p, |e| e.admit_once(request.clone(), 31_910 + i));
        assert!(admitted.starts_with("Ok("), "{admitted}");
        both!(p, |e| e.drive(&mut host(), 31_910 + i, 8));
        both!(p, |e| e.admit_event_in_epoch(
            "d10",
            event(&format!("rolling-{i}"), "p"),
            u64::MAX,
            31_910 + i
        ));
    }
    let s = p.m.snapshot().unwrap();
    assert_eq!(s.retention.replay_order.len(), 3);
    assert!(!s.keyed_admissions.contains_key("rolling-0"), "evicted");
    let runs =
        p.m.inspect(&InspectionQuery {
            kind: InspectionKind::Runs,
            run: None,
            after: None,
            limit: 1000,
        })
        .unwrap()
        .items;
    assert!(runs.len() <= 6 + s.runs.len(), "{}", runs.len());
}

#[test]
fn transition_reference_equivalence() {
    let mut p = Pair::new(RecordStorage::new());
    let stats = p.r.store.stats.clone();
    let before = stats.transitions.load(Ordering::Relaxed);
    scenario(&mut p);
    let transitions = stats.transitions.load(Ordering::Relaxed) - before;
    assert!(transitions > 100, "bounded path exercised: {transitions}");
    // Cohorts settled and left through the bounded path (history compared
    // at every step by `check`).
    assert!(p.m.store.settled_cohorts(NS).len() > 1);
    // A steady-state event admission performs no full load and no full commit.
    let (loads, commits) = (
        stats.full_loads.load(Ordering::Relaxed),
        stats.full_commits.load(Ordering::Relaxed),
    );
    p.r.admit_event_in_epoch("d10", event("steady", "p"), u64::MAX, 30_000)
        .unwrap();
    assert_eq!(stats.full_loads.load(Ordering::Relaxed), loads);
    assert_eq!(stats.full_commits.load(Ordering::Relaxed), commits);
    eprintln!(
        "transitions={transitions} steps={} fallbacks={:?}",
        p.steps,
        stats.fallbacks()
    );
}

#[test]
fn unsupported_transition_preserves_full_state() {
    let mut p = Pair::new(RecordStorage::without_transition());
    assert!(!p.r.store.transition_v1_support(NS).unwrap());
    scenario(&mut p);
    let stats = &p.r.store.stats;
    assert_eq!(stats.transitions.load(Ordering::Relaxed), 0);
    assert!(stats.full_commits.load(Ordering::Relaxed) > 100);
}
