//! Live state only (spec 0003): history written with the commit, inspection
//! over live state plus history.
// The playbook helpers are copied from `semantics.rs` for the engine-level
// tests of later tasks; not every one is used by every test here.
#![allow(dead_code)]
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
fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap()
}
/// One Completed run `id`, its own root, with receipts at `receipts`.
fn synthetic_cohort(id: &str, finished_at_ms: i64, receipts: &[u64]) -> SettledCohort {
    let run = Run {
        catalog_revision: 1,
        id: id.into(),
        root: id.into(),
        parent: None,
        origin: None,
        artifact: "artifact".into(),
        entry: "main".into(),
        dependencies: BTreeMap::new(),
        authority: authority(),
        step: "ok".into(),
        status: RunStatus::Completed,
        input: Value::Null,
        variables: BTreeMap::new(),
        outcome: Some("ok".into()),
        output: Some(Value::Null),
        reason: None,
        admitted_at_ms: 1,
        visits: 1,
        depth: 0,
        children: vec![],
        pending: None,
        loops: vec![],
        attempts: BTreeMap::new(),
        pending_return: None,
        pending_failure: false,
        terminal_sequence: receipts.last().copied(),
    };
    SettledCohort {
        root: id.into(),
        finished_at_ms,
        runs: vec![run],
        intents: vec![],
        receipts: receipts
            .iter()
            .map(|&sequence| Receipt {
                sequence,
                run: id.into(),
                step: "ok".into(),
                at_ms: finished_at_ms,
                kind: "terminal".into(),
                detail: json!({"status": "completed", "outcome": "ok"}),
            })
            .collect(),
    }
}
fn page(
    store: &impl Storage,
    kind: InspectionKind,
    after: Option<String>,
    limit: usize,
) -> InspectionPage {
    store
        .inspect(
            "ns",
            &InspectionQuery {
                kind,
                run: None,
                after,
                limit,
            },
        )
        .unwrap()
}

#[test]
fn memory_history_is_written_with_the_commit_and_merged_into_inspection() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1, 2]));
    let revision = store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let runs = page(&store, InspectionKind::Runs, None, 10);
    assert_eq!(runs.revision, revision);
    assert_eq!(
        runs.items,
        vec![inspection::public_run(&state.settled[0].runs[0])]
    );
    assert_eq!(
        page(&store, InspectionKind::Receipts, None, 10).items.len(),
        2
    );
    assert!(store.load("ns").unwrap().state.settled.is_empty());
    assert!(
        matches!(store.compare_and_swap("ns", "w", revision, &state),
        Err(StorageError::Backend(m)) if m.contains("already recorded"))
    );
}
#[test]
fn conflicting_commit_writes_no_history() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1]));
    assert_eq!(
        store.compare_and_swap("ns", "w", 7, &state),
        Err(StorageError::Conflict)
    );
    assert!(
        page(&store, InspectionKind::Runs, None, 10)
            .items
            .is_empty()
    );
}
#[test]
fn inspection_pages_across_live_and_history_without_gaps() {
    // live receipts at sequences 2 and 4, history receipts at 1, 3, 5
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    for seq in [2, 4] {
        state.receipts.push(Receipt {
            sequence: seq,
            run: "live".into(),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
    }
    state.settled.push(synthetic_cohort("done", 10, &[1, 3, 5]));
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let mut seen = vec![];
    let mut after = None;
    loop {
        let p = page(&store, InspectionKind::Receipts, after, 2);
        seen.extend(p.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        if p.next.is_none() {
            break;
        }
        after = p.next;
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}
#[test]
fn delete_history_before_removes_only_older_cohorts() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let state = State {
        settled: vec![
            synthetic_cohort("old", 10, &[1]),
            synthetic_cohort("new", 20, &[2]),
        ],
        ..State::default()
    };
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    assert_eq!(store.delete_history("ns", "w", 20).unwrap(), 2); // run + receipt of "old"
    let ids: Vec<_> = page(&store, InspectionKind::Runs, None, 10)
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("new")]);
}
#[test]
fn engine_refuses_storage_that_does_not_persist_history() {
    // Forwards load/compare_and_swap/acquire/release only.
    struct NoHistory(MemoryStorage);
    impl Storage for NoHistory {
        fn load(&self, n: &str) -> std::result::Result<VersionedState, StorageError> {
            self.0.load(n)
        }
        fn compare_and_swap(
            &self,
            n: &str,
            o: &str,
            v: u64,
            s: &State,
        ) -> std::result::Result<u64, StorageError> {
            self.0.compare_and_swap(n, o, v, s)
        }
        fn acquire(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.acquire(n, o)
        }
        fn release(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.release(n, o)
        }
    }
    let refused = Engine::new(NoHistory(MemoryStorage::default()), "ns", "w")
        .err()
        .unwrap();
    assert!(matches!(refused, Error::Refused(m) if m.contains("history")));
}
fn refused_as_also_live<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALSO_LIVE)
}
fn live_receipt(sequence: u64) -> Receipt {
    Receipt {
        sequence,
        run: "live".into(),
        step: "s".into(),
        at_ms: 1,
        kind: "admitted".into(),
        detail: json!({}),
    }
}
/// A commit may not leave an item key both live and in history: neither by
/// settling an item the stored State still holds, nor by storing a live item
/// whose key is already in history.
fn refuses_live_and_history_conflicts(store: &impl Storage) {
    store.acquire("ns", "w").unwrap();
    // Settled while still live: a run, and a receipt.
    let mut state = State::default();
    let cohort = synthetic_cohort("a", 10, &[1]);
    state.runs.insert("a".into(), cohort.runs[0].clone());
    state.settled.push(cohort);
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(7));
    state.settled.push(synthetic_cohort("r", 10, &[7]));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    assert!(page(store, InspectionKind::Runs, None, 10).items.is_empty());
    assert!(
        page(store, InspectionKind::Receipts, None, 10)
            .items
            .is_empty()
    );
    // Live again after it is in history: a run, and a receipt.
    let revision = store
        .compare_and_swap(
            "ns",
            "w",
            0,
            &State {
                settled: vec![synthetic_cohort("b", 10, &[2])],
                ..State::default()
            },
        )
        .unwrap();
    let mut state = State::default();
    state
        .runs
        .insert("b".into(), synthetic_cohort("b", 10, &[]).runs[0].clone());
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(2));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    assert_eq!(store.load("ns").unwrap().revision, revision);
}
#[test]
fn memory_storage_refuses_live_and_history_conflicts() {
    refuses_live_and_history_conflicts(&MemoryStorage::default());
}
#[test]
fn record_storage_refuses_live_and_history_conflicts() {
    let store = RecordStorage::new();
    refuses_live_and_history_conflicts(&store);
    // The same through bounded transitions.
    store.acquire("t", "w").unwrap();
    let mut initialized = State::default();
    initialized.indexes.initialized = true;
    let mut revision = store.compare_and_swap("t", "w", 0, &initialized).unwrap();
    let transition = |cohort: Option<SettledCohort>, live: Option<Run>| {
        let (mut cohort, mut live) = (cohort, live);
        let mut plan = |s: &mut State| {
            s.settled.extend(cohort.take());
            if let Some(run) = live.take() {
                s.runs.insert(run.id.clone(), run);
            }
            s.sequence += 1;
            Ok(true)
        };
        store.transition_v1(
            "t",
            "w",
            &mut TransitionRequest {
                operation: "history",
                plan: &mut plan,
            },
        )
    };
    let also_live = |result: Result<TransitionOutcome>| matches!(result, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE);
    let c = synthetic_cohort("c", 10, &[]);
    assert!(also_live(transition(
        Some(c.clone()),
        Some(c.runs[0].clone())
    )));
    assert!(matches!(
        transition(Some(synthetic_cohort("d", 10, &[])), None).unwrap(),
        TransitionOutcome::Committed { .. }
    ));
    revision += 1;
    let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
    assert!(also_live(transition(None, Some(d))));
    assert_eq!(store.load("t").unwrap().revision, revision);
}

// --- Task 4: retention clock, window fields and removal fences ------------

fn receipt(key: &str, accepted_at_ms: i64) -> KeyedAdmissionReceipt {
    KeyedAdmissionReceipt {
        key: key.into(),
        request_digest: format!("digest-{key}"),
        run_id: format!("run-{key}"),
        activation_revision: 1,
        catalog_revision: 1,
        accepted_at_ms,
    }
}
/// A terminal root run `id` with no children.
fn completed_run(id: &str) -> Run {
    synthetic_cohort(id, 1, &[]).runs.remove(0)
}
fn intent(id: &str, run: &str, status: IntentStatus) -> Intent {
    Intent {
        id: id.into(),
        run: run.into(),
        step: "invoke".into(),
        invocation: 1,
        deadline_at_ms: 100,
        command: command(Recovery::NaturallyIdempotent),
        input: Value::Null,
        input_digest: format!("digest-{id}"),
        authority: authority(),
        status,
        cancel_requested: false,
        cancel_dispatched: false,
        handle: None,
        result: None,
        reason: None,
        late_results: vec![],
        completion_event: None,
        dispatch_sequence: None,
    }
}
fn accepted_event(key: &str, accepted_at_ms: i64) -> AcceptedEvent {
    let envelope = event(key, 2);
    AcceptedEvent {
        key: key.into(),
        digest: format!("digest-{key}"),
        envelope,
        accepted_at_ms,
        sequence: 1,
        catalog_revision: 1,
        destinations: vec![],
    }
}
fn refused_with<T>(result: std::result::Result<T, StorageError>, text: &str) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == text)
}

#[test]
fn legacy_retention_state_and_policy_decode_with_defaults() {
    let policy: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(policy.admission_replay_ms, 604_800_000);
    assert_eq!(RetentionPolicy::default().admission_replay_ms, 604_800_000);
    let mut legacy = serde_json::to_value(State::default()).unwrap();
    for f in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        legacy["retention"].as_object_mut().unwrap().remove(f);
    }
    let state: State = serde_json::from_value(legacy).unwrap();
    assert!(state.retention.clock_ms.is_none() && state.retention.expiry.is_empty());
}
/// `tests/fixtures/legacy_state_5046282.json` was written by the engine at
/// 5046282 (before the retention clock): a namespace after a GC, holding a
/// keyed admission receipt, a live run, a live event, event tombstones and
/// disposals. It is the format 2 snapshot (the State's JSON) and the format 3
/// representation (header plus records) of the same State.
#[test]
fn legacy_fixture_from_5046282_decodes_on_format_2_and_format_3() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/legacy_state_5046282.json")).unwrap();
    let snapshot = &fixture["format2_snapshot"];
    for absent in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        assert!(snapshot["retention"].get(absent).is_none(), "{absent}");
    }
    assert!(
        snapshot["retention"]["policy"]
            .get("admission_replay_ms")
            .is_none()
    );
    let f2: State = serde_json::from_value(snapshot.clone()).unwrap();
    let header = fixture["format3_header"].as_str().unwrap().as_bytes();
    let records: Vec<(String, lazy::TreeKey, Vec<u8>)> = fixture["format3_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_str().unwrap().to_string(),
                serde_json::from_value(r[1].clone()).unwrap(),
                r[2].as_str().unwrap().as_bytes().to_vec(),
            )
        })
        .collect();
    let f3 = transition::assemble_state(header, |field| {
        Ok(records
            .iter()
            .filter(|(f, _, _)| f == field)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect())
    })
    .unwrap();
    for state in [&f2, &f3] {
        let r = &state.retention;
        assert!(
            r.clock_ms.is_none() && r.expiry.is_empty() && r.admission_expires_at_ms.is_empty()
        );
        assert_eq!(r.policy.admission_replay_ms, 604_800_000);
        assert_eq!(state.keyed_admissions["legacy-key"].accepted_at_ms, 10);
        // A legacy receipt expires by acceptance plus the default window.
        assert_eq!(
            retention::admission_expiry(state, "legacy-key"),
            Some(10 + 604_800_000)
        );
        let (key, event) = state.events.iter().next().unwrap();
        assert_eq!(
            retention::event_expiry(state, key),
            Some(
                r.event_expires_at_ms
                    .get(key)
                    .copied()
                    .unwrap_or(event.accepted_at_ms + 86_400_000)
            )
        );
        assert!(!r.event_tombstones.is_empty() && !r.disposals.is_empty());
    }
    assert_eq!(
        serde_json::to_value(&f2).unwrap(),
        serde_json::to_value(&f3).unwrap()
    );
    // The decoded State commits on both in-memory backends, and the legacy
    // receipt leaves only once the clock passes its window.
    for store in [
        &MemoryStorage::default() as &dyn Storage,
        &RecordStorage::new(),
    ] {
        store.acquire("legacy", "w").unwrap();
        let r = store.compare_and_swap("legacy", "w", 0, &f2).unwrap();
        let mut next = store.load("legacy").unwrap().state;
        next.keyed_admissions.remove("legacy-key");
        next.retention.clock_ms = Some(10 + 604_800_000 - 1);
        assert!(refused_with(
            store.compare_and_swap("legacy", "w", r, &next),
            retention::REFUSE_EARLY_REMOVAL
        ));
        next.retention.clock_ms = Some(10 + 604_800_000);
        store.compare_and_swap("legacy", "w", r, &next).unwrap();
    }
}
fn removal_fences_hold<S: Storage>(store: S) {
    store.acquire("ns", "w").unwrap();
    let mut s = State::default();
    // Round 1 (I2): a recorded expiry is at least acceptance plus the policy
    // window, so the brief's 1_000 needs a 1_000 window.
    s.retention.policy.admission_replay_ms = 1_000;
    s.retention.clock_ms = Some(500);
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    let r = store.compare_and_swap("ns", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL)
    );
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap("ns", "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL)
    );
    gone.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    let r = store.compare_and_swap("ns", "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK)
    );
}
#[test]
fn memory_storage_removal_fences() {
    removal_fences_hold(MemoryStorage::default())
}
#[test]
fn record_storage_removal_fences() {
    removal_fences_hold(RecordStorage::new())
}
#[test]
fn commits_advance_the_clock_only_with_host_time() {
    let e = engine();
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(5_000));
}
/// Commits without a host time leave the clock where it is.
#[test]
fn commits_without_host_time_do_not_move_the_clock() {
    let e = engine();
    let a = artifact(
        "a",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    );
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate("a", &digest, authority()).unwrap();
    e.set_retention_policy(RetentionPolicy::default()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, None);
    e.admit("a", "main", Value::Null, authority(), 7).unwrap();
    e.activate("b", &digest, authority()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(7));
}
fn audit(sequence: u64, run: &str) -> Receipt {
    Receipt {
        sequence,
        run: run.into(),
        step: "s".into(),
        at_ms: 1,
        kind: "terminal".into(),
        detail: json!({}),
    }
}
fn cohort(run: Run, intents: Vec<Intent>, receipts: Vec<Receipt>) -> SettledCohort {
    SettledCohort {
        root: run.root.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents,
        receipts,
    }
}
/// The window and history state shared by the refusal tests. Policy
/// windows: receipts 2_000, events 3_000; recorded expiries "k" 3_000 and
/// "ev" 4_000; "fallback" and "ev-fallback" have none.
fn fence_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = 2_000;
    s.retention.policy.event_replay_ms = 3_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.runs.insert("run-b".into(), {
        let mut run = completed_run("run-b");
        run.status = RunStatus::Running;
        run
    });
    s.intents.insert(
        "op-a".into(),
        intent("op-a", "run-a", IntentStatus::Completed),
    );
    s.receipts.push(audit(1, "run-a"));
    s.receipts.push(audit(2, "run-b"));
    s.sequence = 2;
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.keyed_admissions
        .insert("fallback".into(), receipt("fallback", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 3_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 4_000);
    s.events
        .insert("ev-fallback".into(), accepted_event("ev-fallback", 0));
    // Round 2 (C1): an installed contract whose window (5_000) is longer
    // than the policy's, and an event of it expiring at 9_000.
    let long = long_artifact();
    s.artifacts.insert(long.digest.clone(), long);
    s.events.insert("ev-long".into(), {
        let mut e = accepted_event("ev-long", 0);
        e.envelope.contract = ContractRef {
            name: long_contract().name,
            fingerprint: long_contract().fingerprint().unwrap(),
        };
        e
    });
    s.retention
        .event_expires_at_ms
        .insert("ev-long".into(), 9_000);
    s
}
fn long_contract() -> EventContract {
    EventContract {
        name: "test.long".into(),
        retention_ms: 5_000,
        ..event_contract()
    }
}
fn long_artifact() -> Artifact {
    let c = long_contract();
    artifact(
        "long",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(c.name.clone(), c)]),
        },
        BTreeMap::new(),
    )
}
fn tombstone(digest: &str) -> retention::EventTombstone {
    retention::EventTombstone {
        producer: "host".into(),
        epoch: 0,
        digest: digest.into(),
        disposed_at_ms: 5,
    }
}
/// An admin licence receipt of `kind` from `from_ms` to `to_ms`.
fn licence_receipt(kind: &str, sequence: u64, from_ms: i64, to_ms: i64) -> Receipt {
    Receipt {
        sequence,
        run: "admin".into(),
        step: "retention".into(),
        at_ms: to_ms,
        kind: kind.into(),
        detail: json!({"from_ms": from_ms, "to_ms": to_ms}),
    }
}
/// That receipt as history written by the commit.
fn licence_cohort(receipts: Vec<Receipt>) -> SettledCohort {
    SettledCohort {
        root: "admin".into(),
        finished_at_ms: 1,
        runs: vec![],
        intents: vec![],
        receipts,
    }
}
/// Write a licence receipt of `kind` as the commit's one new sequence
/// number, as the engine does.
fn license(n: &mut State, kind: &str, from_ms: i64, to_ms: i64) {
    n.sequence += 1;
    n.settled.push(licence_cohort(vec![licence_receipt(
        kind, n.sequence, from_ms, to_ms,
    )]));
}
/// The commit a rebase from 500 to 100 makes on `fence_state()`: clock 100,
/// each expiry capped at 100 plus its own window ("ev-long" by its
/// contract's 5_000), a stale floor.
fn rebased(n: &mut State) {
    n.retention.clock_ms = Some(100);
    n.retention
        .admission_expires_at_ms
        .insert("k".into(), 2_100);
    n.retention.event_expires_at_ms.insert("ev".into(), 3_100);
    n.retention
        .event_expires_at_ms
        .insert("ev-long".into(), 5_100);
    n.retention.stale_floor_ms = Some(50);
    license(n, retention::CLOCK_REBASE, 500, 100);
}
/// Rulings 1 and 2 (round 2): a commit with a licence receipt must be
/// exactly that licence and do nothing else; without one, the clock never
/// decreases and the floor never changes. Cases on `fence_state()` (clock
/// 500; windows 2_000 and 3_000; "k" 3_000, "ev" 4_000, "ev-long" 9_000
/// with a 5_000 contract window), each (label, refusal, change).
fn rebase_refusal_cases() -> Vec<Case> {
    let shortened = retention::REFUSE_EXPIRY;
    let licence = retention::REFUSE_LICENCE;
    let floor = retention::REFUSE_STALE_FLOOR;
    vec![
        (
            "clock lowered without a rebase",
            retention::REFUSE_CLOCK,
            Box::new(|n| n.retention.clock_ms = Some(100)),
        ),
        (
            "clock lowered, rebase receipt left live",
            retention::REFUSE_CLOCK,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                n.sequence += 1;
                n.receipts.push(licence_receipt(
                    retention::CLOCK_REBASE,
                    n.sequence,
                    500,
                    100,
                ));
            }),
        ),
        (
            "clock lowered, rebase from another clock",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 600, 100);
            }),
        ),
        (
            "clock lowered, rebase to another clock",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 500, 200);
            }),
        ),
        (
            "review probe: a receipt from 7 to 500, the clock left at 500, expiries lowered",
            licence,
            Box::new(|n| {
                license(n, retention::CLOCK_REBASE, 7, 500);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_500);
            }),
        ),
        (
            "rebase upwards",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(600);
                license(n, retention::CLOCK_REBASE, 500, 600);
            }),
        ),
        (
            "replayed rebase receipt (not this commit's sequence)",
            licence,
            Box::new(|n| {
                rebased(n);
                let replayed = n.sequence - 1;
                n.settled[0].receipts[0].sequence = replayed;
            }),
        ),
        (
            "rebase that also admits",
            licence,
            Box::new(|n| {
                rebased(n);
                n.sequence += 1;
                n.keyed_admissions.insert("new".into(), receipt("new", 100));
            }),
        ),
        (
            "rebase that also accepts an event",
            licence,
            Box::new(|n| {
                rebased(n);
                n.events
                    .insert("ev-new".into(), accepted_event("ev-new", 100));
            }),
        ),
        (
            "rebase that also settles a run",
            licence,
            Box::new(|n| {
                rebased(n);
                let run = n.runs.remove("run-a").unwrap();
                let op = n.intents.remove("op-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled
                    .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
            }),
        ),
        (
            "two licences in one commit",
            licence,
            Box::new(|n| {
                rebased(n);
                n.settled[0].receipts.push(licence_receipt(
                    retention::STALE_FLOOR_LOWERED,
                    77,
                    50,
                    10,
                ));
            }),
        ),
        (
            "expiry lowered without a rebase",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            }),
        ),
        (
            "expiry lowered with a rebase the clock did not take",
            licence,
            Box::new(|n| {
                license(n, retention::CLOCK_REBASE, 500, 100);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            }),
        ),
        (
            "expiry capped below its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_099);
            }),
        ),
        (
            "expiry lowered, but not to its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_200);
            }),
        ),
        (
            "event expiry capped below its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_099);
            }),
        ),
        (
            "event capped to the policy window, its contract window is longer",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            }),
        ),
        (
            "floor raised without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(50)),
        ),
        (
            "review probe C2: a rebase that also sets both windows to 1 and caps to 101",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.policy.admission_replay_ms = 1;
                n.retention.policy.event_replay_ms = 1;
                n.retention.admission_expires_at_ms.insert("k".into(), 101);
                n.retention.event_expires_at_ms.insert("ev".into(), 101);
            }),
        ),
        (
            "rebase that also changes the policy",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.policy.audit_ms += 1;
            }),
        ),
        (
            "rebase that also installs an artifact",
            licence,
            Box::new(|n| {
                rebased(n);
                let extra = n.artifacts.values().next().unwrap().clone();
                let mut content = extra.content.clone();
                content.compiler = "another".into();
                let a = Artifact::seal(content).unwrap();
                n.artifacts.insert(a.digest.clone(), a);
            }),
        ),
        (
            "rebase that also changes a producer epoch",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.producer_epochs.insert("host".into(), 1);
            }),
        ),
        (
            "rebase that also moves the catalog",
            licence,
            Box::new(|n| {
                rebased(n);
                n.catalog_revision += 1;
            }),
        ),
        (
            "rebase that also changes the catalog history",
            licence,
            Box::new(|n| {
                rebased(n);
                n.catalog_history_at_ms.insert(99, 1);
            }),
        ),
        (
            "rebase that also changes an event's contract to shrink its cap",
            // Task 8b fix round 1: an accepted event's envelope is immutable,
            // so this is refused before its cap is judged.
            retention::REFUSE_EVENT_CHANGE,
            Box::new(|n| {
                rebased(n);
                let mut e = n.events.get("ev-long").unwrap().clone();
                e.envelope.contract.name = "test.other".into();
                n.events.insert("ev-long".into(), e);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            }),
        ),
        (
            "rebase raises the floor past its target",
            floor,
            Box::new(|n| {
                rebased(n);
                n.retention.stale_floor_ms = Some(101);
            }),
        ),
    ]
}
/// After that commit no licence is left, and the floor changes only with
/// its own exact receipt. Changes of the stored state after the rebase
/// (clock 100, floor 50).
fn after_rebase_refusal_cases() -> Vec<Case> {
    let licence = retention::REFUSE_LICENCE;
    let floor = retention::REFUSE_STALE_FLOOR;
    vec![
        (
            "clock lowered again",
            retention::REFUSE_CLOCK,
            Box::new(|n| n.retention.clock_ms = Some(90)),
        ),
        (
            "stale floor lowered without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(40)),
        ),
        (
            "stale floor raised without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(60)),
        ),
        (
            "stale floor removed",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = None),
        ),
        (
            "floor lowered to another value than its receipt",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(30);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
        (
            "floor lowering from another floor",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                license(n, retention::STALE_FLOOR_LOWERED, 60, 40);
            }),
        ),
        (
            "floor lowering that also moves the clock",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention.clock_ms = Some(200);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
        (
            "floor lowering that also changes the policy",
            licence,
            Box::new(|n| {
                n.retention.policy.admission_replay_ms = 1;
                floor_lowered(n);
            }),
        ),
        (
            "floor lowering that also changes an expiry",
            retention::REFUSE_EXPIRY,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 9_000);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
    ]
}
/// The floor lowering the fence accepts after `rebased`: 50 to 40.
fn floor_lowered(n: &mut State) {
    n.retention.stale_floor_ms = Some(40);
    license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
}
fn rebase_fences_hold_on_cas(store: &dyn Storage) {
    let s = fence_state();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("rebase-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, state).unwrap();
        (ns, base)
    };
    let mut failures = vec![];
    for (label, text, f) in rebase_refusal_cases() {
        let (ns, base) = fresh(&s);
        let mut next = s.clone();
        f(&mut next);
        let result = store.compare_and_swap(&ns, "w", base, &next);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
        }
    }
    let mut next = s.clone();
    rebased(&mut next);
    for (label, text, f) in after_rebase_refusal_cases() {
        let (ns, base) = fresh(&s);
        let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut later = store.load(&ns).unwrap().state;
        f(&mut later);
        let result = store.compare_and_swap(&ns, "w", r, &later);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // A rebase, then a floor lowering with its own receipt.
    let (ns, base) = fresh(&s);
    let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
    let mut later = store.load(&ns).unwrap().state;
    floor_lowered(&mut later);
    store.compare_and_swap(&ns, "w", r, &later).unwrap();
    let stored = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (stored.clock_ms, stored.stale_floor_ms),
        (Some(100), Some(40))
    );
}
#[test]
fn memory_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&MemoryStorage::default());
    let store = MemoryStorage::default();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    let r = store.load("ok").unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.admission_expires_at_ms.get("k").copied()
        ),
        (Some(100), Some(50), Some(2_100))
    );
}
#[test]
fn record_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&RecordStorage::new());
    let store = RecordStorage::new();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    assert_eq!(
        store.load("ok").unwrap().state.retention.clock_ms,
        Some(100)
    );
}
/// The fence stays closed for every removal the rule does not allow: no
/// history record in the commit, no expired window entry; and an expiry
/// record never moves earlier.
fn removals_without_history_or_expiry_are_refused(store: &dyn Storage) {
    let s = fence_state();
    // Each case commits on a namespace of its own that holds `s`.
    let count = std::cell::Cell::new(0);
    let fresh = || {
        count.set(count.get() + 1);
        let ns = format!("refuse-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let cas = |next: &State| {
        let (ns, base) = fresh();
        let result = store.compare_and_swap(&ns, "w", base, next);
        if result.is_err() {
            // Nothing was written by the refusal.
            assert_eq!(store.load(&ns).unwrap().revision, base);
        }
        result
    };
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    let obligations = "unresolved run obligations cannot be disposed";
    // Every case is tried; the failures are reported together.
    let failures = std::cell::RefCell::new(vec![]);
    let refused = |label: &str, text: &str, f: &dyn Fn(&mut State)| {
        let mut next = s.clone();
        f(&mut next);
        let result = cas(&next);
        if !refused_with(result.clone(), text) {
            failures.borrow_mut().push(format!("{label}: {result:?}"));
        }
    };
    refused("intent", retention::REFUSE_RUN_REMOVAL, &|n| {
        n.intents.remove("op-a");
    });
    refused("keyed admission", early, &|n| {
        n.retention.clock_ms = Some(2_999);
        n.keyed_admissions.remove("k");
        n.retention.admission_expires_at_ms.remove("k");
    });
    refused("keyed admission, policy window", early, &|n| {
        n.retention.clock_ms = Some(1_999);
        n.keyed_admissions.remove("fallback");
    });
    refused("event", early, &|n| {
        n.retention.clock_ms = Some(3_999);
        n.events.remove("ev");
        n.retention.event_expires_at_ms.remove("ev");
    });
    refused("event, policy window", early, &|n| {
        n.retention.clock_ms = Some(2_999);
        n.events.remove("ev-fallback");
    });
    refused("event with a forged legacy tombstone", early, &|n| {
        n.events.remove("ev");
        n.retention.event_expires_at_ms.remove("ev");
        n.retention
            .event_tombstones
            .insert("ev".into(), tombstone("digest-ev"));
    });
    refused(
        "changed receipt",
        "immutable keyed admission receipt removed or changed",
        &|n| {
            n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            n.retention.clock_ms = Some(5_000);
        },
    );
    // Audit receipts leave only with their history.
    refused(
        "receipt of a live run",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            n.receipts.retain(|r| r.sequence != 2);
        },
    );
    refused(
        "receipt of a settled run without its history item",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            n.receipts.clear();
            n.receipts.push(audit(2, "run-b"));
            let run = n.runs.remove("run-a").unwrap();
            let op = n.intents.remove("op-a").unwrap();
            n.settled.push(cohort(run, vec![op], vec![]));
        },
    );
    // Expiry records: never lowered, deleted only with their item, and a new
    // one is at least the fallback.
    refused("lowered expiry", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("k".into(), 2_500);
    });
    refused("lowered event expiry", shortened, &|n| {
        n.retention.event_expires_at_ms.insert("ev".into(), 3_999);
    });
    refused("expiry deleted, item kept", shortened, &|n| {
        n.retention.admission_expires_at_ms.remove("k");
    });
    refused("event expiry deleted, event kept", shortened, &|n| {
        n.retention.event_expires_at_ms.remove("ev");
    });
    refused("new expiry below the fallback", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("fallback".into(), 1_999);
    });
    refused("new event expiry below the fallback", shortened, &|n| {
        n.retention
            .event_expires_at_ms
            .insert("ev-fallback".into(), 2_999);
    });
    refused("new expiry without its item", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("absent".into(), 1_000_000);
    });
    // History in the commit does not cover a run that is not finished, an
    // unsettled operation, an operation left live, or a run of another root.
    refused("running", obligations, &|n| {
        let running = n.runs.remove("run-b").unwrap();
        n.settled
            .push(cohort(running, vec![], vec![audit(2, "run-b")]));
        n.receipts.retain(|r| r.sequence != 2);
    });
    refused("unsettled operation", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        n.intents.remove("op-a");
        n.settled.push(cohort(
            run,
            vec![intent("op-a", "run-a", IntentStatus::Accepted)],
            vec![],
        ));
    });
    refused("operation left live", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        n.settled.push(cohort(run, vec![], vec![]));
    });
    refused("run of another root", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        let op = n.intents.remove("op-a").unwrap();
        let mut c = cohort(run, vec![op], vec![]);
        c.root = "run-b".into();
        n.settled.push(c);
    });
    refused(
        "operation without history",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            let run = n.runs.remove("run-a").unwrap();
            n.intents.remove("op-a");
            n.settled.push(cohort(run, vec![], vec![]));
        },
    );
    assert!(failures.borrow().is_empty(), "{:#?}", failures.borrow());
    // Allowed: expired entries with their records, an expiry moved later, a
    // new expiry at its fallback, and a run with its operation and receipt.
    let mut next = s.clone();
    next.retention.clock_ms = Some(4_000);
    for key in ["k", "fallback"] {
        next.keyed_admissions.remove(key);
    }
    next.retention.admission_expires_at_ms.remove("k");
    for key in ["ev", "ev-fallback"] {
        next.events.remove(key);
    }
    next.retention.event_expires_at_ms.remove("ev");
    let run = next.runs.remove("run-a").unwrap();
    let op = next.intents.remove("op-a").unwrap();
    next.receipts.retain(|r| r.sequence != 1);
    next.settled
        .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    next.keyed_admissions
        .insert("later".into(), receipt("later", 100));
    next.retention
        .admission_expires_at_ms
        .insert("later".into(), 2_100);
    let (ns, base) = fresh();
    let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
    let mut later = next.clone();
    later.settled.clear();
    later
        .retention
        .admission_expires_at_ms
        .insert("later".into(), 9_000);
    store.compare_and_swap(&ns, "w", revision, &later).unwrap();
}
#[test]
fn memory_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&MemoryStorage::default());
}
#[test]
fn record_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&RecordStorage::new());
}
/// Legacy event tombstones and correlation fences (no engine writes them
/// any more) may always be removed; an expired event leaves by its window.
/// A legacy disposal record licenses nothing: a run removed without its
/// history is refused, disposal record or not.
fn legacy_records_license_no_removal(store: &dyn Storage) {
    store.acquire("legacy", "w").unwrap();
    let mut s = State::default();
    s.retention.policy.event_replay_ms = 1_000_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.receipts.push(audit(1, "run-a"));
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention
        .event_expires_at_ms
        .insert("ev".into(), 1_000_000);
    s.retention
        .expired_correlations
        .insert("c".into(), ("host".into(), 0));
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let r = store.compare_and_swap("legacy", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.runs.remove("run-a");
    next.receipts.clear();
    next.retention.disposals.push(retention::Disposal {
        kind: "run".into(),
        key: "run-a".into(),
        at_ms: 5,
    });
    assert!(refused_with(
        store.compare_and_swap("legacy", "w", r, &next),
        retention::REFUSE_RUN_REMOVAL
    ));
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.events.remove("ev");
    next.retention.event_expires_at_ms.remove("ev");
    next.retention.event_tombstones.remove("old");
    next.retention.expired_correlations.remove("c");
    store.compare_and_swap("legacy", "w", r, &next).unwrap();
}
#[test]
fn memory_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&MemoryStorage::default());
}
#[test]
fn record_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&RecordStorage::new());
}
type Case = (&'static str, &'static str, Box<dyn Fn(&mut State)>);
/// The same fences on `RecordStorage`'s bounded transitions: a removal with
/// no history and no expired entry, or an expiry record moved earlier, is
/// refused and writes nothing; an expired window entry leaves incrementally.
#[test]
fn record_storage_transitions_hold_the_removal_fences() {
    let store = RecordStorage::new();
    let mut s = fence_state();
    s.indexes.initialized = true;
    let count = std::cell::Cell::new(0);
    let fresh = || {
        count.set(count.get() + 1);
        let ns = format!("t-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let transition_in = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        store
            .transition_v1(
                ns,
                "w",
                &mut TransitionRequest {
                    operation: "fence",
                    plan: &mut plan,
                },
            )
            .map_err(|e| match e {
                Error::Storage(e) => e,
                other => StorageError::Backend(other.to_string()),
            })
    };
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    let cases: Vec<Case> = vec![
        (
            "run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.runs.remove("run-a");
            }),
        ),
        (
            "intent",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.intents.remove("op-a");
            }),
        ),
        (
            "receipt, compacted",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.receipts.retain(|r| r.sequence != 1);
            }),
        ),
        (
            "receipt, overwritten",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                *s.receipts.get_mut(0).unwrap() = audit(3, "run-a");
            }),
        ),
        (
            "keyed admission",
            early,
            Box::new(|s| {
                s.retention.clock_ms = Some(2_999);
                s.keyed_admissions.remove("k");
                s.retention.admission_expires_at_ms.remove("k");
            }),
        ),
        (
            "event",
            early,
            Box::new(|s| {
                s.retention.clock_ms = Some(3_999);
                s.events.remove("ev");
                s.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "forged tombstone",
            early,
            Box::new(|s| {
                s.events.remove("ev");
                s.retention.event_expires_at_ms.remove("ev");
                s.retention
                    .event_tombstones
                    .insert("ev".into(), tombstone("digest-ev"));
            }),
        ),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            Box::new(|s| {
                s.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            }),
        ),
        (
            "clock",
            retention::REFUSE_CLOCK,
            Box::new(|s| {
                s.retention.clock_ms = Some(499);
            }),
        ),
        (
            "lowered expiry",
            shortened,
            Box::new(|s| {
                s.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
            }),
        ),
        (
            "expiry deleted, item kept",
            shortened,
            Box::new(|s| {
                s.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "new expiry below the fallback",
            shortened,
            Box::new(|s| {
                s.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            }),
        ),
    ];
    let mut cases = cases;
    cases.extend(rebase_refusal_cases());
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(label, text, f)| {
            let (ns, base) = fresh();
            let result = transition_in(&ns, f.as_ref());
            if result.is_err() {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
            }
            (!refused_with(result.clone(), text)).then(|| format!("{label}: {result:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
    // Ruling 3: a licensed rebase commits through the bounded transition,
    // and the licence is that commit's own.
    let (ns, _) = fresh();
    transition_in(&ns, &rebased).unwrap();
    let r = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (label, text, f) in after_rebase_refusal_cases() {
        let result = transition_in(&ns, f.as_ref());
        assert!(refused_with(result.clone(), text), "{label}: {result:?}");
    }
    transition_in(&ns, &floor_lowered).unwrap();
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
    let (ns, _) = fresh();
    let transition = |f: &dyn Fn(&mut State)| transition_in(&ns, f);
    // Expired: both entries leave in one bounded transition.
    transition(&|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
    })
    .unwrap();
    let state = store.load(&ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
    // With history for the run, its operation and its receipt, the run
    // leaves.
    transition(&|s| {
        let run = s.runs.remove("run-a").unwrap();
        let op = s.intents.remove("op-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        s.settled
            .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    })
    .unwrap();
    let state = store.load(&ns).unwrap().state;
    assert!(!state.runs.contains_key("run-a"));
    assert_eq!(
        state
            .receipts
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![2]
    );
}
/// `check_not_live` on a State whose runtime indexes are initialised (the
/// branch every engine commit takes): a receipt is live when the receipt
/// index points at a receipt with that sequence.
#[test]
fn check_not_live_reads_receipts_through_the_initialised_index() {
    let mut state = State::default();
    state.indexes.initialized = true;
    state.receipts.push(live_receipt(7));
    state.receipts.push(live_receipt(9));
    state.indexes.all_receipts.insert(7, 0);
    state.indexes.all_receipts.insert(9, 1);
    let items = |sequence: u64| synthetic_cohort("r", 10, &[sequence]).history_items();
    let receipts_only = |sequence: u64| {
        items(sequence)
            .into_iter()
            .filter(|i| i.kind == InspectionKind::Receipts)
            .collect::<Vec<_>>()
    };
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(7)
    )));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(9)
    )));
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // An index entry that points at a receipt with another sequence is not
    // taken as live (it is re-checked against the receipt).
    state.indexes.all_receipts.insert(8, 1);
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // Runs and operations are map lookups on either branch.
    state.runs.insert("r".into(), completed_run("r"));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &items(8)
    )));
}

// --- Task 5: a settled cohort leaves the record in the commit that settles it

fn runs_page(e: &Engine<MemoryStorage>, run: Option<&str>) -> Vec<Value> {
    e.inspect(&InspectionQuery {
        kind: InspectionKind::Runs,
        run: run.map(String::from),
        after: None,
        limit: 1000,
    })
    .unwrap()
    .items
}
fn done_artifact() -> Artifact {
    artifact(
        "a",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    )
}
#[test]
fn finished_root_leaves_the_record_in_the_commit_that_finishes_it() {
    let e = engine();
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let before = e.store.load("test").unwrap().revision;
    assert!(e.advance(&id, 2).unwrap());
    let v = e.store.load("test").unwrap();
    assert_eq!(v.revision, before + 1);
    assert!(
        v.state.runs.is_empty()
            && v.state.intents.is_empty()
            && v.state.receipts.is_empty()
            && v.state.budgets.is_empty()
    );
    assert!(
        v.state.indexes.root_members.is_empty()
            && v.state.indexes.run_roots.is_empty()
            && v.state.indexes.receipts.is_empty()
    );
    let run = &runs_page(&e, Some(&id))[0];
    assert_eq!(
        (run["status"].as_str(), run["outcome"].as_str()),
        (Some("completed"), Some("ok"))
    );
}
#[test]
fn cohort_stays_while_an_independent_child_runs() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let id = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs[&id].status.terminal());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, None).len(), 2);
}
#[test]
fn cancelled_run_stays_until_its_operation_settles_then_late_results_are_not_found() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Unknown {
                reason: "lost".into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    e.cancel(&id, 1).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Cancelling);
    let intent = s.intents.keys().next().unwrap().clone();
    let done = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    e.record_result(&intent, done.clone(), 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["status"], "cancelled");
    assert!(matches!(
        e.record_result(&intent, done, 3),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(e.advance(&id, 3), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(&id, 3), Err(Error::NotFound(_))));
}
fn inspect_all(e: &Engine<impl Storage>, kind: InspectionKind, run: Option<&str>) -> Vec<Value> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = e
            .inspect(&InspectionQuery {
                kind,
                run: run.map(String::from),
                after,
                limit: 2,
            })
            .unwrap();
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return items,
        }
    }
}
/// The one item of `kind` whose inspection key is `key`.
fn inspected(e: &Engine<MemoryStorage>, kind: InspectionKind, key: &str) -> Value {
    let items: Vec<Value> = inspect_all(e, kind, None)
        .into_iter()
        .filter(|v| match kind {
            InspectionKind::Receipts => format!("{:020}", v["sequence"].as_u64().unwrap()) == key,
            _ => v["id"] == key,
        })
        .collect();
    assert_eq!(items.len(), 1, "{kind:?} {key}");
    items.into_iter().next().unwrap()
}
#[test]
fn history_equals_the_projection_of_the_removed_cohort() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
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
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.intents.is_empty() && s.receipts.is_empty());
    let cohorts = e.store.settled_cohorts("test");
    assert_eq!(cohorts.len(), 1);
    let cohort = &cohorts[0];
    assert_eq!(cohort.root, id);
    assert_eq!(cohort.runs.len(), 1);
    assert_eq!(cohort.intents.len(), 1);
    assert!(cohort.receipts.len() >= 4, "{:?}", cohort.receipts);
    // Finish time: the terminal receipt's time.
    let terminal = cohort.runs[0].terminal_sequence.unwrap();
    let at = cohort
        .receipts
        .iter()
        .find(|r| r.sequence == terminal)
        .unwrap()
        .at_ms;
    assert_eq!(cohort.finished_at_ms, at);
    for run in &cohort.runs {
        assert_eq!(
            inspection::public_run(run),
            inspected(&e, InspectionKind::Runs, &run.id)
        );
    }
    for intent in &cohort.intents {
        assert_eq!(
            inspection::public_intent(intent),
            inspected(&e, InspectionKind::Effects, &intent.id)
        );
    }
    for receipt in &cohort.receipts {
        assert_eq!(
            inspection::public_receipt(receipt),
            inspected(
                &e,
                InspectionKind::Receipts,
                &format!("{:020}", receipt.sequence)
            )
        );
    }
    // The private fields stay readable from the retained cohort.
    assert_eq!(
        e.store.settled_run("test", &id).unwrap().outcome.as_deref(),
        Some("ok")
    );
    assert_eq!(cohort.intents[0].result, Some(("ok".into(), Value::Null)));
}
#[test]
fn admin_receipts_move_to_history_in_the_commit_that_writes_them() {
    let e = engine();
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    e.set_activation_controls("a", ActivationControls::default(), 3)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.iter().all(|r| r.run != "admin"));
    let receipts = e
        .inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: Some("admin".into()),
            after: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(receipts.items[0]["kind"], "activation_controls");
}
fn install_on<S: Storage>(e: &Engine<S>, a: Artifact, binding: &str) {
    let d = a.digest.clone();
    e.install(a).unwrap();
    e.activate(binding, &d, authority()).unwrap();
}
fn assert_same_engines(m: &Engine<MemoryStorage>, r: &Engine<RecordStorage>, step: &str) {
    assert_eq!(
        serde_json::to_value(m.snapshot().unwrap()).unwrap(),
        serde_json::to_value(r.snapshot().unwrap()).unwrap(),
        "snapshot after {step}"
    );
    for kind in [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ] {
        assert_eq!(
            inspect_all(m, kind, None),
            inspect_all(r, kind, None),
            "{kind:?} after {step}"
        );
    }
}
#[test]
fn record_storage_settles_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    let child = wait_artifact();
    let parent = parent_artifact(&child, CallMode::Independent);
    m.install(child.clone()).unwrap();
    r.install(child.clone()).unwrap();
    assert_same_engines(&m, &r, "install");
    install(&m, parent.clone(), "parent");
    install_on(&r, parent, "parent");
    assert_same_engines(&m, &r, "activate");
    let id = m
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    assert_eq!(
        r.admit("parent", "main", Value::Null, authority(), 0)
            .unwrap(),
        id
    );
    assert_same_engines(&m, &r, "admit");
    m.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    r.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert_same_engines(&m, &r, "drive 0");
    assert_eq!(m.snapshot().unwrap().runs.len(), 2);
    m.admit_event("host", event("e1", 2), 5).unwrap();
    r.admit_event("host", event("e1", 2), 5).unwrap();
    assert_same_engines(&m, &r, "event");
    m.drive(&mut FakeHost::default(), 5, 20).unwrap();
    r.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert_same_engines(&m, &r, "drive 5");
    // The settling commit took the record backend's transition path.
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert!(r.snapshot().unwrap().runs.is_empty());
    assert_eq!(inspect_all(&r, InspectionKind::Runs, None).len(), 2);
}
/// A run key that is already history cannot become live again: the engine's
/// commit is refused, and the refusal leaves the revision and the lease.
#[test]
fn a_live_key_already_in_history_is_refused_through_the_engine() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let v = e.store.load("test").unwrap();
    let next = format!("run-{:016x}", v.state.sequence + 1);
    let mut seeded = v.state.clone();
    seeded.settled.push(synthetic_cohort(&next, 0, &[]));
    e.store
        .compare_and_swap("test", "incarnation-1", v.revision, &seeded)
        .unwrap();
    let revision = e.store.load("test").unwrap().revision;
    let refused = e.admit("a", "main", Value::Null, authority(), 1);
    assert!(
        matches!(&refused, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, revision);
    assert!(after.state.runs.is_empty());
    // The lease is still this engine's.
    assert_eq!(
        e.store.acquire("test", "other"),
        Err(StorageError::Ownership)
    );
    e.set_activation_controls("a", ActivationControls::default(), 2)
        .unwrap();
    assert_eq!(e.store.load("test").unwrap().revision, revision + 1);
}
/// A run that moves from the live state to history between two inspection
/// pages is neither repeated nor skipped.
#[test]
fn a_run_moving_to_history_between_pages_is_neither_repeated_nor_skipped() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let ids: Vec<String> = (0..3)
        .map(|i| e.admit("a", "main", Value::Null, authority(), i).unwrap())
        .collect();
    let query = |after: Option<String>| InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after,
        limit: 1,
    };
    let first = e.inspect(&query(None)).unwrap();
    assert_eq!(first.items[0]["id"], ids[0].as_str());
    assert_eq!(first.items[0]["status"], "admitted");
    // The run already paged and the next one both leave the state.
    assert!(e.advance(&ids[0], 5).unwrap());
    assert!(e.advance(&ids[1], 5).unwrap());
    let second = e.inspect(&query(first.next)).unwrap();
    assert_eq!(second.items[0]["id"], ids[1].as_str());
    assert_eq!(second.items[0]["status"], "completed");
    let third = e.inspect(&query(second.next)).unwrap();
    assert_eq!(third.items[0]["id"], ids[2].as_str());
    assert_eq!(third.items[0]["status"], "admitted");
    assert!(third.next.is_none());
    // Receipts too: a receipt page edge across the move.
    let receipts = |after: Option<String>| {
        e.inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: None,
            after,
            limit: 1,
        })
        .unwrap()
    };
    let page = receipts(None);
    let mut seen = vec![page.items[0]["sequence"].as_u64().unwrap()];
    e.advance(&ids[2], 6).unwrap();
    let mut after = page.next;
    while let Some(cursor) = after {
        let page = receipts(Some(cursor));
        seen.extend(page.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        after = page.next;
    }
    let all: Vec<u64> = e
        .store
        .settled_cohorts("test")
        .iter()
        .flat_map(|c| c.receipts.iter().map(|r| r.sequence))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(seen, all);
}
/// Commands addressed to a run that has left the record get `NotFound`
/// (today's answer for a collected run), and a late wake is a no-op.
#[test]
fn commands_to_a_run_that_left_the_record_are_not_found() {
    let e = engine();
    let child = wait_artifact();
    install(&e, child, "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Waiting);
    // Its deadline timer is due at 100.
    assert!(!s.indexes.timers.is_empty());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.waits.is_empty() && s.indexes.timers.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "ok");
    assert!(matches!(e.advance(&id, 6), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(&id, 6), Err(Error::NotFound(_))));
    // A late wake: driving past the old deadline finds nothing to do.
    let revision = e.store.load("test").unwrap().revision;
    assert_eq!(e.drive(&mut FakeHost::default(), 1_000, 20).unwrap(), 0);
    assert_eq!(e.store.load("test").unwrap().revision, revision);
}
/// A late result for an operation that left with its cohort, and a recovery
/// request for it, are `NotFound`.
#[test]
fn late_results_and_recovery_for_a_removed_operation_are_not_found() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    // The invoke step prepares the operation; cancelling the run cancels the
    // prepared operation, and the run then finishes cancelled.
    e.advance(&id, 0).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 1);
    e.cancel(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let cohort = &e.store.settled_cohorts("test")[0];
    assert_eq!(cohort.intents.len(), 1);
    assert_eq!(cohort.intents[0].status, IntentStatus::Cancelled);
    let intent = cohort.intents[0].id.clone();
    let late = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    assert!(matches!(
        e.record_result(&intent, late, 1),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        e.request_recovery(&intent, "op", "late", vec!["ref".into()], 1),
        Err(Error::NotFound(_))
    ));
}
/// `activate` takes its time as the later of the last receipt and the clock:
/// once every receipt has left, the clock still dates the activation.
#[test]
fn activate_dates_by_the_clock_once_receipts_have_left() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let id = e.admit("a", "main", Value::Null, authority(), 40).unwrap();
    e.advance(&id, 50).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.is_empty());
    assert_eq!(s.retention.clock_ms, Some(50));
    e.activate("b", &done_artifact().digest, authority())
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.catalog_history_at_ms[&s.catalog_revision], 50);
    assert_eq!(s.retention.clock_ms, Some(50));
}
/// Settlement is found from the commit's dirty roots and the root indexes
/// (SC-6): settling one cohort reads the same records whether 20 or 80 other
/// runs are live, and scans no run, operation or root index container.
#[test]
fn settling_one_cohort_does_not_scan_other_live_runs() {
    use std::sync::atomic::Ordering;
    let measure = |others: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        for i in 0..others {
            e.admit("a", "main", Value::Null, authority(), i as i64)
                .unwrap();
        }
        let id = e.admit("a", "main", Value::Null, authority(), 100).unwrap();
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        let transitions = e.store.stats.transitions.load(Ordering::Relaxed);
        assert!(e.advance(&id, 101).unwrap());
        assert_eq!(
            e.store.stats.transitions.load(Ordering::Relaxed),
            transitions + 1
        );
        assert!(!e.snapshot().unwrap().runs.contains_key(&id));
        assert_eq!(e.snapshot().unwrap().runs.len(), others);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    // Both sizes exceed the 16-record page a bounded index read takes (the
    // due-timer query reads one such page), so equal reads mean nothing
    // scales with the number of live runs.
    let (few, few_scanned) = measure(20);
    let (many, many_scanned) = measure(80);
    assert_eq!(few, many, "record reads of the settling commit");
    assert_eq!(few_scanned, many_scanned);
    // Only the positional receipt sequence and its offset indexes are
    // rewritten whole (SC-6: compaction is O(live receipts)).
    let allowed: BTreeSet<String> = ["receipts", "indexes.receipts", "indexes.all_receipts"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(many_scanned.is_subset(&allowed), "{many_scanned:?}");
}

// --- Task 5 fix round 1 ---------------------------------------------------

/// `RuntimeIndexes` fields of the engine that aq-server vendors
/// (`vendor/aq-playbooks-0173673`, pre-D10): no `root_intents` and no
/// backfill flag.
const VENDORED_INDEX_FIELDS: &[&str] = &[
    "ready",
    "ready_tokens",
    "scope_cursor",
    "dispatch_turn",
    "effect_cursor",
    "effect_item_cursor",
    "root_members",
    "root_active",
    "root_effects",
    "root_last_activity",
    "run_roots",
    "intent_roots",
    "effects",
    "effect_memberships",
    "timers",
    "due",
    "run_timer",
    "events",
    "event_memberships",
    "waiters",
    "run_subscriptions",
    "dependents",
    "run_dependencies",
    "completion_ready",
    "uncertain_completion_contracts",
    "uncertain_contract_membership",
    "dirty_runs",
    "dirty_intents",
    "runs",
    "receipts",
    "all_receipts",
    "triggers",
    "activation_triggers",
    "initialized",
];
/// `state` as a pre-D10 engine stored it: the index fields D10 and this task
/// added are absent, so they decode with their defaults.
fn vendored_shape(state: &State) -> State {
    let mut value = serde_json::to_value(state).unwrap();
    let indexes = value["indexes"].as_object_mut().unwrap();
    indexes.retain(|field, _| VENDORED_INDEX_FIELDS.contains(&field.as_str()));
    let fields: BTreeSet<&str> = indexes.keys().map(String::as_str).collect();
    assert_eq!(fields, VENDORED_INDEX_FIELDS.iter().copied().collect());
    serde_json::from_value(value).unwrap()
}
fn completed_host() -> FakeHost {
    FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
            ],
        )]),
        ..Default::default()
    }
}
/// An invoke run whose operation has completed while the run is still live
/// (waiting to take its `ok` edge), on `e`.
fn run_with_completed_operation<S: Storage>(
    e: &Engine<S>,
    host: &mut FakeHost,
) -> (String, String) {
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.dispatch_next(host, 0).unwrap();
    let s = e.snapshot().unwrap();
    let intent = s.intents.values().next().unwrap().clone();
    assert_eq!(intent.status, IntentStatus::Completed);
    assert!(!s.runs[&id].status.terminal());
    (id, intent.id)
}
/// Store `state` over the namespace's current State (direct CAS).
fn overwrite<S: Storage>(store: &S, owner: &str, state: &State) {
    let revision = store.load("test").unwrap().revision;
    store
        .compare_and_swap("test", owner, revision, state)
        .unwrap();
}
/// C1: a namespace written by the pre-D10 engine has no `root_intents`.
/// Opening it backfills the index once, so a pre-upgrade run with an earlier
/// completed operation finishes, leaves with that operation in its history,
/// and a run admitted afterwards progresses.
fn pre_d10_namespace_is_backfilled_on_open<S: Storage + Clone>(store: S) {
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    let (old, intent) = run_with_completed_operation(&e, &mut host);
    let legacy = vendored_shape(&e.snapshot().unwrap());
    assert!(legacy.indexes.initialized && legacy.indexes.root_intents.is_empty());
    assert!(!legacy.indexes.root_intents_complete);
    overwrite(&e.store, "incarnation-1", &legacy);
    e.release().unwrap();
    let e = Engine::new(store, "test", "incarnation-2").unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.indexes.root_intents_complete);
    assert!(s.indexes.root_intents[&old].contains(&intent));
    let new = e
        .admit("invoke", "main", Value::Null, authority(), 1)
        .unwrap();
    for _ in 0..10 {
        e.drive(&mut host, 1, 20).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.is_empty() && s.intents.is_empty(),
        "{:?}",
        s.runs.keys().collect::<Vec<_>>()
    );
    let effects = inspect_all(&e, InspectionKind::Effects, Some(&old));
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["id"], intent.as_str());
    assert_eq!(effects[0]["status"], "completed");
    for id in [&old, &new] {
        let runs = inspect_all(&e, InspectionKind::Runs, Some(id));
        assert_eq!(runs[0]["outcome"], "ok", "{id}");
    }
}
#[test]
fn memory_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(MemoryStorage::retaining_settled());
}
#[test]
fn record_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(RecordStorage::new());
}
/// The backfill runs once: the flag is stored, and a second open does not
/// read the operations again (format 3 record reads of `intents`).
#[test]
fn root_intents_backfill_runs_once() {
    let store = RecordStorage::new();
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    run_with_completed_operation(&e, &mut host);
    overwrite(
        &e.store,
        "incarnation-1",
        &vendored_shape(&e.snapshot().unwrap()),
    );
    e.release().unwrap();
    let scanned = |store: &RecordStorage| {
        store
            .stats
            .fallbacks()
            .iter()
            .filter(|f| f.starts_with("intents "))
            .count()
    };
    let before = scanned(&store);
    let e = Engine::new(store.clone(), "test", "incarnation-2").unwrap();
    let first = scanned(&store) - before;
    assert_eq!(first, 1, "the backfill reads the operations once");
    e.release().unwrap();
    let e = Engine::new(store.clone(), "test", "incarnation-3").unwrap();
    assert_eq!(scanned(&store) - before, first, "the second open does not");
    assert!(e.snapshot().unwrap().indexes.root_intents_complete);
    // A new namespace is complete from its first rebuild.
    let fresh = Engine::new(MemoryStorage::default(), "fresh", "w").unwrap();
    assert!(fresh.snapshot().unwrap().indexes.root_intents_complete);
}
/// Ruling 2: a cohort whose settling commit is refused does not wedge the
/// ready queue. The refusal reaches the host (`drive` returns it) and the
/// failing run moves behind the others, so unrelated runs still progress.
#[test]
fn an_unsettleable_cohort_does_not_stop_unrelated_runs() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install(&e, done_artifact(), "a");
    let mut host = completed_host();
    let (stuck, _) = run_with_completed_operation(&e, &mut host);
    // Break the index behind the engine's back (flag kept), so the commit
    // that finishes `stuck` is refused by the removal fence.
    let mut broken = e.snapshot().unwrap();
    broken.indexes.root_intents.clear();
    overwrite(&e.store, "incarnation-1", &broken);
    // Drive until the finishing commit of `stuck` is refused: `stuck` is
    // then at the head of the ready queue.
    let refused = (0..5).find_map(|_| e.drive(&mut host, 5, 20).err());
    assert!(
        matches!(&refused, Some(Error::Storage(StorageError::Backend(m))) if m == "unresolved run obligations cannot be disposed"),
        "{refused:?}"
    );
    let unrelated: Vec<String> = (0..3)
        .map(|i| {
            e.admit("a", "main", Value::Null, authority(), 1 + i)
                .unwrap()
        })
        .collect();
    let mut refusals = 0;
    for _ in 0..20 {
        match e.drive(&mut host, 5, 20) {
            Ok(_) => {}
            Err(Error::Storage(StorageError::Backend(m))) => {
                assert_eq!(m, "unresolved run obligations cannot be disposed");
                refusals += 1;
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(refusals > 0, "the refusal surfaces to the host");
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.contains_key(&stuck),
        "the unsettleable run stays live"
    );
    for id in &unrelated {
        assert!(!s.runs.contains_key(id), "{id} progressed and left");
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok");
    }
}
/// Ruling 3 (M5): a run whose root has gone is an engine fault. The commit
/// fails with an internal error; the run is not sent down its playbook's
/// error branch and nothing is written.
#[test]
fn a_missing_root_fails_the_commit_instead_of_taking_an_error_branch() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&root, 0).unwrap();
    e.advance(&root, 0).unwrap();
    let mut s = e.snapshot().unwrap();
    assert!(s.runs[&root].status.terminal());
    let member = s.runs.keys().find(|id| **id != root).unwrap().clone();
    // Remove only the root, with its history (the fence allows it: the
    // independent member is not its owned child).
    let run = s.runs.remove(&root).unwrap();
    let receipts: Vec<Receipt> = s
        .receipts
        .iter()
        .filter(|r| r.run == root)
        .cloned()
        .collect();
    s.receipts.retain(|r| r.run != root);
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    for (offset, r) in s.receipts.clone().iter().enumerate() {
        s.indexes
            .receipts
            .entry(r.run.clone())
            .or_default()
            .insert(r.sequence, offset);
        s.indexes.all_receipts.insert(r.sequence, offset);
    }
    s.settled.push(SettledCohort {
        root: root.clone(),
        finished_at_ms: 0,
        runs: vec![run],
        intents: vec![],
        receipts,
    });
    overwrite(&e.store, "incarnation-1", &s);
    let before = e.store.load("test").unwrap();
    let refused = e.advance(&member, 1);
    assert!(
        matches!(&refused, Err(Error::Conflict(m)) if m.starts_with("engine invariant violated")),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.state.runs[&member].step,
        before.state.runs[&member].step
    );
    assert!(after.state.receipts.iter().all(|r| r.kind != "error"));
}
fn trigger_artifact() -> Artifact {
    let c = event_contract();
    let mut a = done_artifact().content;
    a.registry.events.insert(c.name.clone(), c.clone());
    a.playbook.triggers.push(Trigger {
        id: "on-completed".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    Artifact::seal(a).unwrap()
}
fn event_run_id(id: &str) -> bool {
    id.strip_prefix("event-sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
/// SC-7: an event accepted again after its window gets new destination,
/// run (and so child and operation) ids, in the same `event-sha256:` form;
/// the first run's history stays and the second run commits beside it.
#[test]
fn an_event_accepted_again_after_its_window_gets_new_run_ids() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("again", 2), 0).unwrap();
    assert_eq!(e.fanout(&first.key, 10, 0).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let first_run = runs_page(&e, None)[0]["id"].as_str().unwrap().to_string();
    // The window passes: GC drops the event; the legacy fences it leaves
    // (tombstone, correlation fences) go too, as Task 6 will not write them.
    while e.collect_garbage(2_000, 100).unwrap().more {}
    let mut s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    s.retention.event_tombstones.clear();
    s.retention.expired_correlations.clear();
    overwrite(&e.store, "incarnation-1", &s);
    let second = e.admit_event("host", event("again", 2), 2_001).unwrap();
    assert_eq!(second.key, first.key);
    assert!(!second.duplicate);
    assert_eq!(e.fanout(&second.key, 10, 2_001).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 2_001, 20).unwrap();
    let ids: Vec<String> = runs_page(&e, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&first_run));
    assert!(ids.iter().all(|id| event_run_id(id)), "{ids:?}");
    assert!(e.snapshot().unwrap().runs.is_empty());
}
/// SC-7, existing data: a destination stored before the change keeps its
/// key, and its run id is `event-<that key>`.
#[test]
fn a_stored_destination_keeps_its_key_and_run_id() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    let accepted = e.admit_event("host", event("stored", 2), 0).unwrap();
    let mut s = e.snapshot().unwrap();
    let revision = s.bindings["trigger"].revision;
    let legacy = digest(&(&accepted.key, "trigger", revision, "on-completed")).unwrap();
    s.events.get_mut(&accepted.key).unwrap().destinations[0].key = legacy.clone();
    overwrite(&e.store, "incarnation-1", &s);
    assert_eq!(e.fanout(&accepted.key, 10, 1).unwrap(), 1);
    let id = format!("event-{legacy}");
    assert!(event_run_id(&id));
    assert!(e.snapshot().unwrap().runs.contains_key(&id));
}
/// M4: a finished run's inspection items are byte-for-byte the same while it
/// is still live (its cohort waits for an independent child) and after its
/// cohort has left as history.
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert!(e.snapshot().unwrap().runs[&root].status.terminal());
    let pages = |e: &Engine<MemoryStorage>| -> Vec<Vec<u8>> {
        [
            InspectionKind::Runs,
            InspectionKind::Effects,
            InspectionKind::Receipts,
        ]
        .into_iter()
        .map(|kind| serde_json::to_vec(&inspect_all(e, kind, Some(&root))).unwrap())
        .collect()
    };
    let live = pages(&e);
    assert!(live[2].len() > 2, "the root has receipts");
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(pages(&e), live);
}

// --- Task 6: window entries with expiry, dropped 64 per commit ------------

/// The window order of `s` as (expiry, member), oldest first.
fn window_entries(s: &State) -> Vec<(i64, String)> {
    s.retention
        .expiry
        .keys()
        .map(|k| {
            let (at, member) = retention::window_entry(k).unwrap();
            (at, member.to_string())
        })
        .collect()
}
fn ret_engine() -> Engine<MemoryStorage> {
    let e = engine();
    install(&e, done_artifact(), "a");
    e
}
fn keyed_on<S: Storage>(e: &Engine<S>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    }
}
fn keyed(e: &Engine<MemoryStorage>, key: &str) -> KeyedAdmissionRequest {
    keyed_on(e, key)
}
#[test]
fn window_drop_is_bounded_to_64_per_commit_oldest_first() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed(&e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 65);
    e.admit_once(keyed(&e, "k-new"), 2_000).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
        vec!["k-064".to_string(), "k-new".to_string()]
    );
}
#[test]
fn expired_event_leaves_without_tombstone_or_fence_and_redelivery_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap(); // expires at max(1, 1_000)
    e.admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty());
    assert!(
        !e.admit_event("host", event("e1", 2), 1_002)
            .unwrap()
            .duplicate
    );
}
#[test]
fn wait_registered_after_its_event_expired_times_out() {
    // NR-1 option (a)
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit_event("host", event("e1", 2), 0).unwrap();
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    e.advance(&id, 1_001).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.advance(&id, 1_101).unwrap();
    e.advance(&id, 1_102).unwrap();
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "error");
}
#[test]
fn keyed_admission_of_a_live_run_outlives_its_window() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    e.admit_once(keyed(&e, "other"), 2_000).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_001).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt.run_id, first.receipt.run_id);
}
#[test]
fn deferred_entries_do_not_block_later_due_entries() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap(); // live wait, minimum revision 2
    e.admit_event("host", event("held", 1), 0).unwrap(); // revision 1: no match; held by the waiter; window 1_000
    for i in 0..65 {
        let d = e
            .admit_once(keyed(&e, &format!("k-{i:03}")), 1_000 + i)
            .unwrap();
        e.advance(&d.receipt.run_id, 1_000 + i).unwrap();
    }
    e.admit_once(keyed(&e, "x"), 3_000).unwrap(); // event deferred to 4_000 + 63 drops
    e.admit_once(keyed(&e, "y"), 3_001).unwrap(); // the remaining 2 drop; the event is not re-examined
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions.len(), 2);
    assert_eq!(s.events.len(), 1);
    assert_eq!(
        s.retention
            .expiry
            .keys()
            .next()
            .and_then(|k| retention::window_entry(k))
            .map(|(at, _)| at),
        Some(4_000)
    );
}
#[test]
fn backwards_host_time_neither_refuses_work_nor_shortens_windows() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let d = e.admit_once(keyed(&e, "late"), 10).unwrap(); // backwards: accepted, not refused
    e.advance(&d.receipt.run_id, 11).unwrap();
    e.admit("a", "main", Value::Null, authority(), 12).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(5_000));
    assert_eq!(
        s.retention.admission_expires_at_ms.get("late").copied(),
        Some(6_000)
    );
    assert!(s.keyed_admissions.contains_key("late"));
}
/// An event of `wait_artifact`'s contract with its own correlation.
fn correlated_event(id: &str, invocation: &str) -> EventEnvelope {
    EventEnvelope {
        correlation: BTreeMap::from([("invocation".into(), invocation.into())]),
        ..event(id, 2)
    }
}
#[test]
fn burst_at_a_frozen_clock_leaves_at_most_2_kb_of_window_entries_per_run() {
    let e = ret_engine();
    e.install(wait_artifact()).unwrap();
    // Task 8b: above the burst's 400 entries, so nothing is evicted by
    // count and the bytes of every window entry are measured.
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let mut after_first = 0;
    for i in 0..200 {
        let d = e
            .admit_once(keyed(&e, &format!("burst-{i:04}")), 0)
            .unwrap();
        e.advance(&d.receipt.run_id, 0).unwrap();
        e.admit_event(
            "host",
            correlated_event(&format!("burst-{i:04}"), &format!("inv-{i:04}")),
            0,
        )
        .unwrap();
        if i == 0 {
            after_first = canonical_bytes(&e.snapshot().unwrap()).unwrap().len();
        }
    }
    let s = e.snapshot().unwrap();
    let after_last = canonical_bytes(&s).unwrap().len();
    let per_run = (after_last - after_first) / 199;
    eprintln!("burst: {per_run} bytes per run");
    assert!(per_run <= 2_048, "{per_run} bytes per run");
    assert_eq!((s.keyed_admissions.len(), s.events.len()), (200, 200));
    assert!(
        s.runs.is_empty()
            && s.intents.is_empty()
            && s.receipts.is_empty()
            && s.waits.is_empty()
            && s.budgets.is_empty()
    );
}
/// The 64-drop script: 65 finished keyed runs with a 1 s window, then one
/// admission past it.
fn drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed_on(e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    e.admit_once(keyed_on(e, "k-new"), 2_000).unwrap();
}
#[test]
fn record_storage_drops_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, done_artifact(), "a");
    install_on(&r, done_artifact(), "a");
    drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the 64-drop script");
    assert_eq!(r.snapshot().unwrap().keyed_admissions.len(), 2);
}
#[test]
fn commits_without_host_time_drop_nothing() {
    let e = ret_engine();
    drop_script(&e);
    // k-064 expired at 1_064; the clock is 2_000.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
    e.install(wait_artifact()).unwrap();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 2_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.keyed_admissions.contains_key("k-064"));
    assert_eq!(s.retention.clock_ms, Some(2_000));
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
}
/// Carried from Task 4: a changed keyed receipt is always refused, so an
/// expired key admitted again leaves first and is then accepted as new.
fn expired_key_admitted_again_is_new<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed_on(e, "k"), 0).unwrap();
    assert!(e.advance(&first.receipt.run_id, 0).unwrap());
    // No commit between: the old receipt is still in the State, expired.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k"));
    // A new activation revision changes the request's content: after the
    // window that is not a conflict.
    e.activate("a", &done_artifact().digest, authority())
        .unwrap();
    let again = e.admit_once(keyed_on(e, "k"), 2_000).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
    assert_eq!(again.receipt.accepted_at_ms, 2_000);
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["k"], again.receipt);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
    assert_eq!(window_entries(&s), vec![(3_000, "k:k".to_string())]);
    // The first run is history.
    assert!(
        inspect_all(e, InspectionKind::Runs, Some(&first.receipt.run_id))
            .iter()
            .any(|r| r["status"] == "completed")
    );
}
#[test]
fn memory_storage_admits_an_expired_key_again_as_new() {
    expired_key_admitted_again_is_new(&ret_engine());
}
#[test]
fn record_storage_admits_an_expired_key_again_as_new() {
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&r, done_artifact(), "a");
    expired_key_admitted_again_is_new(&r);
}
#[test]
fn an_expired_event_delivered_again_before_its_drop_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap();
    let again = e.admit_event("host", event("e1", 2), 2_000).unwrap();
    assert_eq!(again.key, first.key);
    assert!(!again.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&again.key].accepted_at_ms, 2_000);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&again.key).copied(),
        Some(3_000)
    );
    assert_eq!(
        s.indexes.events.values().map(BTreeMap::len).sum::<usize>(),
        2
    );
}
#[test]
fn an_expired_event_held_by_a_waiter_stays_and_its_redelivery_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.events.contains_key(&held.key));
    // Deferred by its own window (1_000) from the clock.
    assert_eq!(
        s.retention.event_expires_at_ms.get(&held.key).copied(),
        Some(2_500)
    );
    // Expired at 2_500 but still held: a duplicate, as inside its window.
    let again = e.admit_event("host", event("held", 1), 2_600).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.key, held.key);
}
/// A trigger artifact whose trigger input does not fit its bounds: its
/// destination is blocked by input when the event is accepted.
fn input_blocked_trigger_artifact() -> Artifact {
    let mut a = trigger_artifact().content;
    // The input (`null`, 4 bytes) exceeds the playbook's payload bound, which
    // is checked when the event is accepted.
    a.playbook.limits.max_payload_bytes = 1;
    Artifact::seal(a).unwrap()
}
#[test]
fn a_queued_destination_holds_an_expired_event_and_an_input_blocked_one_does_not() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let queued = e
        .admit_event("host", correlated_event("queued", "q"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        s.events.contains_key(&queued.key),
        "a queued destination holds it"
    );
    assert_eq!(
        s.retention.event_expires_at_ms.get(&queued.key).copied(),
        Some(2_500)
    );

    let e = engine();
    install(&e, input_blocked_trigger_artifact(), "blocked");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let blocked = e
        .admit_event("host", correlated_event("blocked", "b"), 0)
        .unwrap();
    let owner = format!("event:{}", blocked.key);
    assert_eq!(
        e.snapshot()
            .unwrap()
            .receipts
            .iter()
            .filter(|r| r.run == owner)
            .count(),
        1
    );
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.events.contains_key(&blocked.key),
        "an input-blocked destination does not hold it"
    );
    // Its receipts went to history with it.
    assert!(s.receipts.iter().all(|r| r.run != owner));
    assert_eq!(
        inspect_all(&e, InspectionKind::Receipts, Some(&owner)).len(),
        1
    );
}
/// Cost (SC-5): a commit with nothing expired reads the same records however
/// many window entries there are, and a commit dropping k entries reads in
/// proportion to k; no window container is read whole.
#[test]
fn window_work_on_record_storage_depends_on_entries_dropped_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, due: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        // Due entries expire at 1_000 + i, kept ones at 1_100 + i.
        for i in 0..due {
            let d = e
                .admit_once(keyed_on(&e, &format!("due-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        for i in 0..kept {
            let d = e
                .admit_once(keyed_on(&e, &format!("kept-{i:03}")), 100 + i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, 100 + i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.admit("a", "main", Value::Null, authority(), 1_050)
            .unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.keyed_admissions.len(), kept);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    let (none_few, scanned) = measure(20, 0);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (none_many, _) = measure(80, 0);
    assert_eq!(none_few, none_many, "nothing expired");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 dropped");
    let (sixteen, _) = measure(20, 16);
    assert_eq!(
        sixteen - eight,
        eight - none_few,
        "reads grow linearly with drops"
    );
    assert!(eight > none_few);
}
/// Events on both backends: one held by a live waiter (deferred), one
/// delivered again after its window (it leaves first, then is new), one
/// dropped with its index entries.
fn event_drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit_event("host", correlated_event("again", "a"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("dropped", "d"), 0)
        .unwrap();
    assert!(
        !e.admit_event("host", correlated_event("again", "a"), 1_500)
            .unwrap()
            .duplicate
    );
    e.admit("waiter", "main", Value::Null, authority(), 1_600)
        .unwrap();
}
#[test]
fn record_storage_drops_events_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, wait_artifact(), "waiter");
    install_on(&r, wait_artifact(), "waiter");
    event_drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    event_drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the event script");
    let s = r.snapshot().unwrap();
    // "held" (deferred to 2_500) and the second "again" (2_500) remain.
    assert_eq!(s.events.len(), 2);
    assert_eq!(
        window_entries(&s)
            .iter()
            .map(|(at, _)| *at)
            .collect::<Vec<_>>(),
        vec![2_500, 2_500]
    );
    assert_eq!(s.indexes.event_memberships.len(), 2);
    // Two correlation index keys each; the dropped event's are gone.
    assert_eq!(s.indexes.events.len(), 4);
}
/// SC-10 at admission: an expired key whose run is still live is a
/// duplicate even before the drop has deferred it.
#[test]
fn an_expired_key_of_a_live_run_is_still_a_duplicate() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_000).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt, first.receipt);
    // An exact replay writes nothing; the next commit's drop defers it.
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(1_000)
    );
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
}
#[test]
fn backwards_host_time_does_not_shorten_an_event_window() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let late = e.admit_event("host", event("late", 2), 10).unwrap();
    assert!(!late.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&late.key].accepted_at_ms, 10);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&late.key).copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, format!("e:{}", late.key))));
}
/// A window written with a host time behind the clock, but whose window
/// still reaches past it, counts from the clock.
#[test]
fn a_keyed_window_written_behind_the_clock_counts_from_the_clock() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit_once(keyed(&e, "behind"), 4_500).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["behind"].accepted_at_ms, 4_500);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("behind").copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, "k:behind".to_string())));
}

// --- Task 6 fix round 1 ---------------------------------------------------

/// Review I1 (probe shape): an expired event that a live wait still holds,
/// delivered again with no commit in between, is a duplicate every time, as
/// it is inside its window. Changed content still conflicts.
#[test]
fn a_held_expired_event_delivered_again_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    for at in [999, 1_000, 5_000, 50_000] {
        let again = e.admit_event("host", event("held", 1), at).unwrap();
        assert!(again.duplicate, "at {at}");
        assert_eq!(again.key, held.key, "at {at}");
    }
    let changed = EventEnvelope {
        subject: Some("other".into()),
        ..event("held", 1)
    };
    assert!(matches!(
        e.admit_event("host", changed, 50_001),
        Err(Error::Conflict(_))
    ));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}
/// A trigger whose destination cannot be fanned out (its dependency bytes
/// are missing), so the accepted event keeps a queued destination.
fn missing_dependency_trigger_artifact() -> Artifact {
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
    Artifact::seal(parent).unwrap()
}
/// Review I1, aq-server's quantum: the outbox re-sends an event whose fanout
/// is not complete, then fans out and ticks. Once that event's window has
/// passed (it is held by its queued destination), the re-send must not stop
/// the quantum, so an unrelated run still progresses in it.
#[test]
fn redelivering_a_held_expired_event_does_not_stop_the_quantum() {
    let e = ret_engine();
    install(&e, missing_dependency_trigger_artifact(), "parent");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let held = e.admit_event("host", event("outbox", 2), 0).unwrap();
    assert_eq!(
        e.fanout(&held.key, 10, 0).unwrap(),
        0,
        "the destination stays queued"
    );
    let unrelated = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    // Event, then Fanout, then Tick, each failing the quantum on error.
    let quantum = |at: i64| -> Result<()> {
        let again = e.admit_event("host", event("outbox", 2), at)?;
        assert!(again.duplicate, "at {at}");
        e.fanout(&held.key, 10, at)?;
        e.drive(&mut FakeHost::default(), at, 20)?;
        Ok(())
    };
    quantum(5_000).unwrap();
    assert!(
        !e.snapshot().unwrap().runs.contains_key(&unrelated),
        "the unrelated run finished"
    );
    assert_eq!(runs_page(&e, Some(&unrelated))[0]["outcome"], "ok");
    quantum(50_000).unwrap();
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}
/// Review I2: a held event is deferred by its window each time, never by its
/// age, so it leaves within one window after its need ends.
#[test]
fn an_event_held_for_many_windows_leaves_within_one_window_after_its_need_ends() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap(); // window 1_000 (contract)
    let expiry = |e: &Engine<MemoryStorage>| {
        e.snapshot()
            .unwrap()
            .retention
            .event_expires_at_ms
            .get(&held.key)
            .copied()
    };
    assert_eq!(expiry(&e), Some(1_000));
    // A commit at each expiry defers it by one window.
    for k in 1..=10 {
        e.admit("a", "main", Value::Null, authority(), k * 1_000)
            .unwrap();
        assert_eq!(expiry(&e), Some((k + 1) * 1_000), "deferral {k}");
    }
    // The need ends: the wait times out and the waiter leaves.
    e.drive(&mut FakeHost::default(), 10_050, 40).unwrap();
    assert!(!e.snapshot().unwrap().runs.contains_key(&waiter));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
    e.admit("a", "main", Value::Null, authority(), 11_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().events.contains_key(&held.key));
}
/// NR-1, both halves under a short window (1_000): a wait registered inside
/// the window matches the earlier event; one registered at its expiry times
/// out, because the admitting commit drops the event first.
#[test]
fn a_wait_inside_the_window_matches_and_one_after_it_times_out() {
    for (at, outcome) in [(999, "ok"), (1_000, "error")] {
        let e = engine();
        install(&e, wait_artifact(), "waiter");
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.admit_event("host", event("e1", 2), 0).unwrap();
        let id = e
            .admit("waiter", "main", Value::Null, authority(), at)
            .unwrap();
        e.drive(&mut FakeHost::default(), at, 20).unwrap();
        if outcome == "error" {
            assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
            e.drive(&mut FakeHost::default(), at + 101, 20).unwrap();
        }
        assert_eq!(
            runs_page(&e, Some(&id))[0]["outcome"],
            outcome,
            "wait at {at}"
        );
    }
}

// --- Task 7: stale requests refused by producer time ----------------------

#[test]
fn stale_producer_time_is_refused_and_missing_time_is_accepted_as_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 600_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let now = 1_000_000;
    assert!(stale(e.admit_once_at(
        keyed(&e, "k1"),
        Some(now - 600_001),
        now
    )));
    // Ruling 1: the cut-off millisecond itself is stale.
    assert!(stale(e.admit_once_at(
        keyed(&e, "k1"),
        Some(now - 600_000),
        now
    )));
    let first = e
        .admit_once_at(keyed(&e, "k1"), Some(now - 599_999), now)
        .unwrap();
    assert!(!first.duplicate);
    assert!(
        e.admit_once_at(keyed(&e, "k1"), Some(0), now + 1)
            .unwrap()
            .duplicate
    );
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(now - 600_001),
        now
    )));
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(now - 600_000),
        now
    )));
    assert!(
        !e.admit_event_at("host", event("e1", 2), 0, Some(now - 599_999), now)
            .unwrap()
            .duplicate
    );
    e.advance(&first.receipt.run_id, now + 2).unwrap();
    let later = now + 600_001;
    e.admit("a", "main", Value::Null, authority(), later)
        .unwrap(); // drops k1
    let again = e.admit_once_at(keyed(&e, "k1"), None, later + 1).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
}
/// A stale refusal: the typed error, whose text starts with `STALE_REQUEST`.
fn stale(result: Result<impl std::fmt::Debug>) -> bool {
    matches!(result, Err(e @ Error::Stale { .. }) if e.to_string().starts_with(STALE_REQUEST))
}
/// Carried item 1: an expired key or event the engine still holds in the
/// record (no commit has dropped it yet) is forgotten by the first of two
/// commits. A stale request is refused before that commit: nothing is
/// written and the expired entry stays.
fn a_stale_request_for_an_expired_entry_writes_nothing<S: Storage>(
    e: &Engine<S>,
    event_first: bool,
) {
    install_on(e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1, // the event's window is the contract's 1_000
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed_on(e, "k"), 0).unwrap();
    assert!(e.advance(&first.receipt.run_id, 0).unwrap());
    let accepted = e.admit_event("host", event("e1", 2), 0).unwrap();
    let before = e.store.load("test").unwrap();
    assert!(before.state.keyed_admissions.contains_key("k"));
    assert!(before.state.events.contains_key(&accepted.key));
    // Judged against max(2_000, clock 0) - 1_000 = 1_000, which is itself
    // stale (ruling 1).
    for requested in [999, 1_000] {
        assert!(stale(e.admit_once_at(
            keyed_on(e, "k"),
            Some(requested),
            2_000
        )));
        assert!(stale(e.admit_event_at(
            "host",
            event("e1", 2),
            0,
            Some(requested),
            2_000
        )));
    }
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.state.keyed_admissions["k"], first.receipt);
    assert!(after.state.events.contains_key(&accepted.key));
    assert_eq!(
        after.state.retention.clock_ms,
        before.state.retention.clock_ms
    );
    // A request time inside the window: the expired entries leave and the
    // repeats are new. The first of the two is forgotten by its own request
    // (the forget-then-new path); the second was dropped by that commit.
    let keyed_again = || {
        let again = e
            .admit_once_at(keyed_on(e, "k"), Some(1_001), 2_000)
            .unwrap();
        assert!(!again.duplicate);
        assert_ne!(again.receipt.run_id, first.receipt.run_id);
    };
    let event_again = || {
        let redelivered = e
            .admit_event_at("host", event("e1", 2), 0, Some(1_001), 2_000)
            .unwrap();
        assert!(!redelivered.duplicate);
        assert_eq!(
            e.snapshot().unwrap().events[&redelivered.key].accepted_at_ms,
            2_000
        );
    };
    if event_first {
        event_again();
        keyed_again();
    } else {
        keyed_again();
        event_again();
    }
}
#[test]
fn memory_storage_stale_request_for_an_expired_entry_writes_nothing() {
    for event_first in [false, true] {
        a_stale_request_for_an_expired_entry_writes_nothing(&ret_engine(), event_first);
    }
}
#[test]
fn record_storage_stale_request_for_an_expired_entry_writes_nothing() {
    for event_first in [false, true] {
        let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&r, done_artifact(), "a");
        a_stale_request_for_an_expired_entry_writes_nothing(&r, event_first);
    }
}
/// Carried item 3: an entry the engine still remembers answers as today,
/// whatever the request time: inside its window, after it while its run is
/// live, and an expired event that something still holds.
#[test]
fn a_remembered_entry_answers_as_today_whatever_its_request_time() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    // Inside the window.
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    let inside = e
        .admit_once_at(keyed(&e, "k"), Some(i64::MIN), 500)
        .unwrap();
    assert!(inside.duplicate);
    assert_eq!(inside.receipt, first.receipt);
    let changed = KeyedAdmissionRequest {
        input: json!(1),
        ..keyed(&e, "k")
    };
    assert!(matches!(
        e.admit_once_at(changed, Some(i64::MIN), 500),
        Err(Error::Conflict(_))
    ));
    // Expired, but its run is still live (SC-10).
    let live = e
        .admit_once_at(keyed(&e, "k"), Some(i64::MIN), 5_000)
        .unwrap();
    assert!(live.duplicate);
    assert_eq!(live.receipt, first.receipt);
    // An expired event held by a live waiter (Task 6 ruling).
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.advance(&waiter, 5_000).unwrap();
    let held = e.admit_event("host", event("held", 1), 5_000).unwrap();
    for at in [6_000, 50_000] {
        let again = e
            .admit_event_at("host", event("held", 1), 0, Some(i64::MIN), at)
            .unwrap();
        assert!(again.duplicate, "at {at}");
        assert_eq!(again.key, held.key, "at {at}");
    }
}
/// Request time is not part of any digest or stored record, and it never
/// moves the retention clock, even when it lies in the future. A future
/// request time is never stale, and it cannot make a remembered entry new.
#[test]
fn request_time_enters_no_digest_record_or_clock() {
    let script = |requested: Option<i64>| {
        let e = ret_engine();
        install(&e, wait_artifact(), "waiter");
        let k = e.admit_once_at(keyed(&e, "k"), requested, 1_000).unwrap();
        let ev = e
            .admit_event_at("host", event("e1", 2), 0, requested, 1_000)
            .unwrap();
        assert!(!k.duplicate && !ev.duplicate);
        // A remembered key stays a duplicate.
        assert!(
            e.admit_once_at(keyed(&e, "k"), requested, 1_001)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_event_at("host", event("e1", 2), 0, requested, 1_001)
                .unwrap()
                .duplicate
        );
        (k, ev, e.snapshot().unwrap())
    };
    let (k_none, ev_none, s_none) = script(None);
    let (k_future, ev_future, s_future) = script(Some(i64::MAX));
    let (k_now, _, s_now) = script(Some(1_000));
    assert_eq!(k_future.receipt, k_none.receipt);
    assert_eq!(k_now.receipt, k_none.receipt);
    assert_eq!(ev_future.key, ev_none.key);
    // The last host time (an event duplicate commits), not the request time.
    assert_eq!(s_future.retention.clock_ms, Some(1_001));
    let json = |s: &State| serde_json::to_value(s).unwrap();
    assert_eq!(json(&s_future), json(&s_none));
    assert_eq!(json(&s_now), json(&s_none));
    // The oldest possible request time does not overflow the check.
    let e = ret_engine();
    assert!(stale(e.admit_once_at(
        keyed(&e, "x"),
        Some(i64::MIN),
        1_000
    )));
}
/// Carried item 2: staleness is judged against the retention clock, as
/// expiry is. One far-future host time pins the clock, and then every
/// request with a real request time is refused as stale; the message names
/// both times. Requests without a request time still go through.
#[test]
fn a_pinned_clock_refuses_every_request_time_and_says_so() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    let pinned: i64 = 9_000_000_000_000;
    let real: i64 = 1_700_000_000_000;
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    for result in [
        e.admit_once_at(keyed(&e, "k"), Some(real), real)
            .map(|_| ()),
        e.admit_event_at("host", event("e1", 2), 0, Some(real), real)
            .map(|_| ()),
    ] {
        let Err(error) = result else {
            panic!("expected a stale refusal, got {result:?}");
        };
        let Error::Stale {
            requested_at_ms,
            clock_ms,
            now_ms,
            ..
        } = error
        else {
            panic!("expected Error::Stale, got {error:?}");
        };
        assert_eq!(
            (requested_at_ms, clock_ms, now_ms),
            (real, Some(pinned), real)
        );
        let message = error.to_string();
        assert!(message.starts_with(STALE_REQUEST), "{message}");
        assert!(message.contains(&real.to_string()), "{message}");
        assert!(message.contains(&pinned.to_string()), "{message}");
        assert!(message.contains("retention clock"), "{message}");
        assert!(message.contains("ahead of host time"), "{message}");
    }
    assert!(
        !e.admit_once_at(keyed(&e, "k"), None, real)
            .unwrap()
            .duplicate
    );
    assert!(
        !e.admit_event_at("host", event("e1", 2), 0, None, real)
            .unwrap()
            .duplicate
    );
    // The existing methods carry no request time, so they are not checked.
    assert!(!e.admit_once(keyed(&e, "k2"), real).unwrap().duplicate);
    assert!(
        !e.admit_event("host", event("e2", 2), real)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(pinned));
}
/// Review I1 (ruling 1): a key accepted at window start S expires at S + w.
/// A retry carrying its original request time S that lands exactly on the
/// expiry millisecond is stale, not admitted a second time.
#[test]
fn a_retry_at_the_expiry_millisecond_is_stale_not_new() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1, // the event's window is the contract's 1_000
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_000).unwrap();
    assert!(e.advance(&first.receipt.run_id, 1_000).unwrap());
    let accepted = e
        .admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(2_000)
    );
    assert_eq!(
        s.retention.event_expires_at_ms.get(&accepted.key).copied(),
        Some(2_000)
    );
    // At 2_000 both are expired and unheld: not remembered, and the retry
    // is stale.
    assert!(stale(e.admit_once_at(keyed(&e, "k"), Some(1_000), 2_000)));
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(1_000),
        2_000
    )));
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["k"], first.receipt);
    assert!(s.events.contains_key(&accepted.key));
    // One millisecond earlier they are still remembered: duplicates.
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_000).unwrap();
    e.advance(&first.receipt.run_id, 1_000).unwrap();
    e.admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_000)
        .unwrap();
    assert!(
        e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_999)
            .unwrap()
            .duplicate
    );
    assert!(
        e.admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_999)
            .unwrap()
            .duplicate
    );
}
/// Ruling 2: hosts match the variant, which carries the request time, the
/// clock, the host time, the window and the cut-off.
#[test]
fn a_stale_refusal_is_typed_and_names_its_times() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let error = e
        .admit_once_at(keyed(&e, "k"), Some(3_000), 4_500)
        .unwrap_err();
    let Error::Stale {
        requested_at_ms,
        cutoff_ms,
        clock_ms,
        now_ms,
        window_ms,
        floor_ms,
    } = error
    else {
        panic!("expected Error::Stale, got {error:?}");
    };
    assert_eq!(
        (
            requested_at_ms,
            cutoff_ms,
            clock_ms,
            now_ms,
            window_ms,
            floor_ms
        ),
        (3_000, 4_000, Some(5_000), 4_500, 1_000, None)
    );
    let message = error.to_string();
    assert!(message.starts_with(STALE_REQUEST), "{message}");
    for part in ["3000", "4000", "5000", "4500", "1000"] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert!(!matches!(error, Error::Refused(_)));
}

// --- Task 7 round 1, ruling 3: rebasing a pinned retention clock ----------

/// The pinned-clock scenario (ruling 3) on any backend. Windows: keyed
/// 1_000, events max(1, contract 1_000). Real time starts at `t0`.
fn pinned_clock_is_rebased_and_normal_operation_resumes<S: Storage>(e: &Engine<S>) {
    install_on(e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let t0: i64 = 10_000;
    let pinned: i64 = t0 + 1_000_000_000;
    // Normal operation: a finished keyed run, a keyed run still live, a
    // waiter holding an event, and an unheld event.
    let done = e.admit_once_at(keyed_on(e, "done"), Some(t0), t0).unwrap();
    assert!(e.advance(&done.receipt.run_id, t0).unwrap());
    let live = e.admit_once_at(keyed_on(e, "live"), Some(t0), t0).unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), t0)
        .unwrap();
    e.advance(&waiter, t0).unwrap();
    let held = e
        .admit_event_at("host", event("held", 1), 0, Some(t0), t0)
        .unwrap();
    e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(t0),
        t0,
    )
    .unwrap();
    // One far-future host time pins the clock. Its drop forgets "done" and
    // "free" a whole window early; "live" and "held" are deferred from the
    // pin.
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(pinned));
    assert!(!s.keyed_admissions.contains_key("done"));
    assert_eq!(s.events.len(), 1);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("live").copied(),
        Some(pinned + 1_000)
    );
    // Back in real time: every request that carries a request time is
    // stale, and the refusal writes nothing.
    let t1 = t0 + 200;
    let before = e.snapshot().unwrap();
    let refused = e
        .admit_once_at(keyed_on(e, "during"), Some(t1), t1)
        .unwrap_err();
    assert!(
        matches!(refused, Error::Stale { clock_ms: Some(c), now_ms, .. } if c == pinned && now_ms == t1)
    );
    assert!(refused.to_string().contains("ahead of host time"));
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("during-e", "inv-during"),
        0,
        Some(t1),
        t1
    )));
    let json = |s: &State| serde_json::to_value(s).unwrap();
    assert_eq!(json(&e.snapshot().unwrap()), json(&before));
    // Requests without a request time go through, and what they write
    // expires from the pin: the record only grows.
    for i in 0..5 {
        let d = e
            .admit_once(keyed_on(e, &format!("pin-{i}")), t1 + i)
            .unwrap();
        e.advance(&d.receipt.run_id, t1 + i).unwrap();
    }
    // An unheld event accepted during the pin, of a contract whose window
    // (1_000) is longer than the policy's (1).
    let pin_event = e
        .admit_event("host", correlated_event("pin-e", "inv-pin"), t1)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), t1 + 5_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.len(),
        6,
        "pin-0..4 and live never expire"
    );
    assert!(
        s.retention
            .admission_expires_at_ms
            .get("pin-0")
            .is_some_and(|at| *at > pinned)
    );
    // The rebase: refused unless now <= to <= now + slack (0 here: a tenth
    // of the smallest window, 1 ms) and the floor is in the past.
    let t2 = t0 + 500;
    for (to, floor, now) in [
        (pinned, t0, t2),
        (pinned + 1, t0, t2),
        (t2 - 1, t0, t2),
        (t2 + 1, t0, t2),
        (t2, t2 + 1, t2),
    ] {
        assert!(
            matches!(
                e.rebase_retention_clock(to, floor, "ops", "pinned clock", now),
                Err(Error::Refused(_))
            ),
            "to {to} floor {floor} now {now}"
        );
    }
    assert!(
        e.rebase_retention_clock(t2, t0, "", "pinned clock", t2)
            .is_err()
    );
    assert_eq!(json(&e.snapshot().unwrap()), json(&s));
    // A dry run reports the same and commits nothing.
    let preview = e
        .rebase_retention_clock_dry_run(t2, t0, "ops", "pinned clock", t2)
        .unwrap();
    assert_eq!(json(&e.snapshot().unwrap()), json(&s));
    let report = e
        .rebase_retention_clock(t2, t0, "ops", "pinned clock", t2)
        .unwrap();
    assert_eq!(preview, report);
    assert_eq!(
        (report.from_ms, report.to_ms, report.stale_floor_ms),
        (pinned, t2, t0)
    );
    assert_eq!((report.capped_admissions, report.capped_events), (6, 2));
    // The run admitted at the pinned time has its duration basis capped.
    assert!(report.needs_operator_action.is_empty());
    assert!(!report.adjusted_runs.is_empty());
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(t2));
    assert_eq!(s.retention.stale_floor_ms, Some(t0));
    for key in ["live", "pin-0", "pin-4"] {
        assert_eq!(
            s.retention.admission_expires_at_ms.get(key).copied(),
            Some(t2 + 1_000),
            "{key}"
        );
    }
    assert_eq!(
        s.retention.event_expires_at_ms.get(&held.key).copied(),
        Some(t2 + 1_000)
    );
    // Capped by its contract's window, not the policy's.
    assert_eq!(
        s.retention.event_expires_at_ms.get(&pin_event.key).copied(),
        Some(t2 + 1_000)
    );
    assert!(window_entries(&s).iter().all(|(at, _)| *at <= t2 + 1_000));
    for id in &report.adjusted_runs {
        assert_eq!(s.runs[id].admitted_at_ms, t2, "{id}");
    }
    // The receipt is history: who, when, the old and new clock.
    let receipts = inspect_all(e, InspectionKind::Receipts, Some("admin"));
    let rebase = receipts
        .iter()
        .find(|r| r["kind"] == retention::CLOCK_REBASE)
        .expect("rebase receipt in history");
    assert_eq!(rebase["at_ms"], t2);
    assert!(s.receipts.iter().all(|r| r.kind != retention::CLOCK_REBASE));
    // Normal operation resumes. A request time is judged against real time
    // again: one created during the pin is new now.
    let during = e
        .admit_once_at(keyed_on(e, "during"), Some(t1), t2)
        .unwrap();
    assert!(!during.duplicate);
    assert!(
        !e.admit_event_at(
            "host",
            correlated_event("during-e", "inv-during"),
            0,
            Some(t1),
            t2
        )
        .unwrap()
        .duplicate
    );
    // Duplicates inside their window are still recognised.
    assert!(
        e.admit_once_at(keyed_on(e, "during"), Some(t1), t2 + 10)
            .unwrap()
            .duplicate
    );
    assert!(
        e.admit_once_at(keyed_on(e, "pin-0"), Some(t1), t2 + 10)
            .unwrap()
            .duplicate
    );
    // Round 3: after an ordinary commit, a key still inside its (capped)
    // window is still remembered.
    e.admit("a", "main", Value::Null, authority(), t2 + 500)
        .unwrap();
    assert!(
        e.admit_once_at(keyed_on(e, "pin-0"), Some(t1), t2 + 999)
            .unwrap()
            .duplicate
    );
    // An event inside its contract window is still a duplicate.
    assert!(
        e.admit_event("host", correlated_event("pin-e", "inv-pin"), t2 + 10)
            .unwrap()
            .duplicate
    );
    // Held events and live runs are intact.
    assert!(
        e.admit_event_at("host", event("held", 1), 0, Some(t0), t2 + 10)
            .unwrap()
            .duplicate
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&waiter].status, RunStatus::Waiting);
    assert!(s.runs.contains_key(&live.receipt.run_id));
    assert!(
        e.admit_once_at(keyed_on(e, "live"), Some(t0), t2 + 10)
            .unwrap()
            .duplicate
    );
    // The floor: "done" was forgotten a window early by the pin. A retry
    // with its original request time is stale, not a second run. Without a
    // request time it is new, as for any key the engine has forgotten.
    let floor = e
        .admit_once_at(keyed_on(e, "done"), Some(t0), t2 + 10)
        .unwrap_err();
    assert!(
        matches!(floor, Error::Stale { floor_ms: Some(f), cutoff_ms, .. } if f == t0 && cutoff_ms == t0)
    );
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(t0),
        t2 + 10
    )));
    let again = e.admit_once(keyed_on(e, "done"), t2 + 10).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, done.receipt.run_id);
    // The record stops growing: what the pin wrote expires in real time.
    for run in [
        &live.receipt.run_id,
        &during.receipt.run_id,
        &again.receipt.run_id,
    ] {
        assert!(e.advance(run, t2 + 20).unwrap());
    }
    for i in 0..10 {
        let at = t2 + 1_001 + i * 200;
        let d = e
            .admit_once_at(keyed_on(e, &format!("after-{i}")), Some(at), at)
            .unwrap();
        e.advance(&d.receipt.run_id, at).unwrap();
    }
    let s = e.snapshot().unwrap();
    let mut keys: Vec<String> = s.keyed_admissions.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        (5..10).map(|i| format!("after-{i}")).collect::<Vec<_>>(),
        "only the last window's keys stay"
    );
    assert!(window_entries(&s).len() <= 7, "{:?}", window_entries(&s));
}
#[test]
fn memory_storage_pinned_clock_is_rebased_and_normal_operation_resumes() {
    pinned_clock_is_rebased_and_normal_operation_resumes(&ret_engine());
}
#[test]
fn record_storage_pinned_clock_is_rebased_and_normal_operation_resumes() {
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&r, done_artifact(), "a");
    pinned_clock_is_rebased_and_normal_operation_resumes(&r);
}
/// Ruling 2 (round 2): a mistaken rebase cannot do lasting damage. It
/// needs a clock pinned by more than the smallest window, a target from
/// host time to the slack after it, a floor in the past, an actor and a
/// reason; a second rebase never lowers the floor; and a floor set too high
/// can be lowered with its own receipt.
#[test]
fn rebase_preconditions_and_the_floor() {
    let e = ret_engine();
    // Default windows: 1 day and 7 days, so the slack is one minute.
    assert!(matches!(
        e.rebase_retention_clock(10, 5, "ops", "r", 10),
        Err(Error::Refused(_))
    ));
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    // Not pinned: ahead by 1 s, less than the smallest window.
    let not_pinned = e
        .rebase_retention_clock(4_000, 3_000, "ops", "r", 4_000)
        .unwrap_err();
    assert!(
        not_pinned.to_string().contains("not pinned"),
        "{not_pinned}"
    );
    let pinned = 1_000_000_000_000;
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let now = 1_000_000;
    for (to, floor, actor, reason) in [
        (now + 60_001, now - 1, "ops", "r"),
        (now - 1, now - 1, "ops", "r"),
        (now, now + 1, "ops", "r"),
        // A floor after the host time, though not after the target.
        (now + 60_000, now + 1, "ops", "r"),
        (now, now - 1, "ops", ""),
        (now, now - 1, "", "r"),
    ] {
        assert!(
            matches!(
                e.rebase_retention_clock(to, floor, actor, reason, now),
                Err(Error::Refused(_))
            ),
            "to {to} floor {floor}"
        );
    }
    let report = e
        .rebase_retention_clock(now + 60_000, now - 100, "ops", "r", now)
        .unwrap();
    assert_eq!(report.stale_floor_ms, now - 100);
    // A second pin and rebase with a lower floor keeps the higher one.
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let later = now + 100_000;
    let report = e
        .rebase_retention_clock(later, now - 500, "ops", "r", later)
        .unwrap();
    assert_eq!(report.stale_floor_ms, now - 100);
    // Lowering the floor: only below the current one, with its own receipt,
    // and nothing else changes.
    // Not below the current floor: refused by the engine itself.
    for to in [now - 100, now] {
        assert!(matches!(
            e.lower_stale_floor(to, "ops", "r", later),
            Err(Error::Refused(_))
        ));
    }
    assert!(e.lower_stale_floor(now - 200, "ops", "", later).is_err());
    let before = e.snapshot().unwrap();
    assert_eq!(
        e.lower_stale_floor(now - 200, "ops", "too high", later)
            .unwrap(),
        (now - 100, now - 200)
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.stale_floor_ms, Some(now - 200));
    assert_eq!(s.retention.clock_ms, before.retention.clock_ms);
    assert_eq!(s.runs.len(), before.runs.len());
    let receipts = inspect_all(&e, InspectionKind::Receipts, Some("admin"));
    assert!(
        receipts
            .iter()
            .any(|r| r["kind"] == retention::STALE_FLOOR_LOWERED)
    );
    // No floor to lower.
    let fresh = ret_engine();
    assert!(fresh.lower_stale_floor(0, "ops", "r", 1).is_err());
}
/// Ruling 3 (round 2): a wait registered at the pinned time gets its
/// deadline capped at the target plus its declared duration, so it times
/// out normally after the rebase, and the run settles and leaves the
/// record. An operation deadline set then is capped the same way.
#[test]
fn a_wait_registered_at_the_pinned_time_times_out_after_the_rebase() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&waiter, pinned).unwrap();
    let invoke = e
        .admit("invoke", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&invoke, pinned).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&waiter].status, RunStatus::Waiting);
    let wait = s.waits.values().find(|w| w.run == waiter).unwrap().clone();
    assert_eq!(wait.deadline, pinned + 100);
    let intent = s
        .intents
        .values()
        .find(|i| i.run == invoke)
        .unwrap()
        .clone();
    assert_eq!(intent.deadline_at_ms, pinned + 100);
    let t2: i64 = 10_000;
    let report = e
        .rebase_retention_clock(t2, t2 - 1_000, "ops", "pinned clock", t2)
        .unwrap();
    // Each run's duration basis, the wait and the operation deadline.
    assert_eq!(report.capped_deadlines, 4);
    assert_eq!(report.adjusted_runs, {
        let mut ids = vec![waiter.clone(), invoke.clone()];
        ids.sort();
        ids
    });
    assert!(report.needs_operator_action.is_empty());
    let s = e.snapshot().unwrap();
    assert_eq!(s.waits[&wait.id].deadline, t2 + 100);
    assert_eq!(s.intents[&intent.id].deadline_at_ms, t2 + 100);
    assert_eq!(s.runs[&waiter].admitted_at_ms, t2);
    // The receipt (in full, as the commit wrote it) names the runs.
    let rebase = e
        .store
        .settled_cohorts("test")
        .into_iter()
        .flat_map(|c| c.receipts)
        .find(|r| r.kind == retention::CLOCK_REBASE)
        .unwrap();
    assert_eq!(rebase.detail["capped_deadlines"], 4);
    // N2 (round 3): each capped value with its original, for audit.
    let adjustment =
        |run: &str, kind: &str, item: &str, original_ms: i64, capped_ms: i64| DeadlineAdjustment {
            run: run.into(),
            kind: kind.into(),
            item: item.into(),
            original_ms,
            capped_ms,
        };
    let mut expected = vec![
        adjustment(&waiter, "duration_basis", &waiter, pinned, t2),
        adjustment(&waiter, "wait", &wait.id, pinned + 100, t2 + 100),
        adjustment(&invoke, "duration_basis", &invoke, pinned, t2),
        adjustment(&invoke, "operation", &intent.id, pinned + 100, t2 + 100),
    ];
    expected.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    let mut got = report.adjustments.clone();
    got.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    assert_eq!(got, expected);
    assert_eq!(report.adjustments_not_listed, 0);
    assert_eq!(rebase.detail["adjustments"], json!(report.adjustments));
    assert_eq!(rebase.detail["adjusted_runs"], json!(report.adjusted_runs));
    assert_eq!(rebase.detail["needs_operator_action"], json!([]));
    // Before its deadline the wait still waits; after it, it times out, the
    // run settles and leaves the record.
    e.drive(&mut FakeHost::default(), t2 + 99, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    e.drive(&mut FakeHost::default(), t2 + 101, 20).unwrap();
    assert!(!e.snapshot().unwrap().runs.contains_key(&waiter));
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "error");
}
/// A deadline whose declared duration cannot be found (here a retry on a
/// step with no retry policy, written directly) is left as it is and its
/// run listed for the operator.
#[test]
fn a_deadline_without_its_declared_duration_needs_operator_action() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let run = e
        .admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let mut s = e.snapshot().unwrap();
    {
        let mut r = s.runs[&run].clone();
        r.status = RunStatus::Waiting;
        r.pending = Some(Pending::Retry {
            deadline: pinned + 5_000,
        });
        s.runs.insert(run.clone(), r);
    }
    overwrite(&e.store, "incarnation-1", &s);
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert_eq!(report.needs_operator_action, vec![run.clone()]);
    let s = e.snapshot().unwrap();
    assert!(matches!(
        s.runs[&run].pending,
        Some(Pending::Retry { deadline }) if deadline == pinned + 5_000
    ));
}
/// A business retry scheduled at the pinned time is capped at the target
/// plus its declared backoff.
#[test]
fn a_retry_scheduled_at_the_pinned_time_is_capped_by_its_backoff() {
    let e = ret_engine();
    install(
        &e,
        invoke_artifact(
            command(Recovery::KeyedIdempotent),
            Some(RetryPolicy {
                max_attempts: 2,
                backoff_ms: 10_000,
                outcomes: set(&["retry"]),
            }),
        ),
        "invoke",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let id = e
        .admit("invoke", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&id, pinned).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "retry".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, pinned).unwrap();
    e.advance(&id, pinned).unwrap();
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Retry { deadline }) if deadline == pinned + 10_000
    ));
    let t2: i64 = 10_000;
    let report = e
        .rebase_retention_clock(t2, t2 - 1, "ops", "pinned clock", t2)
        .unwrap();
    assert!(report.adjusted_runs.contains(&id));
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Retry { deadline }) if deadline == t2 + 10_000
    ));
}
/// A join set at the pinned time (written directly: its step's declared
/// deadline is 5_000) is capped at the target plus that duration.
#[test]
fn a_join_set_at_the_pinned_time_is_capped_by_its_declared_deadline() {
    let e = engine();
    install(
        &e,
        artifact(
            "joiner",
            "join",
            BTreeMap::from([
                (
                    "join".into(),
                    Step {
                        op: Operation::Join {
                            member_keys: vec![],
                            members: vec![Expr::literal(json!({"child": "x"}))],
                            mode: JoinMode::All,
                            accepted_outcomes: set(&["ok"]),
                            deadline_ms: 5_000,
                            remaining: RemainingPolicy::Cancel,
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
            Registry::default(),
            BTreeMap::new(),
        ),
        "joiner",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let run = e
        .admit("joiner", "main", Value::Null, authority(), pinned)
        .unwrap();
    let mut s = e.snapshot().unwrap();
    {
        let mut r = s.runs[&run].clone();
        r.status = RunStatus::Waiting;
        r.pending = Some(Pending::Join {
            members: vec![],
            member_keys: vec![],
            mode: JoinMode::All,
            accepted: set(&["ok"]),
            deadline: pinned + 5_000,
            remaining: RemainingPolicy::Cancel,
            bind: "joined".into(),
        });
        s.runs.insert(run.clone(), r);
    }
    overwrite(&e.store, "incarnation-1", &s);
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert!(report.adjusted_runs.contains(&run));
    assert!(report.needs_operator_action.is_empty());
    assert!(matches!(
        e.snapshot().unwrap().runs[&run].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 15_000
    ));
}
/// The owned-operation join fixture of `semantics.rs`.
fn async_artifact() -> (Artifact, EventContract) {
    async_artifact_with(Recovery::Reconcilable)
}
/// [`async_artifact`] whose command recovers by `recovery`.
fn async_artifact_with(recovery: Recovery) -> (Artifact, EventContract) {
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
    let mut c = command(recovery);
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
/// N4 (round 3): a join driven at the pinned time (an accepted operation,
/// then `Join` with a 100 ms deadline) is capped at the target plus that
/// deadline.
#[test]
fn a_driven_join_at_the_pinned_time_is_capped_by_its_declared_deadline() {
    let e = ret_engine();
    let (a, _) = async_artifact();
    install(&e, a, "async");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let id = e
        .admit("async", "main", Value::Null, authority(), pinned)
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
    e.drive(&mut host, pinned, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].step, "join");
    assert!(matches!(
        s.runs[&id].pending,
        Some(Pending::Join { deadline, .. }) if deadline == pinned + 100
    ));
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert!(report.adjusted_runs.contains(&id));
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 10_100
    ));
}

// --- Task 8: quotas count live work

#[test]
fn max_tombstones_and_terminal_run_ms_are_accepted_and_ignored() {
    let e = engine();
    e.set_retention_policy(RetentionPolicy {
        max_tombstones: 0,
        terminal_run_ms: 0,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.policy.max_tombstones, 0);
}
#[test]
fn retention_windows_must_be_positive() {
    let e = engine();
    for policy in [
        RetentionPolicy {
            event_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            admission_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            audit_ms: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(
            matches!(e.set_retention_policy(policy), Err(Error::Refused(m)) if m == "retention windows must be positive")
        );
    }
}
/// A `max_tombstones` of 1 stops neither a GC that forgets three events nor
/// a policy change (the field is ignored), and GC writes no tombstone.
#[test]
fn max_tombstones_stops_neither_gc_nor_policy_changes_and_gc_writes_no_tombstone() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let policy = RetentionPolicy {
        event_replay_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    };
    e.set_retention_policy(policy.clone()).unwrap();
    for i in 0..3 {
        e.admit_event("host", event(&format!("old-{i}"), 1), 0)
            .unwrap();
    }
    assert_eq!(e.collect_garbage(2_000, 10).unwrap().events, 3);
    assert!(e.snapshot().unwrap().retention.event_tombstones.is_empty());
    e.set_retention_policy(policy).unwrap();
}
/// A namespace written before this change holds finished runs until the
/// drain moves them to history (Task 9; before it, GC moved them out by
/// `audit_ms`). They never make new work wait, whatever a host's deprecated
/// `max_runs` says (Task 8b, no limits): new work is admitted, a live run
/// finishes and leaves, and the drain moves the finished runs out (GC,
/// which would remove them without history, leaves them to it).
#[test]
fn a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains() {
    let legacy = 51;
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "i-1",
        EngineConfig {
            max_runs: legacy + 1,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    install(&e, done_artifact(), "a");
    e.set_retention_policy(RetentionPolicy {
        terminal_run_ms: u64::MAX,
        audit_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let live = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let mut s = e.snapshot().unwrap();
    for i in 0..legacy {
        let id = format!("old-{i:03}");
        s.runs.insert(id.clone(), completed_run(&id));
        s.sequence += 1;
        s.receipts.push(audit(s.sequence, &id));
    }
    s.indexes.initialized = false;
    overwrite(&e.store, "i-1", &s);
    // The commit that rebuilds the indexes re-arms the drain and takes its
    // first step: 50 of the 51 legacy runs leave. `collect_garbage`, an
    // ordinary commit, takes the next: the last one goes to history.
    let admitted = e.admit("a", "main", Value::Null, authority(), 2).unwrap();
    assert_eq!(e.collect_garbage(1_000, 1_000).unwrap().runs, 1);
    assert!(e.advance(&live, 3).unwrap());
    assert!(e.advance(&admitted, 3).unwrap());
    e.admit("a", "main", Value::Null, authority(), 4).unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
    let history = inspect_all(&e, InspectionKind::Runs, None);
    assert_eq!(
        history
            .iter()
            .filter(|r| r["id"].as_str().unwrap().starts_with("old-"))
            .count(),
        legacy
    );
}
fn ok_host(n: usize) -> FakeHost {
    FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                };
                n
            ],
        )]),
        ..Default::default()
    }
}
/// A pre-D10 namespace written before the retention clock opens (the
/// `root_intents` fill grows the record) and takes its first commits under
/// the smallest deprecated `max_state_bytes` a host could pass: nothing
/// judges the record's size (Task 8b, no limits).
#[test]
fn a_pre_d10_namespace_opens_and_takes_its_first_commits_whatever_its_configured_limits() {
    let e = Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install_on(&e, wait_artifact(), "waiter");
    let mut host = completed_host();
    let (old, _) = run_with_completed_operation(&e, &mut host);
    e.admit_event("host", event("seen", 1), 0).unwrap();
    let mut legacy = vendored_shape(&e.snapshot().unwrap());
    legacy.retention.clock_ms = None;
    e.release().unwrap();
    let store = MemoryStorage::retaining_settled();
    store.acquire("test", "w").unwrap();
    store.compare_and_swap("test", "w", 0, &legacy).unwrap();
    store.release("test", "w").unwrap();
    let e = Engine::with_config(store, "test", "incarnation-2", tiny_limits()).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.indexes.root_intents_complete);
    // The first host-time commit starts the clock: a redelivery inside the
    // event's window, a duplicate that writes nothing else.
    assert!(
        e.admit_event("host", event("seen", 1), 1_000_000)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(1_000_000));
    assert!(e.advance(&old, 1_000_001).unwrap());
}

// --- Task 8b (owner override): no namespace limits

/// Every deprecated namespace limit at its smallest value: a host may still
/// pass them, and nothing reads them.
fn tiny_limits() -> EngineConfig {
    EngineConfig {
        max_runs: 1,
        max_events: 1,
        max_queued_destinations: 1,
        max_state_bytes: 1,
        max_receipts: 1,
    }
}
fn unlimited_engine() -> Engine<MemoryStorage> {
    Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "incarnation-1",
        tiny_limits(),
    )
    .unwrap()
}
/// A chain of `n` decisions, then `ok`: each decision writes a `branch`
/// receipt, so a run holds `n + 1` receipts just before it finishes.
fn chain_artifact(n: usize) -> Artifact {
    let mut steps = BTreeMap::new();
    for i in 0..n {
        let next = if i + 1 == n {
            "ok".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(
            format!("d{i}"),
            Step {
                op: Operation::Decide {
                    predicate: Expr::literal(true),
                },
                edges: edges(&[("true", next.as_str()), ("false", "ok"), ("unknown", "ok")]),
                on_error: None,
            },
        );
    }
    steps.insert("ok".into(), ret("ok"));
    artifact("chain", "d0", steps, Registry::default(), BTreeMap::new())
}
/// Task 8 re-review round 2's deadlock: two parents waiting on a child,
/// with a host's `max_runs` at 2. Nothing counts runs, so both start their
/// children and finish.
#[test]
fn calling_parents_complete_whatever_max_runs_says() {
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "incarnation-1",
        EngineConfig {
            max_runs: 2,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    let child = done_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let parents: Vec<String> = (0..2)
        .map(|t| {
            e.admit("parent", "main", Value::Null, authority(), t)
                .unwrap()
        })
        .collect();
    e.drive(&mut FakeHost::default(), 10, 100).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    for parent in &parents {
        assert_eq!(runs_page(&e, Some(parent))[0]["outcome"], "ok", "{parent}");
    }
}
/// 1,000 root runs admitted and left in progress at once (direct and keyed)
/// are all accepted, then all finish.
#[test]
fn a_thousand_root_runs_in_progress_are_all_accepted_then_all_finish() {
    let e = unlimited_engine();
    install(&e, chain_artifact(2), "chain");
    install(&e, done_artifact(), "a");
    let mut runs = vec![];
    for t in 0..500 {
        runs.push(
            e.admit("chain", "main", Value::Null, authority(), t)
                .unwrap(),
        );
        let d = e.admit_once(keyed(&e, &format!("k{t:03}")), t).unwrap();
        assert!(!d.duplicate);
        runs.push(d.receipt.run_id);
    }
    assert_eq!(e.snapshot().unwrap().runs.len(), 1_000);
    while e.drive(&mut FakeHost::default(), 1_000, 1_000).unwrap() > 0 {}
    assert!(e.snapshot().unwrap().runs.is_empty());
    for id in &runs {
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok", "{id}");
    }
}
/// No commit is refused for namespace capacity: with every deprecated limit
/// at its smallest, a direct and a keyed admission, an event with its
/// fan-out, a child start, a run holding 1,500 receipts, a waiting run's
/// wait, an install and an activation all land.
#[test]
fn nothing_is_refused_for_namespace_capacity() {
    let e = unlimited_engine();
    install(&e, done_artifact(), "a");
    install(&e, trigger_artifact(), "trigger");
    install(&e, wait_artifact(), "waiter");
    install(&e, chain_artifact(1_600), "chain");
    let child = done_artifact();
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let direct = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    let keyed_run = e.admit_once(keyed(&e, "k"), 0).unwrap().receipt.run_id;
    let parent = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    let long = e
        .admit("chain", "main", Value::Null, authority(), 0)
        .unwrap();
    for i in 0..20 {
        let d = e
            .admit_event("host", event(&format!("e{i}"), 1), 1)
            .unwrap();
        assert_eq!(e.fanout(&d.key, 10, 1).unwrap(), 1);
    }
    let mut peak = 0;
    while e.drive(&mut FakeHost::default(), 2, 16).unwrap() > 0 {
        peak = peak.max(e.snapshot().unwrap().receipts.len());
    }
    assert!(peak > 1_500, "{peak}");
    for id in [&direct, &keyed_run, &parent, &long] {
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok", "{id}");
    }
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    let mut other = done_artifact().content;
    other.playbook.purpose = "another".into();
    let other = Artifact::seal(other).unwrap();
    let digest = other.digest.clone();
    e.install(other).unwrap();
    e.activate("other", &digest, authority()).unwrap();
}
/// A stored policy with the deprecated limit fields decodes and behaves as
/// one without them, and an engine given a host's limits behaves as one
/// given none: the same script leaves the same record and history.
#[test]
fn deprecated_limit_fields_decode_and_change_nothing() {
    let without: RetentionPolicy = serde_json::from_value(json!({
        "event_replay_ms": 1_000, "audit_ms": 604_800_000,
    }))
    .unwrap();
    let with: RetentionPolicy = serde_json::from_value(json!({
        "event_replay_ms": 1_000, "audit_ms": 604_800_000,
        "terminal_run_ms": 0, "max_tombstones": 0,
    }))
    .unwrap();
    let config: EngineConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        serde_json::to_value(EngineConfig::default()).unwrap()
    );
    let script = |config: EngineConfig, policy: RetentionPolicy| {
        let e = Engine::with_config(
            MemoryStorage::retaining_settled(),
            "test",
            "incarnation-1",
            config,
        )
        .unwrap();
        install(&e, done_artifact(), "a");
        install(&e, trigger_artifact(), "trigger");
        e.set_retention_policy(policy).unwrap();
        for t in 0..5 {
            e.admit("a", "main", Value::Null, authority(), t).unwrap();
            let d = e
                .admit_event("host", event(&format!("e{t}"), 1), t)
                .unwrap();
            e.fanout(&d.key, 10, t).unwrap();
        }
        e.drive(&mut FakeHost::default(), 3_000, 100).unwrap();
        let mut s = serde_json::to_value(e.snapshot().unwrap()).unwrap();
        for field in ["terminal_run_ms", "max_tombstones"] {
            s["retention"]["policy"][field] = Value::Null;
        }
        (s, runs_page(&e, None))
    };
    let reference = script(EngineConfig::default(), without);
    assert_eq!(script(tiny_limits(), with), reference);
}

// --- Task 8b R1: the eviction licence (fence tests, every runner path) ----

/// Window entries of the eviction fence states never expire (window
/// 100_000, clock 500).
const EVICTION_WINDOW: i64 = 100_000;
fn running_run(id: &str) -> Run {
    let mut run = completed_run(id);
    run.status = RunStatus::Running;
    run.outcome = None;
    run.terminal_sequence = None;
    run
}
/// A keyed admission receipt accepted at `sequence` (its run id carries
/// it); with a live run when `live`; in the eviction order when `in_order`,
/// else (live) waiting for its run in `replay_held`.
fn keyed_entry(s: &mut State, key: &str, sequence: u64, live: bool, in_order: bool) {
    let mut r = receipt(key, 0);
    r.run_id = format!("run-{sequence:016x}");
    s.keyed_admissions.insert(key.into(), r.clone());
    s.retention
        .admission_expires_at_ms
        .insert(key.into(), EVICTION_WINDOW);
    s.retention.expiry.insert(
        retention::window_key(EVICTION_WINDOW, &format!("k:{key}")),
        (),
    );
    if live {
        s.runs.insert(r.run_id.clone(), running_run(&r.run_id));
    }
    if in_order {
        s.retention
            .replay_order
            .insert(retention::admission_order_key(&r), ());
    } else if live {
        s.retention.replay_held.insert(r.run_id, key.into());
    }
}
fn event_entry(s: &mut State, key: &str, sequence: u64, in_order: bool) {
    let mut e = accepted_event(key, 0);
    e.sequence = sequence;
    if in_order {
        s.retention
            .replay_order
            .insert(retention::event_order_key(&e), ());
    }
    s.events.insert(key.into(), e);
    s.retention
        .event_expires_at_ms
        .insert(key.into(), EVICTION_WINDOW);
    s.retention.expiry.insert(
        retention::window_key(EVICTION_WINDOW, &format!("e:{key}")),
        (),
    );
}
/// Event `key` held by the live waiter `run-w`: a run waiting on a wait
/// record whose correlation key is the event's, with the derived indexes.
fn hold_by_waiter(s: &mut State, key: &str) {
    let e = s.events.get(key).unwrap().clone();
    let corr = indexes::correlation_key(
        &e.envelope.contract,
        &e.envelope.scope,
        &e.envelope.correlation,
    )
    .unwrap();
    let mut run = running_run("run-w");
    run.status = RunStatus::Waiting;
    run.pending = Some(Pending::Event {
        wait: "wait-w".into(),
    });
    s.runs.insert("run-w".into(), run);
    s.waits.insert(
        "wait-w".into(),
        WaitRecord {
            id: "wait-w".into(),
            run: "run-w".into(),
            step: "wait".into(),
            event: Some(e.envelope.contract.clone()),
            scope: e.envelope.scope.clone(),
            correlation: e.envelope.correlation.clone(),
            minimum_revision: None,
            deadline: EVICTION_WINDOW,
            bind: "completion".into(),
            winner: None,
            expired: false,
        },
    );
    s.indexes
        .event_memberships
        .insert(key.into(), vec![corr.clone()]);
    s.indexes
        .waiters
        .insert(corr, BTreeSet::from(["run-w".to_string()]));
}
/// "eu" at the head of the order, held by an operation of its contract
/// whose admission handle is lost (an uncertain completion), `replay_keep`
/// 4, then k0..k4.
fn uncertain_event_state() -> State {
    let mut s = held_event_state();
    s.events.remove("ew");
    s.retention.event_expires_at_ms.remove("ew");
    s.retention.replay_order.clear();
    s.runs.clear();
    s.waits.clear();
    s.indexes.waiters.clear();
    s.indexes.event_memberships.clear();
    s.retention.expiry.clear();
    for i in 0..5 {
        let r = s.keyed_admissions.get(&format!("k{i}")).unwrap().clone();
        s.retention
            .replay_order
            .insert(retention::admission_order_key(&r), ());
    }
    // Accepted after the operation's dispatch (sequence 0), so it may be
    // its completion; still the head ("e:eu" sorts before "k:k0").
    event_entry(&mut s, "eu", 1, true);
    let e = s.events.get("eu").unwrap().clone();
    let mut op = intent("op-u", "run-u", IntentStatus::Unknown);
    op.dispatch_sequence = Some(0);
    op.command.asynchronous = Some(AsyncContract {
        completion_event: e.envelope.contract.name.clone(),
        completion: Schema::Null,
        correlation_field: "invocation".into(),
        reconciliation_command: "test.command".into(),
    });
    s.intents.insert("op-u".into(), op);
    s.runs.insert("run-u".into(), running_run("run-u"));
    let contract = indexes::trigger_key(&e.envelope.contract, &e.envelope.scope).unwrap();
    s.indexes
        .uncertain_completion_contracts
        .insert(contract.clone(), BTreeSet::from(["op-u".to_string()]));
    s.indexes
        .uncertain_contract_membership
        .insert("op-u".into(), contract);
    s
}
/// "ew" (held by its waiter) at the head of the order, `replay_keep` 4,
/// then k0..k4.
fn held_event_state() -> State {
    let mut held_event = State::default();
    held_event.retention.clock_ms = Some(500);
    held_event.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.replay_keep = 4;
    event_entry(&mut held_event, "ew", 0, true);
    hold_by_waiter(&mut held_event, "ew");
    for i in 0..5 {
        keyed_entry(&mut held_event, &format!("k{i}"), i + 1, false, true);
    }
    held_event.sequence = 20;
    held_event
}
/// Evict keyed receipt `key` as `retention::evict` does: the item, its
/// expiry record, its window key and its order key.
fn evict_keyed(n: &mut State, key: &str) {
    let r = n.keyed_admissions.remove(key).unwrap();
    n.retention.admission_expires_at_ms.remove(key);
    n.retention
        .expiry
        .remove(&retention::window_key(EVICTION_WINDOW, &format!("k:{key}")));
    n.retention
        .replay_order
        .remove(&retention::admission_order_key(&r));
}
fn evict_event(n: &mut State, key: &str) {
    let e = n.events.remove(key).unwrap();
    n.retention.event_expires_at_ms.remove(key);
    n.retention
        .expiry
        .remove(&retention::window_key(EVICTION_WINDOW, &format!("e:{key}")));
    n.retention
        .replay_order
        .remove(&retention::event_order_key(&e));
    n.indexes.event_memberships.remove(key);
}
/// Take keyed receipt `key`, whose run is live, out of the order into
/// `replay_held`, as `retention::evict` does with a held head.
fn hold_out(n: &mut State, key: &str) {
    let r = n.keyed_admissions.get(key).unwrap().clone();
    n.retention
        .replay_order
        .remove(&retention::admission_order_key(&r));
    n.retention.replay_held.insert(r.run_id, key.into());
}
/// The eviction fence state: `replay_keep` 5; the order holds, oldest
/// first, "kh" (sequence 0, its run live: held, as an entry whose hold began
/// after it joined), "k0".."k3" (1..4), "e4" (5) and "ew" (7, held by a
/// live waiter): 7 entries. "kh2" (live run) waits in `replay_held`; "ex"
/// (8, unheld) is outside the order. The correct eviction takes "kh" out
/// of the order and evicts "k0", leaving 5.
fn eviction_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = 5;
    keyed_entry(&mut s, "kh", 0, true, true);
    for i in 0..4 {
        keyed_entry(&mut s, &format!("k{i}"), i + 1, false, true);
    }
    event_entry(&mut s, "e4", 5, true);
    event_entry(&mut s, "ew", 7, true);
    hold_by_waiter(&mut s, "ew");
    keyed_entry(&mut s, "kh2", 9, true, false);
    event_entry(&mut s, "ex", 8, false);
    s.sequence = 20;
    s
}
/// The correct eviction of `eviction_state()`.
fn evicted_correctly(n: &mut State) {
    hold_out(n, "kh");
    evict_keyed(n, "k0");
}
/// Each a (label, refusal, previous State, change): every way an eviction
/// can break the licence.
type EvictionCase = (&'static str, &'static str, State, Box<dyn Fn(&mut State)>);
fn eviction_refusal_cases() -> Vec<EvictionCase> {
    let licence = retention::REFUSE_EVICTION;
    let early = retention::REFUSE_EARLY_REMOVAL;
    let base = eviction_state();
    // "ew" (held by its waiter) at the head of the order.
    let mut held_event = State::default();
    held_event.retention.clock_ms = Some(500);
    held_event.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.replay_keep = 4;
    event_entry(&mut held_event, "ew", 0, true);
    hold_by_waiter(&mut held_event, "ew");
    for i in 0..5 {
        keyed_entry(&mut held_event, &format!("k{i}"), i + 1, false, true);
    }
    held_event.sequence = 20;
    vec![
        (
            "a held keyed receipt",
            licence,
            base.clone(),
            Box::new(|n| evict_keyed(n, "kh")),
        ),
        (
            "an event a live waiter holds",
            licence,
            held_event,
            Box::new(|n| evict_event(n, "ew")),
        ),
        (
            "fewer than replay_keep left",
            licence,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                evict_keyed(n, "k0");
                evict_keyed(n, "k1");
            }),
        ),
        (
            "at replay_keep after a policy change",
            licence,
            base.clone(),
            Box::new(|n| {
                n.retention.policy.replay_keep = 6;
                evicted_correctly(n);
            }),
        ),
        (
            "not the oldest",
            licence,
            base.clone(),
            Box::new(|n| evict_keyed(n, "k1")),
        ),
        (
            "older than a kept entry",
            licence,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                evict_keyed(n, "k1");
            }),
        ),
        (
            "an entry outside the order, not the oldest",
            early,
            base.clone(),
            Box::new(|n| evict_event(n, "ex")),
        ),
        (
            "its order key kept",
            early,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                n.keyed_admissions.remove("k0");
                n.retention.admission_expires_at_ms.remove("k0");
            }),
        ),
        (
            "an order entry without its item",
            licence,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(30, "k:ghost"), ());
            }),
        ),
        (
            "an order entry for a held item",
            licence,
            base.clone(),
            Box::new(|n| {
                let r = n.keyed_admissions.get("kh2").unwrap().clone();
                n.retention
                    .replay_order
                    .insert(retention::admission_order_key(&r), ());
            }),
        ),
        (
            "an order entry at another sequence",
            licence,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(30, "k:k3"), ());
            }),
        ),
        (
            "an order entry that inflates the count",
            licence,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(8, "e:ex"), ());
                n.retention
                    .replay_order
                    .insert(retention::replay_key(31, "k:ghost"), ());
                hold_out(n, "kh");
                evict_keyed(n, "k0");
                evict_keyed(n, "k1");
            }),
        ),
        // Review C1 (round 1): an accepted event's identity never changes,
        // so it cannot be moved to the head of the order and evicted.
        (
            "an event re-sequenced to the head",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| {
                let mut e = n.events.get("e4").unwrap().clone();
                n.retention
                    .replay_order
                    .remove(&retention::event_order_key(&e));
                e.sequence = 0;
                n.retention
                    .replay_order
                    .insert(retention::event_order_key(&e), ());
                n.events.insert("e4".into(), e);
            }),
        ),
        (
            "an event's digest changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().digest = "forged".into()),
        ),
        (
            "an event's acceptance time changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().accepted_at_ms = 7),
        ),
        (
            "an event's envelope changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().envelope.payload = json!(1)),
        ),
        // Review C2 (round 1): the hold is judged from the waiter's real wait
        // record, so dropping the derived index does not release it.
        (
            "an event an uncertain completion holds, its index dropped",
            licence,
            uncertain_event_state(),
            Box::new(|n| {
                n.indexes.uncertain_completion_contracts.clear();
                n.indexes.uncertain_contract_membership.clear();
                evict_event(n, "eu");
            }),
        ),
        (
            "a held event whose waiter index entry is dropped",
            licence,
            held_event_state(),
            Box::new(|n| {
                n.indexes.waiters.clear();
                evict_event(n, "ew");
            }),
        ),
        (
            "more than 64 in one commit",
            licence,
            bulk_eviction_state(),
            Box::new(|n| {
                for i in 0..65 {
                    evict_keyed(n, &format!("b{i:03}"));
                }
            }),
        ),
    ]
}
/// Store `state` as a namespace reaches it: its entries join the order
/// unheld (the live runs and waiters absent), then the holds begin in a
/// second commit (a held entry can never be written into the order).
/// Returns the revision.
fn seed_eviction_state(store: &dyn Storage, ns: &str, state: &State) -> u64 {
    store.acquire(ns, "w").unwrap();
    let mut unheld = state.clone();
    unheld.runs.clear();
    unheld.waits.clear();
    unheld.intents.clear();
    unheld.indexes.waiters.clear();
    unheld.indexes.uncertain_completion_contracts.clear();
    unheld.indexes.uncertain_contract_membership.clear();
    let first = store.compare_and_swap(ns, "w", 0, &unheld).unwrap();
    store.compare_and_swap(ns, "w", first, state).unwrap()
}
/// `replay_keep` 1 and 70 unheld keyed receipts in the order.
fn bulk_eviction_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = 1;
    for i in 0..70 {
        keyed_entry(&mut s, &format!("b{i:03}"), i + 1, false, true);
    }
    s.sequence = 100;
    s
}
/// The eviction licence on a store's compare-and-swap: every refusal
/// case is refused with its text and writes nothing; the correct
/// eviction, and 64 at once, are accepted.
fn eviction_fences_hold_on_cas(store: &dyn Storage) {
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("evict-{}", count.get());
        let base = seed_eviction_state(store, &ns, state);
        (ns, base)
    };
    let mut failures = vec![];
    for (label, text, previous, f) in eviction_refusal_cases() {
        let (ns, base) = fresh(&previous);
        let mut next = previous.clone();
        f(&mut next);
        let result = store.compare_and_swap(&ns, "w", base, &next);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
        }
    }
    for (label, previous, f) in [
        (
            "the correct eviction",
            eviction_state(),
            Box::new(evicted_correctly) as Box<dyn Fn(&mut State)>,
        ),
        (
            "64 at once",
            bulk_eviction_state(),
            Box::new(|n: &mut State| {
                for i in 0..64 {
                    evict_keyed(n, &format!("b{i:03}"));
                }
            }),
        ),
    ] {
        let (ns, base) = fresh(&previous);
        let mut next = previous.clone();
        f(&mut next);
        if let Err(err) = store.compare_and_swap(&ns, "w", base, &next) {
            failures.push(format!("{label} refused: {err}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
#[test]
fn memory_storage_holds_the_eviction_licence() {
    eviction_fences_hold_on_cas(&MemoryStorage::default());
}
#[test]
fn record_storage_holds_the_eviction_licence() {
    eviction_fences_hold_on_cas(&RecordStorage::new());
}
/// The same licence on `RecordStorage`'s bounded transitions (the patch
/// fence).
#[test]
fn record_storage_transitions_hold_the_eviction_licence() {
    let store = RecordStorage::new();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("t-evict-{}", count.get());
        let mut state = state.clone();
        state.indexes.initialized = true;
        let base = seed_eviction_state(&store, &ns, &state);
        (ns, base)
    };
    let transition_in = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        store
            .transition_v1(
                ns,
                "w",
                &mut TransitionRequest {
                    operation: "evict",
                    plan: &mut plan,
                },
            )
            .map_err(|e| match e {
                Error::Storage(e) => e,
                other => StorageError::Backend(other.to_string()),
            })
    };
    let mut failures = vec![];
    for (label, text, previous, f) in eviction_refusal_cases() {
        let (ns, base) = fresh(&previous);
        let result = transition_in(&ns, &*f);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
        }
    }
    let full = store
        .stats
        .full_commits
        .load(std::sync::atomic::Ordering::Relaxed);
    for (label, previous, f) in [
        (
            "the correct eviction",
            eviction_state(),
            Box::new(evicted_correctly) as Box<dyn Fn(&mut State)>,
        ),
        (
            "64 at once",
            bulk_eviction_state(),
            Box::new(|n: &mut State| {
                for i in 0..64 {
                    evict_keyed(n, &format!("b{i:03}"));
                }
            }),
        ),
    ] {
        let (ns, _) = fresh(&previous);
        if let Err(err) = transition_in(&ns, &*f) {
            failures.push(format!("{label} refused: {err}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // Four seeding full commits; the evictions themselves are bounded.
    assert_eq!(
        store
            .stats
            .full_commits
            .load(std::sync::atomic::Ordering::Relaxed),
        full + 4
    );
}

// --- Task 8b R1: repeat protection is a rolling set (engine) -------------

fn keep_engine(keep: usize) -> Engine<MemoryStorage> {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        replay_keep: keep,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e
}
/// Admit keyed `key` at `t` and finish its run.
fn keyed_done(e: &Engine<MemoryStorage>, key: &str, t: i64) -> KeyedAdmissionDisposition {
    let d = e.admit_once(keyed(e, key), t).unwrap();
    if !d.duplicate {
        assert!(e.advance(&d.receipt.run_id, t).unwrap());
    }
    d
}
fn order_members(s: &State) -> Vec<String> {
    s.retention
        .replay_order
        .keys()
        .map(|k| retention::replay_entry(k).unwrap().1.to_string())
        .collect()
}
#[test]
fn replay_keep_must_be_positive() {
    let e = engine();
    assert!(matches!(
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 0,
            ..RetentionPolicy::default()
        }),
        Err(Error::Refused(m)) if m == "replay_keep must be positive"
    ));
    assert_eq!(RetentionPolicy::default().replay_keep, 256);
    let stored: RetentionPolicy =
        serde_json::from_value(json!({"event_replay_ms": 1, "audit_ms": 1})).unwrap();
    assert_eq!((stored.replay_keep, stored.history_keep), (256, 10_000));
    // A State written before the rolling bounds decodes with an empty order.
    let legacy: State = serde_json::from_value(json!({
        "artifacts": {}, "bindings": {}, "catalog_revision": 0, "runs": {},
        "receipts": [], "intents": {}, "waits": {}, "events": {}, "budgets": {},
        "sequence": 0,
    }))
    .unwrap();
    assert!(legacy.retention.replay_order.is_empty() && legacy.retention.replay_held.is_empty());
}
/// With `replay_keep` 4: a duplicate inside the rolling set is recognised;
/// the 5th distinct unheld key evicts the 1st (oldest first, its window not
/// passed); a re-send of the 1st is then accepted as new, and with a
/// request time older than its window refused as stale. No floor: a
/// re-send whose request time is inside its window is new.
#[test]
fn the_fifth_unheld_key_evicts_the_first() {
    let e = keep_engine(4);
    for (i, key) in ["k0", "k1", "k2", "k3"].iter().enumerate() {
        keyed_done(&e, key, i as i64);
    }
    assert!(keyed_done(&e, "k0", 10).duplicate);
    assert_eq!(
        order_members(&e.snapshot().unwrap()),
        ["k:k0", "k:k1", "k:k2", "k:k3"]
    );
    keyed_done(&e, "k4", 11);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:k1", "k:k2", "k:k3", "k:k4"]);
    assert!(!s.keyed_admissions.contains_key("k0"));
    assert!(!s.retention.admission_expires_at_ms.contains_key("k0"));
    assert!(window_entries(&s).iter().all(|(_, m)| m != "k:k0"));
    assert!(keyed_done(&e, "k1", 12).duplicate);
    // The first is forgotten: with a request time older than its window it
    // is stale, writing nothing ...
    let before = e.store.load("test").unwrap().revision;
    assert!(stale(e.admit_once_at(
        keyed(&e, "k0"),
        Some(12 - 600_000),
        12
    )));
    assert_eq!(e.store.load("test").unwrap().revision, before);
    // ... and with one inside its window (or none) it is new.
    let again = e.admit_once_at(keyed(&e, "k0"), Some(0), 13).unwrap();
    assert!(!again.duplicate);
}
/// Entries live work still needs are neither evicted nor counted: keyed
/// receipts of live runs, an event a live waiter holds. A keyed receipt
/// joins the order when its run finishes.
#[test]
fn held_entries_are_not_evicted_and_do_not_count() {
    let e = keep_engine(2);
    let live: Vec<String> = (0..3)
        .map(|i| {
            e.admit_once(keyed(&e, &format!("live-{i}")), i)
                .unwrap()
                .receipt
                .run_id
        })
        .collect();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    // Matches the waiter: held while the waiter is live (it has not
    // advanced past its wait yet).
    let held = e.admit_event("host", event("held", 2), 4).unwrap();
    keyed_done(&e, "done-0", 5);
    keyed_done(&e, "done-1", 6);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-0", "k:done-1"]);
    assert_eq!(s.retention.replay_held.len(), 3);
    assert_eq!(s.keyed_admissions.len(), 5);
    assert!(s.events.contains_key(&held.key));
    // A third finished key evicts the oldest unheld one, not a held one.
    keyed_done(&e, "done-2", 7);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(s.events.contains_key(&held.key));
    assert!(live.iter().all(|run| s.runs.contains_key(run)));
    // A live run finishes: its receipt joins the order (by acceptance, so
    // it is the oldest) and is evicted first.
    assert!(e.advance(&live[0], 8).unwrap());
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.keyed_admissions.contains_key("live-0"));
    // The waiter consumes the event and finishes: the event joins the
    // order by its acceptance (older than "done-1") and, its window not
    // passed, is evicted as the oldest.
    while e.snapshot().unwrap().runs.contains_key(&waiter) {
        e.advance(&waiter, 9).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.events.contains_key(&held.key));
    assert!(s.retention.event_expires_at_ms.get(&held.key).is_none());
}
/// A burst of 10,000 keyed admissions with the clock frozen keeps the
/// unheld set at `replay_keep` (within the per-commit bound) and refuses
/// none of them for what is remembered.
#[test]
fn a_frozen_clock_burst_keeps_the_remembered_set_at_replay_keep() {
    let e = ret_engine();
    let keep = RetentionPolicy::default().replay_keep;
    for i in 0..10_000 {
        let d = keyed_done(&e, &format!("burst-{i:05}"), 0);
        assert!(!d.duplicate, "{i}");
        if i % 500 == 0 || i == 9_999 {
            let s = e.snapshot().unwrap();
            let order = s.retention.replay_order.len();
            assert!(
                order <= keep + retention::REPLAY_EVICTIONS_PER_COMMIT,
                "{i}: {order}"
            );
            assert_eq!(s.keyed_admissions.len(), order.min(i + 1), "{i}");
            if i >= keep {
                assert!(order >= keep, "{i}: {order}");
            }
        }
    }
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.replay_order.len(), keep);
    assert!(s.runs.is_empty());
}
/// An event whose contract declares a 24 h window is evicted by count
/// long before its window passes; a wait registered after its eviction
/// times out, as after an expired event.
#[test]
fn a_day_long_event_window_is_evicted_by_count() {
    let e = ret_engine();
    let mut day = wait_artifact().content;
    day.registry
        .events
        .get_mut("test.completed")
        .unwrap()
        .retention_ms = 86_400_000;
    // The waiter's contract reference must match the event's.
    let contract = day.registry.events["test.completed"].clone();
    if let Operation::Wait { event, .. } = &mut day.playbook.steps.get_mut("wait").unwrap().op {
        *event = Some(ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        });
    }
    let day = Artifact::seal(day).unwrap();
    install(&e, day, "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        replay_keep: 2,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let envelope = |id: &str, invocation: &str| EventEnvelope {
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        ..correlated_event(id, invocation)
    };
    let first = e
        .admit_event("host", envelope("day-0", "exact-invocation"), 0)
        .unwrap();
    assert_eq!(
        e.snapshot().unwrap().retention.event_expires_at_ms[&first.key],
        86_400_000
    );
    e.admit_event("host", envelope("day-1", "inv-1"), 1)
        .unwrap();
    e.admit_event("host", envelope("day-2", "inv-2"), 2)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert_eq!(s.events.len(), 2);
    assert!(s.indexes.event_memberships.get(&first.key).is_none());
    // A wait for it registered after its eviction does not match; it times
    // out.
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    e.drive(&mut FakeHost::default(), 200, 20).unwrap();
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "error");
    // A re-send of the evicted event is new.
    assert!(
        !e.admit_event("host", envelope("day-0", "exact-invocation"), 201)
            .unwrap()
            .duplicate
    );
}
/// Format 3 (bound record containers): the work of a commit that evicts k
/// entries depends on k, not on the entries kept, and reads no container
/// whole. The measured commit lowers `replay_keep` by k.
#[test]
fn eviction_work_on_record_storage_depends_on_entries_evicted_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, k: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        let policy = |keep: usize| RetentionPolicy {
            replay_keep: keep,
            ..RetentionPolicy::default()
        };
        e.set_retention_policy(policy(kept + k)).unwrap();
        for i in 0..kept + k {
            let d = e
                .admit_once(keyed_on(&e, &format!("k-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.set_retention_policy(policy(kept)).unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.replay_order.len(), kept);
        assert_eq!(s.keyed_admissions.len(), kept);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    let (none_few, scanned) = measure(20, 0);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (none_many, _) = measure(80, 0);
    assert_eq!(none_few, none_many, "nothing evicted");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 evicted");
    let (sixteen, _) = measure(20, 16);
    let (twenty_four, _) = measure(20, 24);
    // A fixed part (the head lookup) and the same per eviction.
    let per_eight = sixteen - eight;
    assert_eq!(
        twenty_four - sixteen,
        per_eight,
        "reads grow linearly with evictions"
    );
    assert!(per_eight > 0);
    let fixed = (eight - none_few) - per_eight;
    assert!(fixed <= 4, "fixed part {fixed}");
}
/// Held entries at the head of the order (their holds began after they
/// joined, as through an untracked uncertain completion): the engine takes
/// them out of the order rather than evicting them, then evicts the oldest
/// unheld one; the stored commit passes the licence.
#[test]
fn the_engine_takes_held_entries_at_the_head_out_of_the_order() {
    // States this engine wrote (drained): the drain would otherwise judge
    // the entries first (Task 9).
    let drained = retention::DrainState {
        done: true,
        roots_done: true,
        phase: retention::DRAIN_DONE,
        ..retention::DrainState::default()
    };
    let mut keyed_head = eviction_state();
    keyed_head.indexes.initialized = true;
    keyed_head.retention.drain = drained.clone();
    let mut event_head = State::default();
    event_head.retention.clock_ms = Some(500);
    event_head.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    event_head.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    event_head.retention.policy.replay_keep = 4;
    event_entry(&mut event_head, "ew", 0, true);
    hold_by_waiter(&mut event_head, "ew");
    for i in 0..5 {
        keyed_entry(&mut event_head, &format!("k{i}"), i + 1, false, true);
    }
    event_head.sequence = 20;
    event_head.indexes.initialized = true;
    event_head.retention.drain = drained;
    for (label, state, kept) in [
        (
            "a keyed receipt whose run is live",
            keyed_head,
            vec!["k:k1", "k:k2", "k:k3", "e:e4", "e:ew"],
        ),
        (
            "an event a waiter holds",
            event_head,
            vec!["k:k1", "k:k2", "k:k3", "k:k4"],
        ),
    ] {
        let store = MemoryStorage::default();
        seed_eviction_state(&store, "test", &state);
        store.release("test", "w").unwrap();
        let e = Engine::new(store, "test", "incarnation-1").unwrap();
        e.set_retention_policy(state.retention.policy.clone())
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        let s = e.snapshot().unwrap();
        assert_eq!(order_members(&s), kept, "{label}");
        assert!(!s.keyed_admissions.contains_key("k0"), "{label}");
        assert!(s.events.contains_key("ew"), "{label}");
        assert_eq!(
            s.keyed_admissions.contains_key("kh"),
            state.keyed_admissions.contains_key("kh"),
            "{label}"
        );
    }
}
/// An entry that leaves by its window, or is forgotten to be admitted
/// again, leaves the eviction order with it.
#[test]
fn dropped_and_forgotten_entries_leave_the_order() {
    let e = keep_engine(10);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        replay_keep: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..3 {
        keyed_done(&e, &format!("old-{i}"), i);
    }
    e.admit_event("host", correlated_event("old-event", "nobody"), 0)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.replay_order.len(), 4);
    // Past the windows: the drop takes the receipts and the event.
    keyed_done(&e, "new", 2_000);
    assert_eq!(order_members(&e.snapshot().unwrap()), ["k:new"]);
    // An expired key admitted again is forgotten first, then new.
    keyed_done(&e, "new", 4_000);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:new"]);
    assert_eq!(s.retention.replay_order.len(), 1);
}

// --- Task 8b R2: history is a rolling log of history_keep runs -----------

fn history_keep_engine<S: Storage>(store: S, keep: u64) -> Engine<S> {
    let e = Engine::new(store, "test", "incarnation-1").unwrap();
    install_on(&e, done_artifact(), "a");
    e.set_retention_policy(RetentionPolicy {
        history_keep: keep,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e
}
fn history_items<S: Storage>(e: &Engine<S>, kind: InspectionKind) -> Vec<Value> {
    e.inspect(&InspectionQuery {
        kind,
        run: None,
        after: None,
        limit: 1000,
    })
    .unwrap()
    .items
}
/// With `history_keep` 5, after 20 finished runs inspection lists the
/// newest 5, and history holds exactly those runs' items (and the receipts
/// of no run: admin).
fn history_keeps_the_newest_runs<S: Storage>(store: S) {
    let e = history_keep_engine(store, 5);
    let runs: Vec<String> = (0..20)
        .map(|t| {
            let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
            assert!(e.advance(&id, t).unwrap());
            id
        })
        .collect();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, runs[15..]);
    for item in history_items(&e, InspectionKind::Receipts) {
        let run = item["run"].as_str().unwrap();
        assert!(
            run == "admin" || runs[15..].iter().any(|r| r == run),
            "{item}"
        );
    }
    assert!(history_items(&e, InspectionKind::Receipts).len() >= 10);
}
#[test]
fn memory_storage_history_keeps_the_newest_runs() {
    history_keeps_the_newest_runs(MemoryStorage::default());
}
#[test]
fn record_storage_history_keeps_the_newest_runs() {
    history_keeps_the_newest_runs(RecordStorage::new());
}
/// One commit settling 100 cohorts (more than the trim's spare 64): the
/// trim keeps up, and history holds the newest `history_keep`.
fn trim_keeps_up_with_one_commit(store: &impl Storage) {
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.retention.policy.history_keep = 5;
    state.settled = (0..100)
        .map(|i| synthetic_cohort(&format!("c{i:03}"), i, &[i as u64 + 1]))
        .collect();
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let ids: Vec<Value> = page(store, InspectionKind::Runs, None, 1000)
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(ids, ["c095", "c096", "c097", "c098", "c099"]);
    assert_eq!(
        page(store, InspectionKind::Receipts, None, 1000)
            .items
            .len(),
        5
    );
}
#[test]
fn memory_storage_trim_keeps_up_with_one_commit() {
    trim_keeps_up_with_one_commit(&MemoryStorage::default());
}
#[test]
fn record_storage_trim_keeps_up_with_one_commit() {
    trim_keeps_up_with_one_commit(&RecordStorage::new());
}
/// `history_keep` 0: no history is written; finished runs are not found.
#[test]
fn history_keep_zero_writes_no_history() {
    let e = history_keep_engine(MemoryStorage::default(), 0);
    let id = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    assert!(e.advance(&id, 0).unwrap());
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert!(history_items(&e, InspectionKind::Runs).is_empty());
    assert!(history_items(&e, InspectionKind::Receipts).is_empty());
    assert_eq!(RetentionPolicy::default().history_keep, 10_000);
}
/// `delete_history` (by time) still works beside the trim, and the trim
/// counts what it leaves: 10 runs keep the newest 5; deleting those
/// finished before 7 leaves 3; 4 more keep the newest 5 again.
fn delete_history_and_the_trim<S: Storage>(store: S) {
    let e = history_keep_engine(store, 5);
    let admit = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let runs: Vec<String> = (0..10).map(admit).collect();
    let listed = |e: &Engine<S>| -> Vec<String> {
        history_items(e, InspectionKind::Runs)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(listed(&e), runs[5..]);
    assert!(e.delete_history_before(7).unwrap() > 0);
    assert_eq!(listed(&e), runs[7..]);
    let more: Vec<String> = (10..14).map(admit).collect();
    let mut expected = runs[9..].to_vec();
    expected.extend(more);
    assert_eq!(listed(&e), expected);
}
#[test]
fn memory_storage_delete_history_and_the_trim() {
    delete_history_and_the_trim(MemoryStorage::default());
}
#[test]
fn record_storage_delete_history_and_the_trim() {
    delete_history_and_the_trim(RecordStorage::new());
}
/// A page that straddles a trim neither repeats nor panics: its cursor's
/// run is trimmed between pages; the next page starts after the cursor.
#[test]
fn a_page_across_a_trim_neither_repeats_nor_panics() {
    let e = history_keep_engine(MemoryStorage::default(), 3);
    let admit = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let first: Vec<String> = (0..3).map(admit).collect();
    let query = |after: Option<String>| InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after,
        limit: 2,
    };
    let page_one = e.inspect(&query(None)).unwrap();
    assert_eq!(page_one.items.len(), 2);
    let cursor = page_one.next.clone().unwrap();
    let later: Vec<String> = (3..5).map(admit).collect();
    let page_two = e.inspect(&query(Some(cursor.clone()))).unwrap();
    let ids: Vec<String> = page_two
        .items
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [first[2].clone(), later[0].clone()]);
    assert!(ids.iter().all(|id| *id > cursor));
}

// --- Task 8b fix round 1 --------------------------------------------------

/// Review I1 and re-review round 1: an operation whose admission handle is
/// lost holds only the events accepted after its dispatch (its completion
/// cannot come earlier). Events accepted before it are never held and stay
/// in the eviction order; events accepted while the handle is lost are held
/// (never evicted, not counted) and rejoin at their acceptance positions
/// once the handle is found.
#[test]
fn uncertain_completion_holds_are_tracked_both_ways() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let of_contract = |id: &str, op: &str| EventEnvelope {
        correlation: BTreeMap::from([("operation".into(), op.into())]),
        ..async_event(&completion, id)
    };
    let accept = |envelope: EventEnvelope, t: i64| e.admit_event("host", envelope, t).unwrap().key;
    let before: Vec<String> = (0..2)
        .map(|i| accept(of_contract(&format!("before-{i}"), &format!("op-{i}")), i))
        .collect();
    // The dispatch loses the admission handle.
    e.admit("async", "main", Value::Null, authority(), 2)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 2, 10).unwrap();
    let members =
        |keys: &[&String]| -> Vec<String> { keys.iter().map(|k| format!("e:{k}")).collect() };
    assert_eq!(
        order_members(&e.snapshot().unwrap()),
        members(&before.iter().collect::<Vec<_>>()),
        "events accepted before the dispatch are not held"
    );
    let during: Vec<String> = (0..2)
        .map(|i| {
            accept(
                of_contract(&format!("during-{i}"), &format!("op-d{i}")),
                3 + i,
            )
        })
        .collect();
    let others: Vec<String> = (0..4)
        .map(|i| {
            accept(
                correlated_event(&format!("other-{i}"), &format!("inv-{i}")),
                5 + i,
            )
        })
        .collect();
    let s = e.snapshot().unwrap();
    // Held: never evicted, not counted. The unheld oldest are evicted.
    assert!(during.iter().all(|k| s.events.contains_key(k)));
    assert!(before.iter().all(|k| !s.events.contains_key(k)));
    assert_eq!(
        order_members(&s),
        members(&others[1..].iter().collect::<Vec<_>>())
    );
    // The handle is found: the held events rejoin as older than the others
    // and are evicted first.
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        during.iter().all(|k| !s.events.contains_key(k)),
        "rejoined and evicted"
    );
    assert_eq!(
        order_members(&s),
        members(&others[1..].iter().collect::<Vec<_>>())
    );
}
/// Re-review round 1: an asynchronous dispatch, and the handle that binds
/// right after it, re-judge no event accepted before the dispatch. On bound
/// record containers their work does not depend on how many such events of
/// the completion contract the record holds.
#[test]
fn an_async_dispatch_rejudges_no_earlier_event() {
    use std::sync::atomic::Ordering;
    let measure = |earlier: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        let (a, completion) = async_artifact();
        install_on(&e, a, "async");
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        for i in 0..earlier {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("e{i}"))
            };
            e.admit_event("host", envelope, i as i64).unwrap();
        }
        let run = e
            .admit("async", "main", Value::Null, authority(), 100)
            .unwrap();
        e.advance(&run, 100).unwrap();
        let order = e.snapshot().unwrap().retention.replay_order.len();
        assert_eq!(order, earlier);
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        let mut host = FakeHost {
            responses: BTreeMap::from([(
                "test.command".into(),
                vec![DispatchResult::Accepted {
                    handle: "server-operation".into(),
                }],
            )]),
            ..Default::default()
        };
        // The dispatch commit, the host call, and the commit binding the
        // handle.
        assert!(e.dispatch_next(&mut host, 101).unwrap().is_some());
        let s = e.snapshot().unwrap();
        assert!(s.intents.values().any(|i| i.handle.is_some()));
        assert_eq!(s.retention.replay_order.len(), earlier);
        assert!(e.store.stats.fallbacks()[fallbacks..].is_empty());
        e.store.stats.record_reads.load(Ordering::Relaxed) - reads
    };
    assert_eq!(measure(10), measure(60));
}
/// Review I2: the trim keeps the newest runs by the namespace's sequence,
/// not by host finish time: after a host clock that ran ahead, the newest
/// three finished runs survive.
fn the_trim_orders_by_sequence_not_finish_time<S: Storage>(store: S) {
    let e = history_keep_engine(store, 3);
    let run = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let pinned: Vec<String> = (0..3).map(|_| run(9_000_000_000_000)).collect();
    let later: Vec<String> = (0..2).map(|_| run(1_000)).collect();
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        listed,
        [pinned[2].clone(), later[0].clone(), later[1].clone()]
    );
}
#[test]
fn memory_storage_trims_by_sequence_not_finish_time() {
    the_trim_orders_by_sequence_not_finish_time(MemoryStorage::default());
}
#[test]
fn record_storage_trims_by_sequence_not_finish_time() {
    the_trim_orders_by_sequence_not_finish_time(RecordStorage::new());
}
/// Review I3: nothing in history is exempt from the rolling bound. 1,000
/// events whose destination is blocked (each writes a `destination_blocked`
/// receipt, which goes to history when the event leaves) and 50 operator
/// actions (admin receipts) leave at most `history_keep` of each.
fn receipts_of_no_run_are_rolling_too<S: Storage>(store: S) {
    let e = Engine::new(store, "test", "incarnation-1").unwrap();
    install_on(&e, input_blocked_trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 5,
        history_keep: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..1_000 {
        e.admit_event("host", correlated_event(&format!("b{i:04}"), "x"), i)
            .unwrap();
    }
    for i in 0..50 {
        e.set_activation_controls("trigger", ActivationControls::default(), 2_000 + i)
            .unwrap();
    }
    let receipts = history_items(&e, InspectionKind::Receipts);
    let of = |prefix: &str| {
        receipts
            .iter()
            .filter(|r| r["run"].as_str().unwrap().starts_with(prefix))
            .count()
    };
    // At most 10 in history, plus those of the 5 events still remembered
    // (live receipts, which leave with their event).
    let live: usize = e.snapshot().unwrap().receipts.len();
    assert_eq!(live, 5);
    assert!(
        of("event:") > live && of("event:") <= 10 + live,
        "{}",
        of("event:")
    );
    assert!(of("admin") > 0 && of("admin") <= 10, "{}", of("admin"));
    assert!(receipts.len() <= 20 + live, "{}", receipts.len());
    // The newest stay.
    let newest = receipts
        .iter()
        .filter(|r| r["run"] == "admin")
        .map(|r| r["sequence"].as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(newest, e.snapshot().unwrap().sequence);
}
#[test]
fn memory_storage_receipts_of_no_run_are_rolling_too() {
    receipts_of_no_run_are_rolling_too(MemoryStorage::default());
}
#[test]
fn record_storage_receipts_of_no_run_are_rolling_too() {
    receipts_of_no_run_are_rolling_too(RecordStorage::new());
}
/// An event of the async completion contract (copied from `semantics.rs`).
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
/// Review I2: the trim keeps the run that finished last, not the one
/// admitted last.
fn the_trim_keeps_the_last_finished_run<S: Storage>(store: S) {
    let e = history_keep_engine(store, 1);
    let first = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    let second = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    assert!(e.advance(&second, 1).unwrap());
    assert!(e.advance(&first, 2).unwrap());
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, [first]);
}
#[test]
fn memory_storage_trim_keeps_the_last_finished_run() {
    the_trim_keeps_the_last_finished_run(MemoryStorage::default());
}
#[test]
fn record_storage_trim_keeps_the_last_finished_run() {
    the_trim_keeps_the_last_finished_run(RecordStorage::new());
}

// --- Task 9, first commit: dispatch sequences --------------------------------

fn only_intent(s: &State) -> Intent {
    assert_eq!(s.intents.len(), 1);
    s.intents.values().next().unwrap().clone()
}
/// Task 8b re-review round 2 (Important): a recovery re-dispatch keeps the
/// operation's first dispatch sequence. A completion accepted while the
/// handle was unknown stays held through the recovery (its operation was
/// issued once and may still answer), is not evicted by newer events, and
/// is delivered once the re-dispatch binds the same idempotent handle.
#[test]
fn a_recovery_redispatch_keeps_the_hold_on_a_completion_accepted_while_the_handle_was_unknown() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact_with(Recovery::KeyedIdempotent);
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    // The dispatch loses the admission handle.
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let first = only_intent(&e.snapshot().unwrap());
    assert_eq!(
        (first.status.clone(), first.handle.clone()),
        (IntentStatus::Unknown, None)
    );
    let dispatched = first.dispatch_sequence.unwrap();
    // Its completion arrives while the handle is unknown: held.
    let done = e
        .admit_event("host", async_event(&completion, "done"), 1)
        .unwrap()
        .key;
    let held = |s: &State| {
        s.events.contains_key(&done) && !order_members(s).contains(&format!("e:{done}"))
    };
    assert!(held(&e.snapshot().unwrap()));
    // Recovery prepares the idempotent operation again.
    e.recover(&mut host, 2).unwrap();
    assert_eq!(
        only_intent(&e.snapshot().unwrap()).status,
        IntentStatus::Prepared
    );
    // More than `replay_keep` newer unheld events.
    for i in 0..5 {
        e.admit_event(
            "host",
            correlated_event(&format!("other-{i}"), &format!("inv-{i}")),
            3 + i,
        )
        .unwrap();
    }
    assert!(held(&e.snapshot().unwrap()), "held through the recovery");
    // The re-dispatch binds the same idempotent handle.
    host.responses.insert(
        "test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.dispatch_next(&mut host, 10).unwrap();
    let again = only_intent(&e.snapshot().unwrap());
    assert_eq!(again.handle.as_deref(), Some("server-operation"));
    assert_eq!(
        again.dispatch_sequence,
        Some(dispatched),
        "the first dispatch is kept"
    );
    for t in 10..20 {
        e.drive(&mut host, t, 20).unwrap();
    }
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
}
/// Task 8b re-review round 2 (Minor): an asynchronous operation stopped
/// ("authority revoked") before it was dispatched was never issued, so
/// nothing can complete it: it is not uncertain and holds no event. Events
/// of its contract stay in the eviction order and are evicted by count.
#[test]
fn an_operation_never_dispatched_holds_no_event() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        revoked: set(&["execute"]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    let op = only_intent(&s);
    assert_eq!(
        (op.status, op.handle, op.dispatch_sequence),
        (IntentStatus::Unknown, None, None)
    );
    assert_eq!(s.runs[&run].status, RunStatus::Blocked);
    assert!(host.dispatched.is_empty());
    assert!(
        s.indexes
            .uncertain_completion_contracts
            .values()
            .all(BTreeSet::is_empty)
    );
    let keys: Vec<String> = (0..5)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("c{i}"))
            };
            e.admit_event("host", envelope, 1 + i).unwrap().key
        })
        .collect();
    let s = e.snapshot().unwrap();
    assert_eq!(
        order_members(&s),
        keys[2..]
            .iter()
            .map(|k| format!("e:{k}"))
            .collect::<Vec<_>>()
    );
    assert!(!s.events.contains_key(&keys[0]) && !s.events.contains_key(&keys[1]));
}
/// Carried hazard (Task 8b re-review round 2): an asynchronous operation
/// dispatched by an engine from before Task 8b has no dispatch sequence.
/// Opening the namespace fills it once from the operation's own first
/// `dispatch` receipt, so it holds exactly the events accepted after its
/// dispatch: a completion accepted while its handle is lost stays held,
/// and the commit that binds the handle judges no earlier event (record
/// reads on bound containers do not depend on how many there are).
#[test]
fn a_legacy_dispatched_operation_gets_its_dispatch_sequence_on_open() {
    use std::sync::atomic::Ordering;
    let measure = |earlier: usize| {
        let store = RecordStorage::new();
        let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
        let (a, completion) = async_artifact();
        install_on(&e, a, "async");
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        for i in 0..earlier {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("e{i}"))
            };
            e.admit_event("host", envelope, i as i64).unwrap();
        }
        let run = e
            .admit("async", "main", Value::Null, authority(), 100)
            .unwrap();
        let mut host = FakeHost::default();
        e.drive(&mut host, 100, 10).unwrap();
        // As the old engine stored it: no dispatch sequence.
        let mut legacy = e.snapshot().unwrap();
        let id = only_intent(&legacy).id;
        let dispatched = legacy.intents[&id].dispatch_sequence.take().unwrap();
        legacy.indexes.dispatch_sequences_complete = false;
        // The old engine wrote a `dispatch` receipt for every dispatch; a
        // later one (a recovery re-dispatch) does not move the first.
        legacy.sequence += 1;
        let again = Receipt {
            sequence: legacy.sequence,
            run: run.clone(),
            step: "invoke".into(),
            at_ms: 100,
            kind: "dispatch".into(),
            detail: json!({"operation": id}),
        };
        legacy.receipts.push(again);
        let offset = legacy.receipts.len() - 1;
        legacy
            .indexes
            .receipts
            .entry(run.clone())
            .or_default()
            .insert(legacy.sequence, offset);
        legacy.indexes.all_receipts.insert(legacy.sequence, offset);
        overwrite(&e.store, "incarnation-1", &legacy);
        e.release().unwrap();
        let e = Engine::new(store, "test", "incarnation-2").unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.intents[&id].dispatch_sequence, Some(dispatched));
        assert!(s.indexes.dispatch_sequences_complete);
        assert_eq!(
            s.retention.replay_order.len(),
            earlier,
            "earlier events are not held"
        );
        let done = e
            .admit_event("host", async_event(&completion, "done"), 101)
            .unwrap()
            .key;
        let s = e.snapshot().unwrap();
        assert!(!order_members(&s).contains(&format!("e:{done}")), "held");
        host.responses.insert(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        );
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.recover(&mut host, 102).unwrap();
        assert!(e.store.stats.fallbacks()[fallbacks..].is_empty());
        let work = e.store.stats.record_reads.load(Ordering::Relaxed) - reads;
        for t in 102..110 {
            e.drive(&mut host, t, 20).unwrap();
        }
        let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
        assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
        work
    };
    assert_eq!(measure(10), measure(60));
}

// --- Task 9: the drain of namespaces written by an older engine -------------

/// Inject into `s` `cohorts` settled one-run cohorts as an older engine left
/// them in the record (`run-0-legacy-NNNN`, which sort before the engine's
/// `run-<16 hex>` ids), each with an admitted and a terminal receipt and a
/// budget, and `tombstones` event tombstones.
fn inject_legacy(s: &mut State, cohorts: usize, tombstones: usize) {
    for i in 0..cohorts {
        let id = format!("run-0-legacy-{i:04}");
        let mut run = completed_run(&id);
        for kind in ["admitted", "terminal"] {
            s.sequence += 1;
            s.receipts.push(Receipt {
                sequence: s.sequence,
                run: id.clone(),
                step: "main".into(),
                at_ms: 1,
                kind: kind.into(),
                detail: json!({}),
            });
        }
        run.terminal_sequence = Some(s.sequence);
        s.runs.insert(id.clone(), run);
        s.budgets.insert(id, Budget::default());
    }
    for i in 0..tombstones {
        s.retention
            .event_tombstones
            .insert(format!("tomb-{i:04}"), tombstone(&format!("d-{i}")));
    }
}
/// Store `s` over the namespace as an older engine stored it: indexes not
/// initialized, and no drain state.
fn store_legacy<S: Storage>(store: &S, mut s: State) {
    s.indexes.initialized = false;
    let mut value = serde_json::to_value(&s).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    let s: State = serde_json::from_value(value).unwrap();
    store.acquire("test", "writer").unwrap();
    let revision = store.load("test").unwrap().revision;
    store
        .compare_and_swap("test", "writer", revision, &s)
        .unwrap();
    store.release("test", "writer").unwrap();
}
/// A legacy namespace (the brief's helper): `"a"` installed and activated
/// through a throwaway engine, then `cohorts` settled legacy cohorts and
/// `tombstones` event tombstones; `edit` may change the State first.
fn legacy_with(cohorts: usize, tombstones: usize, edit: impl FnOnce(&mut State)) -> MemoryStorage {
    let store = MemoryStorage::retaining_settled();
    let e = Engine::new(store.clone(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    e.release().unwrap();
    let mut s = store.load("test").unwrap().state;
    inject_legacy(&mut s, cohorts, tombstones);
    edit(&mut s);
    store_legacy(&store, s);
    store
}
fn legacy(cohorts: usize, tombstones: usize) -> MemoryStorage {
    legacy_with(cohorts, tombstones, |_| {})
}
/// A new namespace `"test"` in `store` holding `state` (an older engine's
/// State, which has no retention clock), written by direct CAS.
fn fresh_legacy<S: Storage>(store: S, state: &State) -> S {
    store.acquire("test", "writer").unwrap();
    store.compare_and_swap("test", "writer", 0, state).unwrap();
    store.release("test", "writer").unwrap();
    store
}
fn legacy_ids(e: &Engine<impl Storage>) -> Vec<String> {
    inspect_all(e, InspectionKind::Runs, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .filter(|id| id.starts_with("run-0-legacy-"))
        .collect()
}
/// `state` as the engine aq-server vendors (0173673) stores it: none of the
/// fields this branch added (window, eviction order, clock, drain, new
/// policy fields, new indexes, dispatch sequences).
fn old_engine_shape(state: &State) -> State {
    let mut value = serde_json::to_value(vendored_shape(state)).unwrap();
    let retention = value["retention"].as_object_mut().unwrap();
    for f in [
        "clock_ms",
        "expiry",
        "admission_expires_at_ms",
        "stale_floor_ms",
        "replay_order",
        "replay_held",
        "drain",
    ] {
        retention.remove(f);
    }
    let policy = retention["policy"].as_object_mut().unwrap();
    for f in [
        "admission_replay_ms",
        "replay_keep",
        "history_keep",
        "drain_cohorts_per_commit",
        "drain_entries_per_commit",
    ] {
        policy.remove(f);
    }
    for intent in value["intents"].as_object_mut().unwrap().values_mut() {
        intent.as_object_mut().unwrap().remove("dispatch_sequence");
    }
    serde_json::from_value(value).unwrap()
}

/// The brief's test: the open commit and each later commit move at most 50
/// settled legacy cohorts to history and drop at most 500 tombstones; the
/// drain then records that it is done; every legacy run is in history.
#[test]
fn legacy_namespace_drains_50_cohorts_and_500_entries_per_commit() {
    let e = Engine::new(legacy(120, 1_200), "test", "i-1").unwrap(); // commit 1
    let count = |e: &Engine<MemoryStorage>| {
        let s = e.snapshot().unwrap();
        (s.runs.len(), s.retention.event_tombstones.len())
    };
    assert_eq!(count(&e), (70, 700));
    assert!(!e.snapshot().unwrap().retention.drain.done);
    e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    assert_eq!(count(&e), (21, 200)); // 20 legacy cohorts left plus the new, still live run
    e.admit("a", "main", Value::Null, authority(), 2).unwrap();
    assert_eq!(count(&e), (2, 0));
    assert!(e.snapshot().unwrap().retention.drain.done);
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// The brief's test, with live legacy runs of three shapes: five admitted
/// runs, and a parent waiting on its child, which waits for an event. The
/// drain moves the settled cohorts and leaves the live ones; each live one
/// settles normally and leaves in the commit that settles it.
#[test]
fn legacy_live_cohorts_survive_the_drain_and_leave_when_they_finish() {
    let store = MemoryStorage::retaining_settled();
    let e = Engine::new(store.clone(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    let child = wait_artifact();
    install(&e, child.clone(), "waiter");
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let parent = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let live: Vec<String> = (0..5)
        .map(|_| e.admit("a", "main", Value::Null, authority(), 0).unwrap())
        .collect();
    e.release().unwrap();
    let mut s = store.load("test").unwrap().state;
    let kid = s.runs[&parent].children[0].clone();
    assert_eq!(s.runs[&parent].status, RunStatus::Waiting);
    assert_eq!(s.runs[&kid].status, RunStatus::Waiting);
    inject_legacy(&mut s, 60, 0);
    store_legacy(&store, s);
    let e = Engine::new(store, "test", "i-1").unwrap();
    e.install(done_artifact()).unwrap(); // one more commit (a no-op install)
    let s = e.snapshot().unwrap();
    assert_eq!(legacy_ids(&e).len(), 60);
    assert!(s.runs.keys().all(|id| !id.starts_with("run-0-legacy-")));
    assert_eq!(s.runs.len(), 7);
    for id in &live {
        assert!(e.advance(id, 1).unwrap());
        assert!(!e.snapshot().unwrap().runs.contains_key(id), "{id} left");
    }
    e.admit_event("host", event("for-the-child", 2), 1).unwrap();
    for _ in 0..4 {
        e.drive(&mut FakeHost::default(), 2, 10).unwrap();
    }
    assert!(e.snapshot().unwrap().runs.is_empty());
    let runs = inspect_all(&e, InspectionKind::Runs, None);
    assert_eq!(runs.len(), 67);
    let outcome = |id: &str| {
        runs.iter()
            .find(|r| r["id"] == id)
            .map(|r| r["outcome"].clone())
    };
    assert_eq!(outcome(&parent), Some(json!("ok")));
    assert_eq!(outcome(&kid), Some(json!("ok")));
}
/// Mid-drain, inspection shows every legacy run exactly once, whether it is
/// still in the record or already in history.
#[test]
fn inspection_lists_each_legacy_run_once_at_every_commit_of_the_drain() {
    let e = Engine::new(
        legacy_with(120, 0, |s| s.retention.policy.drain_cohorts_per_commit = 7),
        "test",
        "i-1",
    )
    .unwrap();
    let left = |e: &Engine<MemoryStorage>| {
        e.snapshot()
            .unwrap()
            .runs
            .keys()
            .filter(|id| id.starts_with("run-0-legacy-"))
            .count()
    };
    let listed_once = |e: &Engine<MemoryStorage>, step: usize| {
        let mut ids = legacy_ids(e);
        assert_eq!(ids.len(), 120, "step {step}");
        ids.dedup();
        assert_eq!(ids.len(), 120, "step {step}: listed twice");
    };
    assert_eq!(left(&e), 113);
    let mut step = 0;
    while !e.snapshot().unwrap().retention.drain.done {
        listed_once(&e, step);
        let before = left(&e);
        // Two commits: each moves at most 7 cohorts, its own included.
        let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
        listed_once(&e, step);
        assert!(e.advance(&id, 1).unwrap());
        assert!(before - left(&e) <= 7 + 6, "step {step}");
        step += 1;
        assert!(step < 30);
    }
    listed_once(&e, step);
    assert_eq!(left(&e), 0);
}
/// Drained cohorts go through the rolling history trim and sort as the
/// oldest: with `history_keep` 30, history keeps the new runs finished
/// during the drain and the 25 newest legacy runs.
#[test]
fn drained_legacy_runs_are_the_oldest_in_the_rolling_history() {
    let e = Engine::new(
        legacy_with(120, 0, |s| s.retention.policy.history_keep = 30),
        "test",
        "i-1",
    )
    .unwrap();
    let new: Vec<String> = (0..5)
        .map(|t| {
            let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
            assert!(e.advance(&id, t).unwrap());
            id
        })
        .collect();
    assert!(e.snapshot().unwrap().retention.drain.done);
    let listed: BTreeSet<String> = inspect_all(&e, InspectionKind::Runs, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    let expected: BTreeSet<String> = (95..120)
        .map(|i| format!("run-0-legacy-{i:04}"))
        .chain(new)
        .collect();
    assert_eq!(listed, expected);
}
/// Receipts owned by no run that an older engine left (`admin`) leave in
/// bounded steps, counted against the entries bound, each dated by its own
/// time; an admin receipt written during the drain still leaves with its
/// own commit.
#[test]
fn legacy_admin_receipts_leave_in_bounded_steps_dated_by_their_own_time() {
    let store = legacy_with(0, 0, |s| {
        for i in 0..1_200 {
            s.sequence += 1;
            s.receipts.push(Receipt {
                sequence: s.sequence,
                run: "admin".into(),
                step: "controls".into(),
                at_ms: i,
                kind: "activation_controls".into(),
                detail: json!({}),
            });
        }
    });
    let e = Engine::new(store, "test", "i-1").unwrap();
    assert_eq!(e.snapshot().unwrap().receipts.len(), 700);
    e.set_activation_controls("a", ActivationControls::default(), 5_000)
        .unwrap();
    assert_eq!(
        e.snapshot().unwrap().receipts.len(),
        200,
        "its own left too"
    );
    e.install(done_artifact()).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.is_empty() && s.retention.drain.done);
    let admin = inspect_all(&e, InspectionKind::Receipts, Some("admin"));
    assert_eq!(admin.len(), 1_201);
    // Each row carries its own time: deleting history before 600 removes
    // exactly the 600 receipts written before it.
    assert_eq!(e.delete_history_before(600).unwrap(), 600);
}
/// Legacy keyed receipts and events (no expiry record, no window or
/// eviction-order key) are indexed one entry at a time here (entries bound
/// 1): each gets its expiry record and window key, and joins the eviction
/// order unless live work holds it (a live run's receipt goes to
/// `replay_held`; an event a live wait needs stays out). Throughout, a
/// duplicate of each is still recognised. Afterwards the rolling bound
/// applies to them like to any entry.
#[test]
fn legacy_keyed_receipts_and_events_join_the_window_and_the_eviction_order() {
    let e = Engine::new(MemoryStorage::retaining_settled(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    install(&e, wait_artifact(), "waiter");
    let request = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    };
    let done = e.admit_once(request("k-done"), 10).unwrap();
    assert!(e.advance(&done.receipt.run_id, 10).unwrap());
    let live = e.admit_once(request("k-live"), 11).unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 12)
        .unwrap();
    e.advance(&waiter, 12).unwrap();
    let free = e
        .admit_event("host", correlated_event("free", "nobody"), 13)
        .unwrap()
        .key;
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 1;
    e.release().unwrap();
    let store = fresh_legacy(MemoryStorage::retaining_settled(), &legacy);
    let e = Engine::new(store, "test", "i-1").unwrap();
    let mut commits = 0;
    while !e.snapshot().unwrap().retention.drain.done {
        assert!(e.admit_once(request("k-done"), 20).unwrap().duplicate);
        assert!(e.admit_once(request("k-live"), 20).unwrap().duplicate);
        assert!(
            e.admit_event("host", correlated_event("free", "nobody"), 20)
                .unwrap()
                .duplicate
        );
        commits += 1;
        assert!(commits < 20);
    }
    let s = e.snapshot().unwrap();
    for (key, receipt) in [("k-done", &done.receipt), ("k-live", &live.receipt)] {
        let at = s.retention.admission_expires_at_ms.get(key).copied();
        assert_eq!(at, Some(receipt.accepted_at_ms + 604_800_000), "{key}");
        assert!(window_entries(&s).contains(&(at.unwrap(), format!("k:{key}"))));
    }
    assert!(
        s.retention
            .replay_order
            .contains_key(&retention::admission_order_key(&done.receipt))
    );
    assert_eq!(
        s.retention.replay_held.get(&live.receipt.run_id),
        Some(&"k-live".to_string())
    );
    assert!(
        s.retention
            .replay_order
            .contains_key(&retention::event_order_key(&s.events[&free]))
    );
    let at = retention::event_expiry(&s, &free).unwrap();
    assert!(window_entries(&s).contains(&(at, format!("e:{free}"))));
    assert_eq!(order_members(&s).len(), 2, "{:?}", order_members(&s));
    // The live run finishes: its receipt joins the order. With
    // `replay_keep` 1 the oldest legacy entries are then evicted.
    assert!(e.advance(&live.receipt.run_id, 30).unwrap());
    assert_eq!(order_members(&e.snapshot().unwrap()).len(), 3);
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1,
        ..e.snapshot().unwrap().retention.policy
    })
    .unwrap();
    // The newest by acceptance is the event; both keyed receipts go.
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), vec![format!("e:{free}")]);
    assert!(s.keyed_admissions.is_empty());
    assert!(!e.admit_once(request("k-done"), 31).unwrap().duplicate);
}
/// The drain's bounds are policy fields (defaults 50 and 500), read from
/// the stored policy, and must be positive.
#[test]
fn drain_bounds_come_from_the_stored_policy_and_must_be_positive() {
    let policy = RetentionPolicy::default();
    assert_eq!(
        (
            policy.drain_cohorts_per_commit,
            policy.drain_entries_per_commit
        ),
        (50, 500)
    );
    let decoded: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(
        (
            decoded.drain_cohorts_per_commit,
            decoded.drain_entries_per_commit
        ),
        (50, 500)
    );
    let e = Engine::new(
        legacy_with(120, 30, |s| {
            s.retention.policy.drain_cohorts_per_commit = 10;
            s.retention.policy.drain_entries_per_commit = 3;
        }),
        "test",
        "i-1",
    )
    .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        (s.runs.len(), s.retention.event_tombstones.len()),
        (110, 27)
    );
    for bad in [
        RetentionPolicy {
            drain_cohorts_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            drain_entries_per_commit: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(matches!(
            e.set_retention_policy(bad),
            Err(Error::Refused(m)) if m == "drain bounds must be positive"
        ));
    }
}
/// Progress is stored: a restart continues where the drain stopped, and no
/// legacy run is written to history twice.
#[test]
fn a_drain_continues_after_a_restart() {
    let store = legacy(120, 0);
    let e = Engine::new(store.clone(), "test", "i-1").unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 70);
    e.release().unwrap();
    let e = Engine::new(store.clone(), "test", "i-2").unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 20);
    e.release().unwrap();
    let e = Engine::new(store, "test", "i-3").unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.retention.drain.done);
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// A namespace this engine created has nothing to drain: its first commit
/// (the open commit) records the drain as done.
#[test]
fn a_new_namespace_is_drained_by_its_first_commit() {
    let e = engine();
    assert!(e.snapshot().unwrap().retention.drain.done);
}
/// The carried hazard, bounded: an older engine's namespace with many
/// events of one completion contract, before and after the dispatch of an
/// asynchronous operation of that contract whose handle is lost (no
/// dispatch sequence, the old engine's shape). The drain judges the legacy
/// events a bounded number per commit (entries bound 8 here); a change of
/// the contract's uncertain operations before the drain has reached an
/// event leaves it to the drain. So when the host finds the handle in the
/// first commits after the upgrade (`recover`, mid-drain), no commit — the
/// open commit, the binding one, nor a drain commit — judges all of them:
/// on bound record containers the most any one commit reads does not
/// depend on how many there are. With the handle found after the drain,
/// the events it held rejoin through `contract_events`, which the drain
/// filled. Either way every event ends unheld in the eviction order and the
/// run finishes `ok`.
fn legacy_hazard(before: usize, after: usize, bind_mid_drain: bool) -> usize {
    use std::sync::atomic::Ordering;
    let e = Engine::new(RecordStorage::new(), "test", "i-1").unwrap();
    let (a, completion) = async_artifact();
    install_on(&e, a, "async");
    install_on(&e, done_artifact(), "a");
    let accept = |label: &str, i: usize, t: i64| {
        let envelope = EventEnvelope {
            correlation: BTreeMap::from([("operation".into(), format!("{label}-{i}"))]),
            ..async_event(&completion, &format!("{label}{i}"))
        };
        e.admit_event("host", envelope, t).unwrap();
    };
    for i in 0..before {
        accept("b", i, i as i64);
    }
    let run = e
        .admit("async", "main", Value::Null, authority(), 1_000)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 1_000, 10).unwrap();
    for i in 0..after {
        accept("a", i, 1_001);
    }
    let done = e
        .admit_event("host", async_event(&completion, "done"), 1_001)
        .unwrap()
        .key;
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 8;
    legacy.retention.policy.replay_keep = 100_000;
    e.release().unwrap();
    let store = fresh_legacy(RecordStorage::new(), &legacy);
    let stats = store.stats.clone();
    let reads = || stats.record_reads.load(Ordering::Relaxed);
    let mut work = vec![];
    let measured = |work: &mut Vec<usize>, f: &mut dyn FnMut()| {
        let before = reads();
        f();
        work.push(reads() - before);
    };
    let mut e = None;
    measured(&mut work, &mut || {
        e = Some(Engine::new(store.clone(), "test", "i-2").unwrap())
    });
    let e = e.unwrap();
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    if bind_mid_drain {
        measured(&mut work, &mut || {
            e.recover(&mut host, 1_002).unwrap();
        });
    }
    let mut t = 1_003;
    while !e.snapshot().unwrap().retention.drain.done {
        measured(&mut work, &mut || e.install(done_artifact()).unwrap());
        t += 1;
        assert!(t < 2_000);
    }
    if !bind_mid_drain {
        e.recover(&mut host, t).unwrap();
    }
    for t in t..t + 5 {
        e.drive(&mut host, t, 20).unwrap();
    }
    let s = e.snapshot().unwrap();
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
    // Every event (the delivered completion too) is unheld and in the
    // eviction order.
    assert!(s.events.contains_key(&done));
    assert_eq!(
        s.retention.replay_order.len(),
        before + after + 1,
        "{work:?}"
    );
    assert!(s.indexes.contract_events_complete);
    work.into_iter().max().unwrap()
}
#[test]
fn legacy_events_are_ordered_in_bounded_steps_and_never_all_rejudged_at_once() {
    // A drain commit reads the same for any number of events (8 judged).
    // The binding commits re-judge only held events the drain has already
    // reached (at most a few commits' bound), so their cost varies with
    // which events those are, not with how many there are: three times the
    // events cost no more (10 % allowed for which ones were reached).
    let small = legacy_hazard(40, 40, true);
    let large = legacy_hazard(120, 120, true);
    assert!(large <= small + small / 10, "{large} vs {small}");
    legacy_hazard(20, 20, false);
}
/// The same when the open commit rebuilds the indexes (a State whose
/// indexes were never initialized): the rebuild puts every event into
/// `contract_events` at once, and the drain still judges the older
/// engine's events itself. When the handle of the operation that held 40
/// of them is found mid-drain, they do not all join the eviction order in
/// that commit: only those the drain has reached; the drain then judges the
/// rest, 4 per commit here.
#[test]
fn a_rebuilt_legacy_namespace_leaves_its_events_to_the_drain() {
    let e = engine();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    e.admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    for i in 0..40 {
        let envelope = EventEnvelope {
            correlation: BTreeMap::from([("operation".into(), format!("after-{i}"))]),
            ..async_event(&completion, &format!("after-{i}"))
        };
        e.admit_event("host", envelope, 1).unwrap();
    }
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 4;
    legacy.indexes.initialized = false;
    e.release().unwrap();
    let e = Engine::new(
        fresh_legacy(MemoryStorage::retaining_settled(), &legacy),
        "test",
        "i-1",
    )
    .unwrap();
    let held = e.snapshot().unwrap();
    assert!(
        held.indexes.contract_events_complete,
        "the rebuild filled it"
    );
    assert!(held.retention.replay_order.len() <= 4);
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 2).unwrap();
    // `recover` is two commits: the drain reached at most 3 × 4 events.
    let s = e.snapshot().unwrap();
    assert!(s.intents.values().all(|i| i.handle.is_some()));
    assert!(
        s.retention.replay_order.len() <= 12,
        "{} joined at once",
        s.retention.replay_order.len()
    );
    while !e.snapshot().unwrap().retention.drain.done {
        e.install(done_artifact()).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().retention.replay_order.len(), 40);
}
/// `tests/fixtures/legacy_state_0173673.json` is a namespace written by the
/// engine aq-server vendors (0173673) on its in-memory store, by the
/// generator kept in `.local/task9/generator/` (see the Task 9 report): a
/// GC (short windows) that removed 5 finished runs and 3 expired events
/// (tombstones, correlation fences, disposals); 33 finished cohorts still
/// in the record (two with operations, one keyed); live runs: 3 admitted,
/// one waiting for a 24 h event, a terminal parent with a waiting
/// independent child, a parent waiting on its waiting child, a live keyed
/// admission, and an asynchronous operation dispatched without a handle;
/// an admin receipt; events of the completion contract before and after
/// that dispatch; ten events inside their 24 h window.
fn vendored_fixture() -> State {
    serde_json::from_str(include_str!("fixtures/legacy_state_0173673.json")).unwrap()
}
/// The vendored engine's namespace opens and drains to the end while new
/// runs are admitted and served at every commit (bounds 4 cohorts and 3
/// entries per commit, from its stored policy, so the drain spans many
/// commits); no commit fails. Legacy duplicates are recognised until their
/// entries leave by the normal rules, every legacy live run settles
/// normally, and inspection lists every legacy run exactly once at every
/// step.
fn vendored_namespace_drains_while_new_runs_are_served<S: Storage + Clone>(store: S) {
    let legacy = vendored_fixture();
    let finished: BTreeSet<String> = legacy
        .runs
        .values()
        .filter(|r| r.status.terminal())
        .map(|r| r.id.clone())
        .collect();
    let all_legacy: BTreeSet<String> = legacy.runs.keys().cloned().collect();
    let day = legacy.waits.values().find_map(|w| w.event.clone()).unwrap();
    let completion = legacy
        .events
        .values()
        .find(|e| e.envelope.event_id.starts_with("async-before"))
        .unwrap()
        .envelope
        .contract
        .clone();
    let day_event = |id: &str, invocation: &str| EventEnvelope {
        contract: day.clone(),
        correlation: BTreeMap::from([("invocation".into(), invocation.into())]),
        subject: None,
        revision: None,
        ..event(id, 1)
    };
    let mut stored = legacy.clone();
    stored.retention.policy.drain_cohorts_per_commit = 4;
    stored.retention.policy.drain_entries_per_commit = 3;
    let mut value = serde_json::to_value(&stored).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    let stored: State = serde_json::from_value(value).unwrap();
    store.acquire("test", "vendored").unwrap();
    store
        .compare_and_swap("test", "vendored", 0, &stored)
        .unwrap();
    store.release("test", "vendored").unwrap();
    let e = Engine::new(store, "test", "upgraded").unwrap();
    assert!(
        !e.snapshot().unwrap().retention.drain.done,
        "the drain spans more than the open commit"
    );
    let keyed = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "done".into(),
        expected_activation_revision: legacy.bindings["done"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    };
    let listed_once = |e: &Engine<S>, step: &str| {
        let ids: Vec<String> = inspect_all(e, InspectionKind::Runs, None)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .filter(|id| all_legacy.contains(id))
            .collect();
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{step}: listed twice");
        assert_eq!(unique.len(), all_legacy.len(), "{step}: missing");
    };
    listed_once(&e, "open");
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    let mut t = 30_000;
    let mut new_runs = vec![];
    let mut commits = 0;
    while !e.snapshot().unwrap().retention.drain.done {
        t += 1;
        // New work at every step, while the drain goes on.
        let id = e
            .admit("done", "main", Value::Null, authority(), t)
            .unwrap();
        e.drive(&mut host, t, 20).unwrap();
        new_runs.push(id);
        // Legacy duplicates are still recognised.
        assert!(e.admit_once(keyed("k-done"), t).unwrap().duplicate);
        assert!(e.admit_once(keyed("k-live"), t).unwrap().duplicate);
        assert!(
            e.admit_event("host", day_event("day-3", "other-3"), t)
                .unwrap()
                .duplicate
        );
        listed_once(&e, &format!("commit {commits}"));
        commits += 1;
        assert!(commits < 200);
    }
    // The live legacy runs settle normally: the waiting run and both
    // children receive their event, the operation's handle is found and its
    // completion arrives, the keyed and admitted runs advance.
    e.admit_event("host", day_event("wake", "exact-invocation"), t + 1)
        .unwrap();
    e.recover(&mut host, t + 1).unwrap();
    e.admit_event(
        "host",
        EventEnvelope {
            contract: completion.clone(),
            correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
            ..async_event(&async_artifact().1, "completion")
        },
        t + 2,
    )
    .unwrap();
    for t in t + 2..t + 10 {
        e.drive(&mut host, t, 50).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty(), "{:?}", s.runs.keys().collect::<Vec<_>>());
    let r = &s.retention;
    assert!(r.event_tombstones.is_empty() && r.expired_correlations.is_empty());
    assert!(r.disposals.is_empty());
    let runs = inspect_all(&e, InspectionKind::Runs, None);
    for id in all_legacy.iter().chain(&new_runs) {
        let found: Vec<&Value> = runs.iter().filter(|r| r["id"] == id.as_str()).collect();
        assert_eq!(found.len(), 1, "{id}");
        if !finished.contains(id) {
            assert_eq!(found[0]["outcome"], "ok", "{id}: {:?}", found[0]);
        }
    }
    // After the drain a re-delivery of an event the old GC disposed of is
    // judged by the rules of this engine: its tombstone has left, so it is
    // accepted as new.
    assert!(
        !e.admit_event("host", event("gc-old-0", 2), t + 20)
            .unwrap()
            .duplicate
    );
}
#[test]
fn memory_storage_vendored_namespace_drains_while_new_runs_are_served() {
    vendored_namespace_drains_while_new_runs_are_served(MemoryStorage::retaining_settled());
}
#[test]
fn record_storage_vendored_namespace_drains_while_new_runs_are_served() {
    vendored_namespace_drains_while_new_runs_are_served(RecordStorage::new());
}

// --- Task 10, item D: per-commit bounds are policy fields -------------------

/// The owner's rule "no hard-coded limits": the window drop, the eviction,
/// the history trim and the GC page bound are policy fields with the old
/// constants as defaults, read from the stored policy, and must be
/// positive.
#[test]
fn per_commit_bounds_are_policy_fields_read_from_the_stored_policy() {
    let defaults = RetentionPolicy::default();
    let bounds = |p: &RetentionPolicy| {
        (
            p.window_drops_per_commit,
            p.replay_evictions_per_commit,
            p.history_trim_per_commit,
            p.gc_page_limit,
            p.settle_cohorts_per_commit,
        )
    };
    assert_eq!(bounds(&defaults), (64, 64, 64, 1_000, 50));
    let decoded: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(bounds(&decoded), (64, 64, 64, 1_000, 50));
    // The window drop: 10 expired keyed receipts, 3 per commit.
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        window_drops_per_commit: 3,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..10 {
        keyed_done(&e, &format!("k-{i:02}"), i);
    }
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 7);
    // The eviction: `replay_keep` lowered to 1 over 7 unheld entries, 2
    // evicted per commit; `collect_garbage` reports them and `more` until
    // the set is down to `replay_keep`.
    let v = keep_engine(10);
    for i in 0..7 {
        keyed_done(&v, &format!("v-{i:02}"), i);
    }
    v.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        replay_keep: 1,
        replay_evictions_per_commit: 2,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(v.snapshot().unwrap().keyed_admissions.len(), 5);
    let r = v.collect_garbage(10, 10).unwrap();
    assert_eq!((r.evicted, r.more), (2, true));
    let r = v.collect_garbage(11, 10).unwrap();
    assert_eq!((r.evicted, r.more), (2, false));
    assert_eq!(v.snapshot().unwrap().keyed_admissions.len(), 1);
    // The history trim: 6 finished runs, `history_keep` lowered to 1, one
    // more than written per commit.
    let h = Engine::new(MemoryStorage::default(), "test", "i").unwrap();
    install(&h, done_artifact(), "a");
    for t in 0..6 {
        let id = h.admit("a", "main", Value::Null, authority(), t).unwrap();
        h.advance(&id, t).unwrap();
    }
    h.set_retention_policy(RetentionPolicy {
        history_keep: 1,
        history_trim_per_commit: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let id = h.admit("a", "main", Value::Null, authority(), 10).unwrap();
    h.advance(&id, 10).unwrap();
    // 7 runs written; the finishing commit wrote 1 and trimmed 1 + 1.
    assert_eq!(inspect_all(&h, InspectionKind::Runs, None).len(), 5);
    // The GC page bound.
    h.set_retention_policy(RetentionPolicy {
        gc_page_limit: 5,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert!(matches!(
        h.collect_garbage(20, 6),
        Err(Error::Refused(m)) if m == "GC page limit must be 1..5"
    ));
    h.collect_garbage(20, 5).unwrap();
    for bad in [
        RetentionPolicy {
            window_drops_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            replay_evictions_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            history_trim_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            gc_page_limit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            settle_cohorts_per_commit: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(matches!(
            h.set_retention_policy(bad),
            Err(Error::Refused(m)) if m == "per-commit retention bounds must be positive"
        ));
    }
}
/// The eviction licence reads its bound from the policy: a commit that
/// evicts more than `replay_evictions_per_commit` entries is refused.
#[test]
fn the_eviction_licence_reads_its_bound_from_the_policy() {
    let store = MemoryStorage::default();
    let mut state = bulk_eviction_state();
    state.retention.policy.replay_evictions_per_commit = 2;
    let revision = seed_eviction_state(&store, "ns", &state);
    let mut next = store.load("ns").unwrap().state;
    for key in ["b000", "b001", "b002"] {
        evict_keyed(&mut next, key);
    }
    assert!(refused_with(
        store.compare_and_swap("ns", "w", revision, &next),
        retention::REFUSE_EVICTION
    ));
    let mut next = store.load("ns").unwrap().state;
    for key in ["b000", "b001"] {
        evict_keyed(&mut next, key);
    }
    store.compare_and_swap("ns", "w", revision, &next).unwrap();
}

// --- Task 10, item B: collect_garbage is a thin wrapper ---------------------

/// The plan's test: `collect_garbage` is optional; it runs the same bounded
/// work as every commit (here the window drop, 64 per commit) and reports
/// it. Its clock never moves back.
#[test]
fn collect_garbage_is_optional_and_reports_the_bounded_drop() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..70 {
        let d = e.admit_once(keyed(&e, &format!("k{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    let report = e.collect_garbage(5_000, 10).unwrap();
    assert_eq!((report.runs, report.admissions, report.more), (0, 64, true));
    let report = e.collect_garbage(5_001, 10).unwrap();
    assert_eq!((report.admissions, report.more), (6, false));
    assert!(e.snapshot().unwrap().retention.disposals.is_empty());
    assert!(matches!(
        e.collect_garbage(4_000, 10),
        Err(Error::Refused(_))
    ));
}
/// `collect_garbage` creates nothing the rest of the engine has stopped
/// creating: an expired event leaves by the commit's window drop, with no
/// event tombstone, correlation fence or disposal record, and a redelivery
/// is new.
#[test]
fn collect_garbage_writes_no_tombstone_fence_or_disposal() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        audit_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap();
    let report = e.collect_garbage(5_000, 100).unwrap();
    assert_eq!((report.events, report.runs), (1, 0));
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.event_tombstones.is_empty());
    assert!(s.retention.expired_correlations.is_empty());
    assert!(s.retention.disposals.is_empty());
    assert!(
        !e.admit_event("host", event("e1", 2), 5_001)
            .unwrap()
            .duplicate
    );
}
/// On a namespace an older engine wrote, `collect_garbage` advances the
/// same drain a commit does: each call moves the next bounded batch of
/// settled cohorts to history (reported as `runs`) and drops legacy
/// tombstones, and reports `more` until the drain is done. No run leaves
/// without its history.
#[test]
fn collect_garbage_on_a_legacy_namespace_advances_the_drain() {
    let e = Engine::new(
        legacy_with(120, 30, |s| s.retention.policy.audit_ms = 1),
        "test",
        "i-1",
    )
    .unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 70);
    let mut moved = vec![];
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls < 10);
        let report = e.collect_garbage(1_000_000 + calls, 1_000).unwrap();
        moved.push(report.runs);
        if !report.more {
            break;
        }
    }
    assert_eq!(&moved[..2], &[50, 20], "{moved:?}");
    let s = e.snapshot().unwrap();
    assert!(s.retention.drain.done && s.runs.is_empty());
    assert!(s.retention.event_tombstones.is_empty() && s.retention.disposals.is_empty());
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// A finished run is never removed without its history, by any call: even
/// over a State whose drain claims to be done while settled cohorts remain
/// (the case the old collector removed without history), `collect_garbage`
/// removes nothing without history and writes no disposal record.
#[test]
fn collect_garbage_never_removes_a_finished_run_without_history() {
    let store = legacy_with(5, 0, |s| s.retention.policy.audit_ms = 1);
    let e = Engine::new(store.clone(), "test", "i-1").unwrap();
    // The drain has taken the 5 cohorts; put 5 more back behind a drain
    // marked done, as no engine writes it.
    let mut s = e.snapshot().unwrap();
    for i in 0..5 {
        let id = format!("run-0-stale-{i:04}");
        s.runs.insert(id.clone(), completed_run(&id));
        s.indexes
            .root_members
            .entry(id.clone())
            .or_default()
            .insert(id.clone());
        s.indexes.root_last_activity.insert(id.clone(), 1);
        s.indexes.run_roots.insert(id.clone(), id);
    }
    let revision = e.store.load("test").unwrap().revision;
    e.store
        .compare_and_swap("test", "i-1", revision, &s)
        .unwrap();
    assert!(e.snapshot().unwrap().retention.drain.done);
    let report = e.collect_garbage(1_000_000, 1_000).unwrap();
    assert_eq!(report.runs, 0);
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.runs.len(),
        5,
        "left as they are, never removed without history"
    );
    assert!(s.retention.disposals.is_empty());
}

// --- Task 10, item C: a lost handle's hold lapses ----------------------------

/// An asynchronous operation whose dispatch lost its handle (dispatched at
/// 0, command deadline 100), with a 10 s event window (`replay_keep` 3):
/// its completion `done` and another event of its contract arrive at 500
/// and 600, while it is uncertain, then four unrelated events. Returns the
/// engine, the run, the two held keys and the host.
fn lapse_fixture() -> (Engine<MemoryStorage>, String, [String; 2], FakeHost) {
    let e = keep_engine(3);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 3,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!((op.handle, op.deadline_at_ms), (None, 100));
    let done = e
        .admit_event("host", async_event(&completion, "done"), 500)
        .unwrap()
        .key;
    let other = EventEnvelope {
        correlation: BTreeMap::from([("operation".into(), "elsewhere".into())]),
        ..async_event(&completion, "other")
    };
    let other = e.admit_event("host", other, 600).unwrap().key;
    for i in 0..4 {
        e.admit_event(
            "host",
            correlated_event(&format!("unrelated-{i}"), &format!("inv-{i}")),
            700 + i,
        )
        .unwrap();
    }
    (e, run, [done, other], host)
}
fn held(s: &State, key: &str) -> bool {
    s.events.contains_key(key) && !order_members(s).contains(&format!("e:{key}"))
}
/// Ruling C: an operation whose dispatch lost its handle holds the later
/// events of its completion contract and scope only until it is past its
/// command deadline plus the event's window (100 + 10,000 here). Before
/// that they are held (never evicted, not counted); after it they are
/// ordinary unheld entries: they rejoin the eviction order at their
/// acceptance positions, as the oldest, and `replay_keep` pushes them out
/// (their own window has not passed yet: 10,500 and 10,600).
#[test]
fn a_lost_handle_hold_lapses_after_the_command_deadline_plus_the_window() {
    let (e, _, [done, other], _) = lapse_fixture();
    let s = e.snapshot().unwrap();
    assert!(held(&s, &done) && held(&s, &other));
    e.admit_event("host", correlated_event("before", "inv-b"), 10_099)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(held(&s, &done) && held(&s, &other), "held until 10,100");
    e.admit_event("host", correlated_event("after", "inv-a"), 10_100)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.events.contains_key(&done) && !s.events.contains_key(&other),
        "lapsed: evicted by count as the oldest"
    );
    assert_eq!(order_members(&s).len(), 3);
    assert!(
        s.indexes
            .uncertain_completion_contracts
            .values()
            .all(BTreeSet::is_empty)
    );
}
/// Ruling C, the handle found before the hold lapses: the held completion
/// matches the handle and the run finishes `ok`, as before.
#[test]
fn a_handle_found_before_the_hold_lapses_takes_its_held_completion() {
    let (e, run, [done, _], mut host) = lapse_fixture();
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 5_000).unwrap();
    for t in 5_000..5_010 {
        e.drive(&mut host, t, 20).unwrap();
    }
    assert!(!e.snapshot().unwrap().runs.contains_key(&run));
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
    assert!(!e.snapshot().unwrap().events.contains_key(&done));
}
/// Ruling C, the handle found after the hold lapsed and its completion was
/// evicted: binding the handle finds no completion. The operation then
/// resolves by the existing deadline and recovery path: it is past its
/// command deadline, so the next drive marks it `Unknown` again ("operation
/// deadline elapsed with unresolved external outcome"), and recovery
/// reconciles it with the host, whose answer (`ok`) completes it.
#[test]
fn a_handle_found_after_the_hold_lapsed_resolves_by_the_deadline_and_recovery_path() {
    let (e, run, [done, _], mut host) = lapse_fixture();
    e.admit_event("host", correlated_event("after", "inv-a"), 10_100)
        .unwrap();
    assert!(!e.snapshot().unwrap().events.contains_key(&done));
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![
            DispatchResult::Accepted {
                handle: "server-operation".into(),
            },
            DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            },
        ],
    );
    e.recover(&mut host, 10_200).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!(op.handle.as_deref(), Some("server-operation"));
    assert_eq!(op.completion_event, None, "its completion is gone");
    e.drive(&mut host, 10_201, 20).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!(op.status, IntentStatus::Unknown);
    assert_eq!(
        op.reason.as_deref(),
        Some("operation deadline elapsed with unresolved external outcome")
    );
    e.recover(&mut host, 10_202).unwrap();
    for t in 10_202..10_212 {
        e.drive(&mut host, t, 20).unwrap();
    }
    assert_eq!(host.reconciliations.len(), 2);
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
}
/// Review M1 (fix round 1): a lapsing hold re-judges the events it held in
/// batches of `window_drops_per_commit` per commit (10 here), not all in
/// the commit that lapses it; the remainder continues in later commits
/// (`more` set meanwhile). 25 events held while the handle was lost join
/// the eviction order 10, 10, 5.
#[test]
fn a_lapsing_hold_rejudges_its_events_in_bounded_batches() {
    let e = keep_engine(1_000);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 1_000,
        window_drops_per_commit: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    e.admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 10).unwrap();
    let held: Vec<String> = (0..25)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("held-{i:02}"))
            };
            e.admit_event("host", envelope, 500 + i).unwrap().key
        })
        .collect();
    let in_order = |e: &Engine<MemoryStorage>| {
        let s = e.snapshot().unwrap();
        let members = order_members(&s);
        held.iter()
            .filter(|k| members.contains(&format!("e:{k}")))
            .count()
    };
    assert_eq!(in_order(&e), 0);
    let mut joined = vec![];
    let mut more = vec![];
    for t in 0..4 {
        let report = e.collect_garbage(10_100 + t, 10).unwrap();
        joined.push(in_order(&e));
        more.push(report.more);
    }
    assert_eq!(joined, [10, 20, 25, 25]);
    assert_eq!(more[..2], [true, true]);
    assert!(!more[3]);
    assert!(e.snapshot().unwrap().retention.rejudge.is_empty());
}
/// The batched re-judge restarts from the earliest change: two operations
/// of one contract lost their handles (dispatched at 0 and 1, lapsing at
/// 10,100 and 10,101). The first lapse starts judging the 30 events both
/// held (still held by the second); the second lapse, a commit later,
/// re-judges them from the start, so every one of them joins the order.
#[test]
fn a_second_lapse_rejudges_from_the_earliest_change() {
    let e = keep_engine(1_000);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 1_000,
        window_drops_per_commit: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    for t in 0..2 {
        e.admit("async", "main", Value::Null, authority(), t)
            .unwrap();
        e.drive(&mut FakeHost::default(), t, 10).unwrap();
    }
    let held: Vec<String> = (0..30)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("held-{i:02}"))
            };
            e.admit_event("host", envelope, 500 + i).unwrap().key
        })
        .collect();
    let in_order = |e: &Engine<MemoryStorage>| {
        let members = order_members(&e.snapshot().unwrap());
        held.iter()
            .filter(|k| members.contains(&format!("e:{k}")))
            .count()
    };
    let mut joined = vec![];
    for t in 0..5 {
        e.collect_garbage(10_100 + t, 10).unwrap();
        joined.push(in_order(&e));
    }
    assert_eq!(joined, [0, 10, 20, 30, 30]);
}

/// Storage writes `to_value_without_artifacts` on every commit: it is the
/// State's own serialization with an empty `artifacts` map, and it decodes.
#[test]
fn stored_value_is_the_state_without_its_artifacts() {
    let mut state = vendored_fixture();
    assert!(!state.artifacts.is_empty());
    let stored = state.to_value_without_artifacts().unwrap();
    state.artifacts.clear();
    assert_eq!(stored, serde_json::to_value(&state).unwrap());
    let decoded: State = serde_json::from_value(stored).unwrap();
    assert!(decoded.artifacts.is_empty());
}
