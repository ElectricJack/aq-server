#![cfg(feature = "postgres-tests")]
//! Live state only (spec 0003) on PostgreSQL: insert-once `run_history` rows
//! written with the commit, and inspection over live rows plus history.
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[allow(dead_code)]
mod support;

/// The server's message (`postgres::Error`'s `Display` is only "db error").
fn db_message(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
}
fn history_state(cohorts: Vec<SettledCohort>) -> State {
    State {
        settled: cohorts,
        ..State::default()
    }
}
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
        authority: Authority {
            actor: "alice".into(),
            scope: "project".into(),
            capabilities: ["execute".to_string()].into(),
        },
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
fn both() -> [(&'static str, PostgresStorage); 2] {
    [("f2", support::protected()), ("f3", support::records())]
}
fn runs_query() -> InspectionQuery {
    InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after: None,
        limit: 1000,
    }
}
fn all_receipt_sequences(store: &PostgresStorage, ns: &str, limit: usize) -> Vec<u64> {
    let mut sequences = vec![];
    let mut after = None;
    loop {
        let page = store
            .inspect(
                ns,
                &InspectionQuery {
                    kind: InspectionKind::Receipts,
                    run: None,
                    after,
                    limit,
                },
            )
            .unwrap();
        assert!(page.items.len() <= limit);
        sequences.extend(page.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        match page.next {
            Some(next) => after = Some(next),
            None => return sequences,
        }
    }
}
fn run_ids(store: &PostgresStorage, ns: &str) -> Vec<Value> {
    store
        .inspect(ns, &runs_query())
        .unwrap()
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect()
}
fn history_rows(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}
fn already_recorded<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALREADY_RECORDED)
}
fn also_live<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALSO_LIVE)
}
fn storage_error<T>(result: Result<T>) -> std::result::Result<T, StorageError> {
    result.map_err(|error| match error {
        Error::Storage(error) => error,
        other => StorageError::Backend(other.to_string()),
    })
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
/// A format 3 transition that carries `cohort` (if any) and stores `live` as
/// a live run (if any). `heavy` also changes a header field that sends the
/// commit through the complete path.
fn transition_with(
    store: &PostgresStorage,
    ns: &str,
    cohort: Option<SettledCohort>,
    live: Option<Run>,
    heavy: bool,
) -> std::result::Result<TransitionOutcome, StorageError> {
    let (mut cohort, mut live) = (cohort, live);
    let mut plan = |s: &mut State| {
        s.settled.extend(cohort.take());
        if let Some(run) = live.take() {
            s.runs.insert(run.id.clone(), run);
        }
        s.sequence += 1;
        if heavy {
            s.execution_denied.insert(format!("denied-{}", s.sequence));
        }
        Ok(true)
    };
    storage_error(store.transition_v1(
        ns,
        "w",
        &mut TransitionRequest {
            operation: "history",
            plan: &mut plan,
        },
    ))
}
/// A format 3 namespace whose State has initialized runtime indexes, so
/// `transition_v1` runs.
fn transition_namespace(label: &str) -> (String, PostgresStorage, u64) {
    let (ns, store) = (support::namespace(label), support::records());
    store.acquire(&ns, "w").unwrap();
    let mut initialized = State::default();
    initialized.indexes.initialized = true;
    let revision = store.compare_and_swap(&ns, "w", 0, &initialized).unwrap();
    (ns, store, revision)
}

#[test]
fn protected_history_rows_are_plaintext_public_and_insert_once() {
    let (ns, store) = (support::namespace("history"), support::protected());
    store.acquire(&ns, "w").unwrap();
    // Protected values that inspection never projects.
    let mut cohort = synthetic_cohort("run-a", 10, &[1, 2]);
    let run = &mut cohort.runs[0];
    run.input = json!({"secret": "PROTECTED_SENTINEL_INPUT"});
    run.output = Some(json!("PROTECTED_SENTINEL_OUTPUT"));
    run.variables
        .insert("v".into(), json!("PROTECTED_SENTINEL_VARIABLE"));
    cohort.receipts[0].detail["private"] = json!("PROTECTED_SENTINEL_DETAIL");
    let revision = store
        .compare_and_swap(&ns, "w", 0, &history_state(vec![cohort]))
        .unwrap();
    let mut c = support::client();
    let rows: i64 = c
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0);
    assert_eq!(rows, 3);
    let envelope = c.query_one("SELECT snapshot IS NULL, protection_key_id IS NOT NULL FROM aq_playbooks.namespaces WHERE namespace=$1", &[&ns]).unwrap();
    assert!(envelope.get::<_, bool>(0) && envelope.get::<_, bool>(1));
    let leaked: bool = c
        .query_one(
            "SELECT bool_or(public_value::text LIKE '%PROTECTED_SENTINEL%') FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0);
    assert!(!leaked);
    let mut tx = support::marked(&mut c, &ns);
    let err = tx
        .execute(
            "UPDATE aq_playbooks.run_history SET finished_at_ms=0 WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("run history rows are immutable"));
    assert!(already_recorded(store.compare_and_swap(
        &ns,
        "w",
        revision,
        &history_state(vec![synthetic_cohort("run-a", 10, &[1, 2])])
    )));
}
#[test]
fn history_union_pages_in_item_key_order() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = history_state(vec![synthetic_cohort("done", 10, &[1, 3, 5])]);
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
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        // page with limit 2 until next is None
        assert_eq!(
            all_receipt_sequences(&store, &ns, 2),
            vec![1, 2, 3, 4, 5],
            "{label}"
        );
    }
}
#[test]
fn rolled_back_commit_leaves_no_history_rows() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        assert_eq!(
            store.compare_and_swap(
                &ns,
                "w",
                9,
                &history_state(vec![synthetic_cohort("run-a", 10, &[1])])
            ),
            Err(StorageError::Conflict)
        );
        assert!(
            store.history_page(&ns, &runs_query()).unwrap().is_empty(),
            "{label}"
        );
    }
}
#[test]
fn delete_history_before_is_an_indexed_delete() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let state = history_state(vec![
            synthetic_cohort("old", 10, &[1]),
            synthetic_cohort("new", 20, &[2]),
        ]);
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        assert_eq!(
            store.delete_history(&ns, "other", 20),
            Err(StorageError::Ownership),
            "{label}"
        );
        assert_eq!(store.delete_history(&ns, "w", 20).unwrap(), 2, "{label}"); // run + receipt of "old"
        assert_eq!(run_ids(&store, &ns), vec![json!("new")], "{label}");
        assert_eq!(all_receipt_sequences(&store, &ns, 10), vec![2], "{label}");
    }
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.batch_execute("SET LOCAL enable_seqscan=off").unwrap();
    let plan: Vec<String> = tx
        .query(
            "EXPLAIN DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND finished_at_ms<$2",
            &[&"ns", &20_i64],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        plan.join("\n").contains("run_history_by_finish"),
        "{plan:?}"
    );
}
#[test]
fn history_rows_need_the_namespace_writer_and_cannot_be_truncated() {
    let (ns, store) = (support::namespace("writer"), support::store());
    store.acquire(&ns, "w").unwrap();
    store
        .compare_and_swap(
            &ns,
            "w",
            0,
            &history_state(vec![synthetic_cohort("a", 10, &[1])]),
        )
        .unwrap();
    let mut c = support::client();
    let insert = "INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) VALUES($1,'runs','forged','forged','forged',0,1,'{}'::jsonb)";
    let err = c.execute(insert, &[&ns]).unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    let err = c
        .execute(
            "DELETE FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    // The writer marker alone does not let a session without the lease delete.
    let mut tx = support::marked(&mut c, &ns);
    let err = tx
        .execute(
            "DELETE FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    drop(tx);
    let err = c
        .batch_execute("TRUNCATE aq_playbooks.run_history")
        .unwrap_err();
    assert_eq!(db_message(&err), "run_history is insert-only");
    assert_eq!(history_rows(&ns), 2); // run + receipt of "a"
}
/// Pins what PostgreSQL does with a commit that carries settled cohorts, on
/// every commit path: the history rows are written with the commit, and a
/// repeated item fails the commit (insert-once) without writing anything.
#[test]
fn settled_cohorts_are_written_on_every_commit_path() {
    // Full compare-and-swap, unprotected format 2.
    let (ns, store) = (support::namespace("f2_plain"), support::store());
    store.acquire(&ns, "w").unwrap();
    let revision = store
        .compare_and_swap(
            &ns,
            "w",
            0,
            &history_state(vec![synthetic_cohort("a", 10, &[1, 2])]),
        )
        .unwrap();
    assert_eq!(history_rows(&ns), 3);
    assert!(already_recorded(store.compare_and_swap(
        &ns,
        "w",
        revision,
        &history_state(vec![
            synthetic_cohort("b", 11, &[3]),
            synthetic_cohort("a", 10, &[1, 2])
        ])
    )));
    assert_eq!(history_rows(&ns), 3);
    assert_eq!(store.load(&ns).unwrap().revision, revision);
    assert_eq!(run_ids(&store, &ns), vec![json!("a")]);

    // Format 3 transitions, incremental and complete.
    let (ns, store, mut revision) = transition_namespace("f3_transition");
    store.reset_transition_stats();
    let committed = transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("inc", 10, &[1])),
        None,
        false,
    )
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    revision += 1;
    assert!(matches!(committed, TransitionOutcome::Committed { revision: r } if r == revision));
    let committed = transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("heavy", 11, &[2])),
        None,
        true,
    )
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    revision += 1;
    assert!(matches!(committed, TransitionOutcome::Committed { revision: r } if r == revision));
    assert_eq!(history_rows(&ns), 4);
    assert_eq!(run_ids(&store, &ns), vec![json!("heavy"), json!("inc")]);
    assert!(already_recorded(transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("inc", 10, &[1])),
        None,
        false
    )));
    assert!(already_recorded(transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("heavy", 11, &[2])),
        None,
        true
    )));
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (1, 1));
    assert_eq!(history_rows(&ns), 4);
    assert_eq!(store.load(&ns).unwrap().revision, revision);
}
/// No commit leaves an item key both live and in history, on any path:
/// neither settling an item the stored State still holds, nor storing a live
/// item whose key is already in history. Nothing is written by the refusal.
#[test]
fn history_that_is_also_live_is_refused_on_every_commit_path() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let cohort = synthetic_cohort("a", 10, &[1]);
        let mut state = history_state(vec![cohort.clone()]);
        state.runs.insert("a".into(), cohort.runs[0].clone());
        assert!(
            also_live(store.compare_and_swap(&ns, "w", 0, &state)),
            "{label}"
        );
        let mut state = history_state(vec![synthetic_cohort("r", 10, &[7])]);
        state.receipts.push(live_receipt(7));
        assert!(
            also_live(store.compare_and_swap(&ns, "w", 0, &state)),
            "{label}"
        );
        assert_eq!(history_rows(&ns), 0, "{label}");
        let revision = store
            .compare_and_swap(
                &ns,
                "w",
                0,
                &history_state(vec![synthetic_cohort("b", 10, &[2])]),
            )
            .unwrap();
        let mut state = State::default();
        state
            .runs
            .insert("b".into(), synthetic_cohort("b", 10, &[]).runs[0].clone());
        assert!(
            also_live(store.compare_and_swap(&ns, "w", revision, &state)),
            "{label}"
        );
        let mut state = State::default();
        state.receipts.push(live_receipt(2));
        assert!(
            also_live(store.compare_and_swap(&ns, "w", revision, &state)),
            "{label}"
        );
        assert_eq!(history_rows(&ns), 2, "{label}");
        assert_eq!(store.load(&ns).unwrap().revision, revision, "{label}");
    }
    for heavy in [false, true] {
        let (ns, store, revision) = transition_namespace(if heavy { "f3_heavy" } else { "f3_inc" });
        let c = synthetic_cohort("c", 10, &[]);
        assert!(
            also_live(transition_with(
                &store,
                &ns,
                Some(c.clone()),
                Some(c.runs[0].clone()),
                heavy
            )),
            "{heavy}"
        );
        assert_eq!(history_rows(&ns), 0);
        transition_with(
            &store,
            &ns,
            Some(synthetic_cohort("d", 10, &[])),
            None,
            heavy,
        )
        .unwrap();
        let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
        store.reset_transition_stats();
        assert!(
            also_live(transition_with(&store, &ns, None, Some(d), heavy)),
            "{heavy}"
        );
        let stats = store.transition_stats();
        assert_eq!((stats.incremental, stats.heavy), (0, 0));
        assert_eq!(history_rows(&ns), 1);
        assert_eq!(store.load(&ns).unwrap().revision, revision + 1);
    }
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
fn cohort_of(run: Run) -> SettledCohort {
    SettledCohort {
        root: run.id.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents: vec![],
        receipts: vec![],
    }
}
fn refused_with<T>(result: std::result::Result<T, StorageError>, text: &str) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == text)
}
fn removal_fences_hold(store: &PostgresStorage, ns: &str) {
    store.acquire(ns, "w").unwrap();
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
    let r = store.compare_and_swap(ns, "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL)
    );
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap(ns, "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL)
    );
    gone.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    let r = store.compare_and_swap(ns, "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK)
    );
}
#[test]
fn removal_fences_hold_on_postgres() {
    removal_fences_hold(&support::protected(), &support::namespace("fence_f2"));
    removal_fences_hold(&support::records(), &support::namespace("fence_f3"));
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
fn accepted_event(key: &str, accepted_at_ms: i64) -> AcceptedEvent {
    AcceptedEvent {
        key: key.into(),
        envelope: aq_playbooks_model::EventEnvelope {
            producer: "host".into(),
            event_id: key.into(),
            contract: aq_playbooks_model::ContractRef {
                name: "test.completed".into(),
                fingerprint: "fingerprint".into(),
            },
            scope: "project".into(),
            correlation: BTreeMap::new(),
            subject: None,
            revision: None,
            causation: None,
            occurred_at_ms: None,
            observed_at_ms: None,
            payload: Value::Null,
        },
        digest: format!("digest-{key}"),
        accepted_at_ms,
        sequence: 1,
        catalog_revision: 1,
        destinations: vec![],
    }
}
fn tombstone(digest: &str) -> retention::EventTombstone {
    retention::EventTombstone {
        producer: "host".into(),
        epoch: 0,
        digest: digest.into(),
        disposed_at_ms: 5,
    }
}
/// Policy windows: receipts 2_000, events 3_000; recorded expiries "k"
/// 3_000 and "ev" 4_000; "fallback" has none. Receipt 1 belongs to the
/// finished run-a, receipt 2 to the running run-b.
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
    // Round 2 (C1): an installed contract whose window (5_000) is longer
    // than the policy's, and an event of it expiring at 9_000.
    let long = long_artifact();
    s.artifacts.insert(long.digest.clone(), long);
    s.events.insert("ev-long".into(), {
        let mut e = accepted_event("ev-long", 0);
        e.envelope.contract = aq_playbooks_model::ContractRef {
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
fn long_contract() -> aq_playbooks_model::EventContract {
    aq_playbooks_model::EventContract {
        name: "test.long".into(),
        retention_ms: 5_000,
        ..pg_event_contract()
    }
}
fn long_artifact() -> aq_playbooks_model::Artifact {
    let mut content = support::artifact("long").content;
    let c = long_contract();
    content.registry.events.insert(c.name.clone(), c);
    aq_playbooks_model::Artifact::seal(content).unwrap()
}
type Case = (&'static str, &'static str, Box<dyn Fn(&mut State)>);
/// The refusals every commit path must make, each as a change of
/// `fence_state()`.
fn refusal_cases() -> Vec<Case> {
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    let mut cases: Vec<Case> = vec![
        (
            "run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                n.runs.remove("run-a");
            }),
        ),
        (
            "running settled run",
            "unresolved run obligations cannot be disposed",
            Box::new(|n| {
                let running = n.runs.remove("run-b").unwrap();
                n.receipts.retain(|r| r.sequence != 2);
                let mut c = cohort_of(running);
                c.receipts.push(audit(2, "run-b"));
                n.settled.push(c);
            }),
        ),
        (
            "keyed admission",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(2_999);
                n.keyed_admissions.remove("k");
                n.retention.admission_expires_at_ms.remove("k");
            }),
        ),
        (
            "keyed admission, policy window",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(1_999);
                n.keyed_admissions.remove("fallback");
            }),
        ),
        (
            "event",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(3_999);
                n.events.remove("ev");
                n.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "forged legacy tombstone",
            early,
            Box::new(|n| {
                n.events.remove("ev");
                n.retention.event_expires_at_ms.remove("ev");
                n.retention
                    .event_tombstones
                    .insert("ev".into(), tombstone("digest-ev"));
            }),
        ),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            Box::new(|n| {
                n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            }),
        ),
        (
            "receipt of a live run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                n.receipts.retain(|r| r.sequence != 2);
            }),
        ),
        (
            "receipt overwritten",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                *n.receipts.get_mut(1).unwrap() = audit(3, "run-b");
            }),
        ),
        (
            "receipt of a settled run without its history",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                let run = n.runs.remove("run-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled.push(cohort_of(run));
            }),
        ),
        (
            "lowered expiry",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
            }),
        ),
        (
            "expiry deleted, item kept",
            shortened,
            Box::new(|n| {
                n.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "new expiry below the fallback, stored item",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            }),
        ),
        (
            "new expiry below the fallback, new item",
            shortened,
            Box::new(|n| {
                n.keyed_admissions.insert("new".into(), receipt("new", 100));
                n.retention
                    .admission_expires_at_ms
                    .insert("new".into(), 2_099);
            }),
        ),
        (
            "clock",
            retention::REFUSE_CLOCK,
            Box::new(|n| {
                n.retention.clock_ms = Some(499);
            }),
        ),
    ];
    cases.extend(rebase_refusal_cases());
    cases
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
                n.receipts.retain(|r| r.sequence != 1);
                let mut c = cohort_of(run);
                c.receipts.push(audit(1, "run-a"));
                n.settled.push(c);
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
                let a = aq_playbooks_model::Artifact::seal(content).unwrap();
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
/// A licensed rebase commits on every full path (protected and plain
/// format 2, format 3 compare-and-swap) and through a format 3 transition
/// (on the complete path); after it the fences are closed again.
#[test]
fn a_clock_rebase_is_licensed_on_every_commit_path() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        for (case, text, f) in after_rebase_refusal_cases() {
            let ns = support::namespace(label);
            store.acquire(&ns, "w").unwrap();
            let base = store.compare_and_swap(&ns, "w", 0, &fence_state()).unwrap();
            let mut next = fence_state();
            rebased(&mut next);
            let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
            let s = store.load(&ns).unwrap().state;
            assert_eq!(
                (
                    s.retention.clock_ms,
                    s.retention.stale_floor_ms,
                    s.retention.admission_expires_at_ms.get("k").copied()
                ),
                (Some(100), Some(50), Some(2_100)),
                "{label}"
            );
            assert_eq!(
                s.retention.event_expires_at_ms.get("ev-long").copied(),
                Some(5_100),
                "{label}"
            );
            assert_eq!(history_rows(&ns), 1, "{label}: the rebase receipt");
            let mut later = s.clone();
            f(&mut later);
            let result = store.compare_and_swap(&ns, "w", r, &later);
            assert!(
                refused_with(result.clone(), text),
                "{label} {case}: {result:?}"
            );
            assert_eq!(store.load(&ns).unwrap().revision, r, "{label} {case}");
        }
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &fence_state()).unwrap();
        let mut next = fence_state();
        rebased(&mut next);
        let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut later = store.load(&ns).unwrap().state;
        floor_lowered(&mut later);
        store.compare_and_swap(&ns, "w", r, &later).unwrap();
        assert_eq!(
            store.load(&ns).unwrap().state.retention.stale_floor_ms,
            Some(40),
            "{label}"
        );
    }
    let store = support::records();
    let ns = support::namespace("f3_rebase");
    store.acquire(&ns, "w").unwrap();
    let mut s = fence_state();
    s.indexes.initialized = true;
    store.compare_and_swap(&ns, "w", 0, &s).unwrap();
    let transition = |f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        storage_error(store.transition_v1(
            &ns,
            "w",
            &mut TransitionRequest {
                operation: "rebase",
                plan: &mut plan,
            },
        ))
    };
    store.reset_transition_stats();
    transition(&rebased).unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    let r = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (case, text, f) in after_rebase_refusal_cases() {
        let result = transition(f.as_ref());
        assert!(refused_with(result.clone(), text), "{case}: {result:?}");
    }
    // The floor lowered with its own receipt, also on the complete path.
    transition(&floor_lowered).unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
    // A rebase that lowers only the clock also takes the complete path.
    let ns = support::namespace("f3_rebase_clock_only");
    store.acquire(&ns, "w").unwrap();
    store.compare_and_swap(&ns, "w", 0, &s).unwrap();
    let mut plan = |s: &mut State| {
        s.retention.clock_ms = Some(100);
        license(s, retention::CLOCK_REBASE, 500, 100);
        Ok(true)
    };
    storage_error(store.transition_v1(
        &ns,
        "w",
        &mut TransitionRequest {
            operation: "rebase",
            plan: &mut plan,
        },
    ))
    .unwrap();
    assert_eq!(store.load(&ns).unwrap().state.retention.clock_ms, Some(100));
}
fn tombstone_rows(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}
/// No removal without a history record or an expired window entry, and no
/// expiry moved earlier, on every full commit path. A refusal writes
/// nothing and writes no tombstone.
#[test]
fn removals_without_history_or_expiry_are_refused_on_every_full_commit_path() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        let s = fence_state();
        let fresh = || {
            let ns = support::namespace(label);
            store.acquire(&ns, "w").unwrap();
            let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
            (ns, base)
        };
        let mut failures = vec![];
        for (case, text, f) in refusal_cases() {
            let (ns, base) = fresh();
            let mut next = s.clone();
            f(&mut next);
            let result = store.compare_and_swap(&ns, "w", base, &next);
            if !refused_with(result.clone(), text) {
                failures.push(format!("{case}: {result:?}"));
            } else {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label} {case}");
                assert_eq!(
                    (history_rows(&ns), tombstone_rows(&ns)),
                    (0, 0),
                    "{label} {case}"
                );
            }
        }
        assert!(failures.is_empty(), "{label}: {failures:#?}");
        // Allowed: both receipts and the event expired with their records,
        // the run with its history and its receipt. No tombstone row.
        let (ns, base) = fresh();
        let mut next = s.clone();
        next.retention.clock_ms = Some(4_000);
        next.keyed_admissions.clear();
        next.retention.admission_expires_at_ms.remove("k");
        next.events.remove("ev");
        next.retention.event_expires_at_ms.remove("ev");
        let run = next.runs.remove("run-a").unwrap();
        next.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        next.settled.push(c);
        store.compare_and_swap(&ns, "w", base, &next).unwrap();
        assert_eq!(history_rows(&ns), 2, "{label}");
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
    }
}
/// Format 3 bounded transitions refuse the same commits, incrementally or
/// on the complete path; nothing is written and no commit is counted. An
/// expired window entry leaves incrementally; an event tombstone's row
/// leaves with it.
#[test]
fn format_3_transitions_hold_the_removal_fences() {
    let store = support::records();
    let mut s = fence_state();
    s.indexes.initialized = true;
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let fresh = || {
        let ns = support::namespace("f3_fence");
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let transition = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        storage_error(store.transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: "fence",
                plan: &mut plan,
            },
        ))
    };
    store.reset_transition_stats();
    let mut cases = refusal_cases();
    // Also when the commit is heavy for another reason.
    cases.push((
        "early removal, heavy",
        retention::REFUSE_EARLY_REMOVAL,
        Box::new(|n| {
            n.execution_denied.insert("x".into());
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        }),
    ));
    // Refused by the bounded commit itself, before any complete path.
    let bounded = [
        "lowered expiry",
        "new expiry below the fallback, new item",
        "changed receipt",
        "clock",
    ];
    let mut failures = vec![];
    for (case, text, f) in &cases {
        let (ns, base) = fresh();
        store.set_profiling(true);
        let result = transition(&ns, f.as_ref());
        let complete_path = store
            .profiling_snapshot()
            .unwrap()
            .operations
            .contains_key("cas");
        store.set_profiling(false);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{case}: {result:?}"));
        } else if bounded.contains(case) && complete_path {
            failures.push(format!("{case}: refused only on the complete path"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{case}");
            assert_eq!(history_rows(&ns), 0, "{case}");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (0, 0));
    let (ns, _) = fresh();
    // The in-State tombstone "old" has its row; removing it is bounded and
    // takes the row with it.
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(
        "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','3',true)",
        &[&ns],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,'event','old','d',5)",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(tombstone_rows(&ns), 1);
    transition(&ns, &|s| {
        s.retention.event_tombstones.remove("old");
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    assert_eq!(tombstone_rows(&ns), 0);
    // Expired: a receipt and an event leave in one bounded commit, and an
    // expiry moves later; no tombstone row is written.
    transition(&ns, &|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
        s.keyed_admissions
            .insert("new".into(), receipt("new", 4_000));
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 6_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 2);
    transition(&ns, &|s| {
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 7_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 3);
    let state = store.load(&ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
    assert_eq!(tombstone_rows(&ns), 0);
    // Without its expiry record in the patch, an expired receipt leaves
    // through the complete path.
    transition(&ns, &|s| {
        s.keyed_admissions.remove("fallback");
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    // The run leaves with its history and its receipt, through the complete
    // path.
    transition(&ns, &|s| {
        let run = s.runs.remove("run-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        s.settled.push(c);
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert!(!store.load(&ns).unwrap().state.runs.contains_key("run-a"));
    assert_eq!(history_rows(&ns), 2);
}
#[test]
fn full_commit_reads_only_created_identities_from_disposal_tombstones() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let base = store
            .compare_and_swap(&ns, "w", 0, &State::default())
            .unwrap();
        // 1,000 legacy event tombstone rows, written as the namespace writer.
        let mut c = support::client();
        let mut tx = c.transaction().unwrap();
        let format = if label == "f3" { "3" } else { "2" };
        tx.query_one(
            "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
            &[&ns, &format],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,'event','event-'||i,'d',1 FROM generate_series(1,1000) i",
            &[&ns],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,'run','run-old',NULL,1)",
            &[&ns],
        )
        .unwrap();
        tx.commit().unwrap();
        let mut next = State::default();
        next.runs.insert("run-new".into(), completed_run("run-new"));
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut reused = next.clone();
        reused
            .runs
            .insert("run-old".into(), completed_run("run-old"));
        assert!(
            refused_with(
                store.compare_and_swap(&ns, "w", revision, &reused),
                "disposed run identity cannot be reused"
            ),
            "{label}"
        );
        assert_eq!(store.load(&ns).unwrap().revision, revision, "{label}");
    }
}

// --- Task 5: a settled cohort leaves the record in the commit that settles it

fn count(sql: &str, ns: &str, id: &str) -> i64 {
    support::client()
        .query_one(sql, &[&ns, &id])
        .unwrap()
        .get(0)
}
#[test]
fn finished_run_leaves_protected_snapshot_and_projections_and_writes_no_tombstone() {
    for (format, store) in both() {
        let ns = support::namespace("leaves");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = support::artifact("a");
        let digest = a.digest.clone();
        e.install(a).unwrap();
        let authority = Authority {
            actor: "synthetic-test".into(),
            scope: "project".into(),
            capabilities: Default::default(),
        };
        e.activate_at("a", &digest, authority.clone(), 0).unwrap();
        let id = e.admit("a", "main", Value::Null, authority, 1).unwrap();
        // Live first: the projections hold the run.
        if format == "f2" {
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND run_id=$2",
                    &ns,
                    &id
                ),
                2,
                "{format}: run and admitted receipt projected while live"
            );
        }
        assert!(e.advance(&id, 2).unwrap());
        let state = e.store.load(&ns).unwrap().state;
        assert!(state.runs.is_empty(), "{format}");
        assert!(
            state.receipts.is_empty() && state.budgets.is_empty(),
            "{format}"
        );
        if format == "f2" {
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND (run_id=$2 OR item_key=$2)",
                    &ns,
                    &id
                ),
                0,
                "{format}: inspection_rows"
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND (item_key=$2 OR item_key LIKE '%' || $2)",
                    &ns,
                    &id
                ),
                0,
                "{format}: runtime_index"
            );
        }
        assert_eq!(
            count(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' AND item_key=$2",
                &ns,
                &id
            ),
            1,
            "{format}: run history row"
        );
        assert_eq!(tombstone_rows(&ns), 0, "{format}: disposal tombstones");
        let runs = e.inspect(&runs_query()).unwrap();
        assert_eq!(runs.items.len(), 1, "{format}");
        assert_eq!(runs.items[0]["status"], "completed", "{format}");
        assert!(
            matches!(e.advance(&id, 3), Err(Error::NotFound(_))),
            "{format}"
        );
    }
}

// --- Task 5 fix round 1 ---------------------------------------------------

fn pg_authority() -> Authority {
    Authority {
        actor: "synthetic-test".into(),
        scope: "project".into(),
        capabilities: Default::default(),
    }
}
fn pg_command() -> aq_playbooks_model::CommandContract {
    use aq_playbooks_model::*;
    CommandContract {
        name: "test.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: Default::default(),
        scopes: ["project".to_string()].into(),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: Default::default(),
        sensitive_input_fields: Default::default(),
        sensitive_output_fields: Default::default(),
        max_payload_bytes: 1024,
        deadline_ms: 100_000,
    }
}
/// `label`'s fixture with `steps` from `start`.
fn steps_artifact(
    label: &str,
    start: &str,
    steps: Vec<(&str, aq_playbooks_model::Step)>,
    dependencies: BTreeMap<String, aq_playbooks_model::CallableDependency>,
) -> aq_playbooks_model::Artifact {
    let mut content = support::artifact(label).content;
    content.playbook.entries.get_mut("main").unwrap().start = start.into();
    content.playbook.steps = steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let c = pg_command();
    content.registry.commands.insert(c.name.clone(), c);
    content.dependencies = dependencies;
    aq_playbooks_model::Artifact::seal(content).unwrap()
}
fn ret_ok() -> aq_playbooks_model::Step {
    support::artifact("a").content.playbook.steps["return"].clone()
}
/// invoke `test.command`, then return ok.
fn pg_invoke_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_command();
    steps_artifact(
        "invoke",
        "invoke",
        vec![
            (
                "invoke",
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
                    edges: BTreeMap::from([("ok".into(), "return".into())]),
                    on_error: Some("return".into()),
                },
            ),
            ("return", ret_ok()),
        ],
        BTreeMap::new(),
    )
}
fn ok_host() -> FakeHost {
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
/// C1 on PostgreSQL: a namespace stored by the pre-D10 engine (no
/// `root_intents`, no backfill flag) is backfilled when it is opened, on
/// protected format 2 and on format 3; the pre-upgrade run finishes with its
/// earlier operation in its history and a later run progresses.
#[test]
fn pre_d10_namespace_is_backfilled_on_open_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("pre_d10");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = pg_invoke_artifact();
        e.install(a.clone()).unwrap();
        e.activate_at("invoke", &a.digest, pg_authority(), 0)
            .unwrap();
        let mut host = ok_host();
        let old = e
            .admit("invoke", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&old, 0).unwrap();
        e.dispatch_next(&mut host, 0).unwrap();
        let v = e.store.load(&ns).unwrap();
        let intent = v.state.intents.values().next().unwrap().clone();
        assert_eq!(intent.status, IntentStatus::Completed, "{format}");
        // The State as the vendored engine stored it.
        let mut value = serde_json::to_value(&v.state).unwrap();
        let indexes = value["indexes"].as_object_mut().unwrap();
        indexes.remove("root_intents");
        indexes.remove("root_intents_complete");
        let legacy: State = serde_json::from_value(value).unwrap();
        assert!(legacy.indexes.root_intents.is_empty() && !legacy.indexes.root_intents_complete);
        e.store
            .compare_and_swap(&ns, "w", v.revision, &legacy)
            .unwrap();
        let store = e.store;
        store.release(&ns, "w").unwrap();
        let e = Engine::new(store, &ns, "w2").unwrap();
        let s = e.store.load(&ns).unwrap().state;
        assert!(s.indexes.root_intents_complete, "{format}");
        assert!(
            s.indexes.root_intents[&old].contains(&intent.id),
            "{format}"
        );
        let new = e
            .admit("invoke", "main", Value::Null, pg_authority(), 1)
            .unwrap();
        for _ in 0..10 {
            e.drive(&mut host, 1, 20).unwrap();
        }
        let s = e.store.load(&ns).unwrap().state;
        assert!(s.runs.is_empty() && s.intents.is_empty(), "{format}");
        let effects = e
            .inspect(&InspectionQuery {
                kind: InspectionKind::Effects,
                run: Some(old.clone()),
                after: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(effects.items.len(), 1, "{format}");
        assert_eq!(effects.items[0]["id"], intent.id.as_str(), "{format}");
        for id in [&old, &new] {
            let runs = e
                .inspect(&InspectionQuery {
                    kind: InspectionKind::Runs,
                    run: Some(id.clone()),
                    after: None,
                    limit: 10,
                })
                .unwrap();
            assert_eq!(runs.items[0]["outcome"], "ok", "{format} {id}");
        }
    }
}
/// M4 on PostgreSQL: a finished root's inspection items are byte-for-byte
/// the same while its cohort waits for an independent child and after the
/// cohort has left as `run_history` rows (format 2 pages through jsonb).
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal_on_postgres() {
    use aq_playbooks_model::*;
    for (format, store) in both() {
        let ns = support::namespace("same_items");
        let e = Engine::new(store, &ns, "w").unwrap();
        let child = pg_invoke_artifact();
        let parent = steps_artifact(
            "parent",
            "call",
            vec![
                (
                    "call",
                    Step {
                        op: Operation::Call {
                            dependency: "child".into(),
                            entry: "main".into(),
                            input: Expr::literal(Value::Null),
                            mode: CallMode::Independent,
                            bind: "handle".into(),
                        },
                        edges: BTreeMap::from([("started".into(), "return".into())]),
                        on_error: Some("return".into()),
                    },
                ),
                ("return", ret_ok()),
            ],
            BTreeMap::from([(
                "child".into(),
                CallableDependency {
                    selection: DependencySelection::Exact,
                    binding: "child".into(),
                    artifact: child.digest.clone(),
                    interfaces: child.content.playbook.entries.clone(),
                    capabilities: Default::default(),
                    scope: "project".into(),
                    dependencies: Default::default(),
                },
            )]),
        );
        e.install(child).unwrap();
        e.install(parent.clone()).unwrap();
        e.activate_at("parent", &parent.digest, pg_authority(), 0)
            .unwrap();
        let root = e
            .admit("parent", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&root, 0).unwrap();
        e.advance(&root, 0).unwrap();
        let s = e.store.load(&ns).unwrap().state;
        assert!(
            s.runs[&root].status.terminal() && s.runs.len() == 2,
            "{format}"
        );
        let pages = |e: &Engine<PostgresStorage>| -> Vec<Vec<u8>> {
            [
                InspectionKind::Runs,
                InspectionKind::Effects,
                InspectionKind::Receipts,
            ]
            .into_iter()
            .map(|kind| {
                let page = e
                    .inspect(&InspectionQuery {
                        kind,
                        run: Some(root.clone()),
                        after: None,
                        limit: 1000,
                    })
                    .unwrap();
                serde_json::to_vec(&page.items).unwrap()
            })
            .collect()
        };
        let live = pages(&e);
        e.drive(&mut ok_host(), 1, 20).unwrap();
        assert!(e.store.load(&ns).unwrap().state.runs.is_empty(), "{format}");
        assert!(history_rows(&ns) > 0, "{format}");
        assert_eq!(pages(&e), live, "{format}");
    }
}

// --- Task 6: window entries dropped 64 per commit --------------------------

fn pg_keyed(e: &Engine<PostgresStorage>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: pg_authority(),
    }
}
fn pg_event_contract() -> aq_playbooks_model::EventContract {
    use aq_playbooks_model::*;
    EventContract {
        sensitive_payload_fields: Default::default(),
        name: "test.signal".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: ["host".to_string()].into(),
        scopes: ["project".to_string()].into(),
        correlation_fields: ["invocation".to_string()].into(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1_000,
    }
}
fn pg_event(id: &str) -> aq_playbooks_model::EventEnvelope {
    let c = pg_event_contract();
    aq_playbooks_model::EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: aq_playbooks_model::ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), id.into())]),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: Value::Null,
    }
}
/// The 64-drop script on f2 protected and f3: 65 finished keyed runs with a
/// 1 s window and one event with a 1 s window, then one admission past them.
/// The dropping commit passes the removal fence on f2 and stays on the
/// bounded path on f3; an expired key admitted again leaves first and is
/// new; no tombstone row is written.
#[test]
fn window_drop_on_postgres_is_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("window");
        let e = Engine::new(store, &ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.admit_event("host", pg_event("e1"), 0).unwrap();
        for i in 0..65 {
            let d = e.admit_once(pg_keyed(&e, &format!("k-{i:03}")), i).unwrap();
            e.advance(&d.receipt.run_id, i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.keyed_admissions.len(), s.events.len()),
            (65, 1),
            "{format}"
        );
        e.store.reset_transition_stats();
        e.admit_once(pg_keyed(&e, "k-new"), 2_000).unwrap();
        let s = e.snapshot().unwrap();
        // The event (1_000) and k-000..k-062 (1_000..1_062): 64, oldest first.
        assert!(s.events.is_empty(), "{format}");
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec![
                "k-063".to_string(),
                "k-064".to_string(),
                "k-new".to_string()
            ],
            "{format}"
        );
        assert!(
            s.retention.event_epochs.is_empty()
                && s.retention.event_expires_at_ms.is_empty()
                && s.indexes.events.is_empty()
                && s.indexes.event_memberships.is_empty(),
            "{format}"
        );
        assert_eq!(
            s.retention
                .expiry
                .keys()
                .map(|k| retention::window_entry(k).unwrap().0)
                .collect::<Vec<_>>(),
            vec![1_063, 1_064, 3_000],
            "{format}"
        );
        assert!(
            s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty(),
            "{format}"
        );
        let stats = e.store.transition_stats();
        if format == "f3" {
            assert_eq!((stats.incremental, stats.heavy), (1, 0), "{format}");
        }
        // An expired key admitted again: its receipt leaves in a commit of
        // its own, then the key is new (and the drop takes k-064's sibling).
        let again = e.admit_once(pg_keyed(&e, "k-064"), 2_001).unwrap();
        assert!(!again.duplicate, "{format}");
        let s = e.snapshot().unwrap();
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec!["k-064".to_string(), "k-new".to_string()],
            "{format}"
        );
        assert_eq!(s.keyed_admissions["k-064"], again.receipt, "{format}");
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (3, 0), "{format}");
        }
        // A redelivered event after its window is new.
        assert!(
            !e.admit_event("host", pg_event("e1"), 2_002)
                .unwrap()
                .duplicate
        );
        assert_eq!(e.snapshot().unwrap().events.len(), 1, "{format}");
        assert_eq!(tombstone_rows(&ns), 0, "{format}");
        if format == "f3" {
            // A burst in one millisecond: one small record per entry, and
            // each admission stays on the bounded path.
            e.store.reset_transition_stats();
            for i in 0..20 {
                e.admit_once(pg_keyed(&e, &format!("burst-{i:02}")), 2_003)
                    .unwrap();
            }
            assert_eq!(e.store.transition_stats().heavy, 0);
            let row = support::client()
                .query_one(
                    "SELECT count(*), max(octet_length(value)) FROM aq_playbooks.state_records WHERE namespace=$1 AND field='retention.expiry'",
                    &[&ns],
                )
                .unwrap();
            let (records, largest): (i64, i32) = (row.get(0), row.get(1));
            assert_eq!(records, 23, "k-064, k-new, the event and 20 burst entries");
            assert!(largest <= 8, "{largest} bytes");
        }
    }
}

/// A waiter on `test.signal` with correlation `invocation = "held"`: it
/// holds the event `pg_event("held")` while it waits (it is never driven
/// here, so its deadline never fires).
fn pg_waiter_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_event_contract();
    let mut content = support::artifact("waiter").content;
    content.playbook.entries.get_mut("main").unwrap().start = "wait".into();
    content.playbook.steps = BTreeMap::from([
        (
            "wait".to_string(),
            Step {
                op: Operation::Wait {
                    event: Some(ContractRef {
                        name: c.name.clone(),
                        fingerprint: c.fingerprint().unwrap(),
                    }),
                    correlation: BTreeMap::from([("invocation".into(), Expr::literal("held"))]),
                    minimum_revision: None,
                    deadline_ms: 100,
                    bind: "completion".into(),
                },
                edges: BTreeMap::from([
                    ("received".into(), "return".into()),
                    ("timeout".into(), "return".into()),
                ]),
                on_error: Some("return".into()),
            },
        ),
        ("return".to_string(), ret_ok()),
    ]);
    content.registry.events.insert(c.name.clone(), c);
    Artifact::seal(content).unwrap()
}
/// Ruling 3, the full scenario on PostgreSQL protected format 2 and format
/// 3: normal operation, one far-future host time, stale refusals that write
/// nothing and entries that do not expire, the rebase, then normal
/// operation with held events, live runs and duplicates intact and the
/// record no longer growing.
#[test]
fn a_pinned_clock_is_rebased_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rebase");
        let e = Engine::new(store, &ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        let w = pg_waiter_artifact();
        let waiter_digest = w.digest.clone();
        e.install(w).unwrap();
        e.activate_at("waiter", &waiter_digest, pg_authority(), 0)
            .unwrap();
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let t0: i64 = 10_000;
        let pinned = t0 + 1_000_000_000;
        let done = e.admit_once_at(pg_keyed(&e, "done"), Some(t0), t0).unwrap();
        e.advance(&done.receipt.run_id, t0).unwrap();
        let live = e.admit_once_at(pg_keyed(&e, "live"), Some(t0), t0).unwrap();
        let waiter = e
            .admit("waiter", "main", Value::Null, pg_authority(), t0)
            .unwrap();
        e.advance(&waiter, t0).unwrap();
        let held = e
            .admit_event_at("host", pg_event("held"), 0, Some(t0), t0)
            .unwrap();
        e.admit("a", "main", Value::Null, pg_authority(), pinned)
            .unwrap();
        let s = e.snapshot().unwrap();
        assert!(!s.keyed_admissions.contains_key("done"), "{format}");
        assert!(s.events.contains_key(&held.key), "{format}");
        // Real time again: stale, and nothing written (M5).
        let t1 = t0 + 200;
        let revision = e.store.load(&ns).unwrap().revision;
        assert!(matches!(
            e.admit_once_at(pg_keyed(&e, "during"), Some(t1), t1),
            Err(Error::Stale { .. })
        ));
        assert!(matches!(
            e.admit_event_at("host", pg_event("during-e"), 0, Some(t1), t1),
            Err(Error::Stale { .. })
        ));
        assert_eq!(e.store.load(&ns).unwrap().revision, revision, "{format}");
        for i in 0..3 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("pin-{i}")), t1 + i)
                .unwrap();
            e.advance(&d.receipt.run_id, t1 + i).unwrap();
        }
        e.admit("a", "main", Value::Null, pg_authority(), t1 + 5_000)
            .unwrap();
        assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 4, "{format}");
        // The rebase.
        let t2 = t0 + 500;
        let report = e
            .rebase_retention_clock(t2, t0, "ops", "pinned clock", t2)
            .unwrap();
        assert_eq!(
            (
                report.from_ms,
                report.capped_admissions,
                report.capped_events
            ),
            (pinned, 4, 1),
            "{format}"
        );
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.retention.clock_ms, s.retention.stale_floor_ms),
            (Some(t2), Some(t0)),
            "{format}"
        );
        assert_eq!(
            s.retention.event_expires_at_ms.get(&held.key).copied(),
            Some(t2 + 1_000),
            "{format}"
        );
        let rows: i64 = support::client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='receipts' AND public_value->>'kind'='retention_clock_rebased'",
                &[&ns],
            )
            .unwrap()
            .get(0);
        assert_eq!(rows, 1, "{format}: the rebase receipt is history");
        // Normal operation resumes.
        let during = e
            .admit_once_at(pg_keyed(&e, "during"), Some(t1), t2)
            .unwrap();
        assert!(!during.duplicate, "{format}");
        assert!(
            e.admit_once_at(pg_keyed(&e, "during"), Some(t1), t2 + 10)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_once_at(pg_keyed(&e, "live"), Some(t0), t2 + 10)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_event_at("host", pg_event("held"), 0, Some(t0), t2 + 10)
                .unwrap()
                .duplicate
        );
        let s = e.snapshot().unwrap();
        assert!(s.runs.contains_key(&waiter), "{format}");
        assert!(s.runs.contains_key(&live.receipt.run_id), "{format}");
        // The floor refuses a retry of the early-forgotten key.
        assert!(matches!(
            e.admit_once_at(pg_keyed(&e, "done"), Some(t0), t2 + 10),
            Err(Error::Stale { floor_ms: Some(f), .. }) if f == t0
        ));
        // The record stops growing.
        for run in [&live.receipt.run_id, &during.receipt.run_id] {
            e.advance(run, t2 + 20).unwrap();
        }
        for i in 0..10 {
            let at = t2 + 1_001 + i * 200;
            let d = e
                .admit_once_at(pg_keyed(&e, &format!("after-{i}")), Some(at), at)
                .unwrap();
            e.advance(&d.receipt.run_id, at).unwrap();
        }
        let s = e.snapshot().unwrap();
        let mut keys: Vec<String> = s.keyed_admissions.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            (5..10).map(|i| format!("after-{i}")).collect::<Vec<_>>(),
            "{format}"
        );
    }
}

// --- Task 8: max_tombstones is ignored ------------------------------------

/// Legacy disposal tombstone rows beyond `max_tombstones` refuse no full
/// commit and are not counted. Rows of runs are left to the drain, never to
/// the GC clock's cutoff deletes: a handle that has seen the namespace
/// drained does not look again (here they were inserted behind its back),
/// and a new handle deletes them `drain_entries_per_commit` per commit. On
/// f2 protected, f2 plain and f3.
#[test]
fn legacy_tombstone_rows_beyond_max_tombstones_refuse_no_commit_and_are_not_scanned() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = State::default();
        state.retention.policy.max_tombstones = 1;
        state.retention.policy.audit_ms = 1;
        state.retention.last_gc_at_ms = Some(10);
        let base = store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        // 1,000 legacy run tombstone rows (no inspection row: the cutoff
        // delete would take them), written as the namespace writer.
        let mut c = support::client();
        let mut tx = c.transaction().unwrap();
        let format = if label == "f3" { "3" } else { "2" };
        tx.query_one(
            "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
            &[&ns, &format],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,'run','legacy-'||i,NULL,1 FROM generate_series(1,1000) i",
            &[&ns],
        )
        .unwrap();
        tx.commit().unwrap();
        let mut next = state.clone();
        next.runs.insert("run-new".into(), completed_run("run-new"));
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 1_000, "{label}");
        next.retention.last_gc_at_ms = Some(20);
        let revision = store.compare_and_swap(&ns, "w", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 1_000, "{label}");
        store.release(&ns, "w").unwrap();
        let store = match label {
            "f2" => support::protected(),
            "f2_plain" => support::store(),
            _ => support::records(),
        };
        store.acquire(&ns, "w2").unwrap();
        next.sequence += 1;
        let revision = store.compare_and_swap(&ns, "w2", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 500, "{label}");
        next.sequence += 1;
        store.compare_and_swap(&ns, "w2", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
    }
}
// --- Task 8b (owner override): no namespace limits -----------------------

/// On f2 protected and f3, with every deprecated limit at its smallest:
/// direct, keyed and `invoke` admissions land, the runs drive to completion
/// and leave, and a clock rebase lands. Nothing is refused for capacity.
#[test]
fn nothing_is_refused_for_namespace_capacity_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("unlimited");
        let e = Engine::with_config(
            store,
            &ns,
            "w",
            EngineConfig {
                max_runs: 1,
                max_events: 1,
                max_queued_destinations: 1,
                max_state_bytes: 1,
                max_receipts: 1,
            },
        )
        .unwrap();
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        let invoke = pg_invoke_artifact();
        e.install(invoke.clone()).unwrap();
        e.activate_at("invoke", &invoke.digest, pg_authority(), 0)
            .unwrap();
        for i in 0..20 {
            e.admit_once(pg_keyed(&e, &format!("w{i}")), i).unwrap();
        }
        for t in 0..3 {
            e.admit("invoke", "main", Value::Null, pg_authority(), 200 + t)
                .unwrap();
        }
        let mut host = ok_host();
        host.responses
            .get_mut("test.command")
            .unwrap()
            .push(DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            });
        e.drive(&mut host, 310, 200)
            .unwrap_or_else(|err| panic!("{format}: {err}"));
        assert!(e.snapshot().unwrap().runs.is_empty(), "{format}");
        // Pin the clock a year ahead, then rebase it.
        let now: i64 = 1_763_000_000_000;
        let pinned = now + 31_536_000_000;
        for i in 0..3 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("p{i}")), pinned)
                .unwrap();
            assert!(e.advance(&d.receipt.run_id, pinned).unwrap(), "{format}");
        }
        e.rebase_retention_clock(now, now, "operator", "clock pinned a year ahead", now)
            .unwrap_or_else(|err| panic!("{format}: {err}"));
        assert_eq!(
            e.snapshot().unwrap().retention.clock_ms,
            Some(now),
            "{format}"
        );
    }
}

// --- Task 8b R1: the eviction licence on every PostgreSQL commit path -----

const EVICTION_WINDOW: i64 = 100_000;
fn running_run(id: &str) -> Run {
    let mut run = completed_run(id);
    run.status = RunStatus::Running;
    run.outcome = None;
    run.terminal_sequence = None;
    run
}
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
    s.indexes.waiters.insert(
        corr,
        std::collections::BTreeSet::from(["run-w".to_string()]),
    );
}
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
fn hold_out(n: &mut State, key: &str) {
    let r = n.keyed_admissions.get(key).unwrap().clone();
    n.retention
        .replay_order
        .remove(&retention::admission_order_key(&r));
    n.retention.replay_held.insert(r.run_id, key.into());
}
fn eviction_base(keep: usize) -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = keep;
    s.indexes.initialized = true;
    s.sequence = 100;
    s
}
/// As the runner's: `replay_keep` 5, order kh (held), k0..k3, e4, ew (held
/// by a waiter); kh2 waits in `replay_held`; ex is outside the order.
fn eviction_state() -> State {
    let mut s = eviction_base(5);
    keyed_entry(&mut s, "kh", 0, true, true);
    for i in 0..4 {
        keyed_entry(&mut s, &format!("k{i}"), i + 1, false, true);
    }
    event_entry(&mut s, "e4", 5, true);
    event_entry(&mut s, "ew", 7, true);
    hold_by_waiter(&mut s, "ew");
    keyed_entry(&mut s, "kh2", 9, true, false);
    event_entry(&mut s, "ex", 8, false);
    s
}
fn evicted_correctly(n: &mut State) {
    hold_out(n, "kh");
    evict_keyed(n, "k0");
}
fn bulk_eviction_state() -> State {
    let mut s = eviction_base(1);
    for i in 0..70 {
        keyed_entry(&mut s, &format!("b{i:03}"), i + 1, false, true);
    }
    s
}
fn evict_bulk(n: &mut State, count: usize) {
    for i in 0..count {
        evict_keyed(n, &format!("b{i:03}"));
    }
}
type EvictionCase = (&'static str, &'static str, State, Box<dyn Fn(&mut State)>);
fn eviction_refusal_cases() -> Vec<EvictionCase> {
    let licence = retention::REFUSE_EVICTION;
    let early = retention::REFUSE_EARLY_REMOVAL;
    let base = eviction_state();
    let mut held_event = eviction_base(4);
    event_entry(&mut held_event, "ew", 0, true);
    hold_by_waiter(&mut held_event, "ew");
    for i in 0..5 {
        keyed_entry(&mut held_event, &format!("k{i}"), i + 1, false, true);
    }
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
            held_event.clone(),
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
            "not the oldest",
            licence,
            base.clone(),
            Box::new(|n| evict_keyed(n, "k1")),
        ),
        (
            "an entry outside the order, not the oldest",
            early,
            base.clone(),
            Box::new(|n| evict_event(n, "ex")),
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
                    .insert(retention::replay_key(31, "k:ghost"), ());
                hold_out(n, "kh");
                evict_keyed(n, "k0");
                evict_keyed(n, "k1");
            }),
        ),
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
            "an event's envelope changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().envelope.payload = json!(1)),
        ),
        (
            "a held event whose waiter index is dropped",
            licence,
            held_event.clone(),
            Box::new(|n| {
                n.indexes.waiters.clear();
                evict_event(n, "ew");
            }),
        ),
        (
            "more than 64 in one commit",
            licence,
            bulk_eviction_state(),
            Box::new(|n| evict_bulk(n, 65)),
        ),
    ]
}
/// Store `state` as a namespace reaches it: entries join unheld, the holds
/// begin in a second commit.
fn seed_eviction_state(store: &PostgresStorage, ns: &str, state: &State) -> u64 {
    store.acquire(ns, "w").unwrap();
    let mut unheld = state.clone();
    unheld.runs.clear();
    unheld.waits.clear();
    unheld.indexes.waiters.clear();
    let first = store.compare_and_swap(ns, "w", 0, &unheld).unwrap();
    store.compare_and_swap(ns, "w", first, state).unwrap()
}
type AllowedEviction = (&'static str, State, Box<dyn Fn(&mut State)>);
fn allowed_evictions() -> Vec<AllowedEviction> {
    vec![
        (
            "the correct eviction",
            eviction_state(),
            Box::new(evicted_correctly),
        ),
        (
            "64 at once",
            bulk_eviction_state(),
            Box::new(|n| evict_bulk(n, 64)),
        ),
    ]
}
/// The eviction licence on f2 protected, f2 plain and f3 compare-and-swap.
#[test]
fn the_eviction_licence_holds_on_every_full_commit_path() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let mut failures = vec![];
        for (case, text, previous, f) in eviction_refusal_cases() {
            let ns = support::namespace(label);
            let base = seed_eviction_state(&store, &ns, &previous);
            let mut next = previous.clone();
            f(&mut next);
            let result = store.compare_and_swap(&ns, "w", base, &next);
            if !refused_with(result.clone(), text) {
                failures.push(format!("{label} {case}: {result:?}"));
            } else {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label} {case}");
            }
        }
        for (case, previous, f) in allowed_evictions() {
            let ns = support::namespace(label);
            let base = seed_eviction_state(&store, &ns, &previous);
            let mut next = previous.clone();
            f(&mut next);
            if let Err(err) = store.compare_and_swap(&ns, "w", base, &next) {
                failures.push(format!("{label} {case} refused: {err}"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
/// On f3 bounded transitions: each refusal is made by the bounded commit
/// itself (no complete path) and writes nothing; a correct eviction, and 64
/// at once, commit incrementally.
#[test]
fn format_3_evictions_are_incremental_and_fenced() {
    let store = support::records();
    let transition = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        storage_error(store.transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: "evict",
                plan: &mut plan,
            },
        ))
    };
    let mut failures = vec![];
    for (case, text, previous, f) in eviction_refusal_cases() {
        let ns = support::namespace("f3_evict");
        let base = seed_eviction_state(&store, &ns, &previous);
        store.set_profiling(true);
        let result = transition(&ns, f.as_ref());
        let complete_path = store
            .profiling_snapshot()
            .unwrap()
            .operations
            .contains_key("cas");
        store.set_profiling(false);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{case}: {result:?}"));
        } else if complete_path {
            failures.push(format!("{case}: refused only on the complete path"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{case}");
        }
    }
    store.reset_transition_stats();
    for (case, previous, f) in allowed_evictions() {
        let ns = support::namespace("f3_evict");
        seed_eviction_state(&store, &ns, &previous);
        if let Err(err) = transition(&ns, f.as_ref()) {
            failures.push(format!("{case} refused: {err}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (2, 0));
}
/// The engine on PostgreSQL with `replay_keep` 3: events past it evict the
/// oldest, on f3 incrementally (no heavy commit), on f2 protected through
/// the complete fence; an evicted event delivered again is new, a kept one
/// a duplicate. Finished keyed runs then evict the oldest too.
#[test]
fn evictions_on_postgres_are_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("evict");
        let e = Engine::new(store, &ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 3,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.store.reset_transition_stats();
        for i in 0..6 {
            e.admit_event("host", pg_event(&format!("e{i}")), i)
                .unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(s.events.len(), 3, "{format}");
        assert_eq!(s.retention.replay_order.len(), 3, "{format}");
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (6, 0), "{format}");
        }
        assert!(
            e.admit_event("host", pg_event("e5"), 10).unwrap().duplicate,
            "{format}"
        );
        assert!(
            !e.admit_event("host", pg_event("e0"), 11).unwrap().duplicate,
            "{format}"
        );
        for i in 0..4 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("k{i}")), 20 + i)
                .unwrap();
            e.advance(&d.receipt.run_id, 20 + i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.replay_order.len(), 3, "{format}");
        assert_eq!(s.keyed_admissions.len() + s.events.len(), 3, "{format}");
        assert!(s.events.is_empty(), "{format}");
        assert!(!s.keyed_admissions.contains_key("k0"), "{format}");
    }
}

// --- Task 8b R2: history is a rolling log on PostgreSQL --------------------

fn history_count(ns: &str) -> Option<i64> {
    support::client()
        .query_opt(
            "SELECT runs FROM aq_playbooks.run_history_counts WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .map(|row| row.get(0))
}
fn history_run_ids(ns: &str) -> Vec<String> {
    support::client()
        .query(
            "SELECT item_key FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' ORDER BY finished_at_ms,item_key",
            &[&ns],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}
/// On f2 protected and f3: `history_keep` 5 keeps the newest 5 of 20
/// finished runs, holds only those runs' items (and admin receipts), and
/// counts them; deleting history by time keeps the count, so the trim after
/// it keeps the newest 5 again; `history_keep` 0 writes no row.
#[test]
fn history_is_a_rolling_log_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rolling");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            history_keep: 5,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let admit = |t: i64| {
            let id = e
                .admit("a", "main", Value::Null, pg_authority(), t)
                .unwrap();
            assert!(e.advance(&id, t).unwrap(), "{format}");
            id
        };
        let runs: Vec<String> = (0..20).map(admit).collect();
        assert_eq!(history_run_ids(&ns), runs[15..], "{format}");
        assert_eq!(history_count(&ns), Some(5), "{format}");
        let foreign: i64 = support::client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND run_id<>ALL($2) AND run_id<>'admin'",
                &[&ns, &runs[15..].to_vec()],
            )
            .unwrap()
            .get(0);
        assert_eq!(foreign, 0, "{format}");
        let listed: Vec<Value> = e
            .inspect(&runs_query())
            .unwrap()
            .items
            .iter()
            .map(|r| r["id"].clone())
            .collect();
        assert_eq!(listed.len(), 5, "{format}");
        // Delete by time, then the trim counts what is left.
        assert!(e.delete_history_before(17).unwrap() > 0, "{format}");
        assert_eq!(history_run_ids(&ns), runs[17..], "{format}");
        assert_eq!(history_count(&ns), Some(3), "{format}");
        let more: Vec<String> = (20..24).map(admit).collect();
        let mut expected = runs[19..].to_vec();
        expected.extend(more);
        assert_eq!(history_run_ids(&ns), expected, "{format}");
        assert_eq!(history_count(&ns), Some(5), "{format}");
        // `history_keep` 0: nothing is written; what is there is trimmed.
        e.set_retention_policy(RetentionPolicy {
            history_keep: 0,
            ..RetentionPolicy::default()
        })
        .unwrap();
        admit(30);
        assert!(history_run_ids(&ns).is_empty(), "{format}");
        assert_eq!(history_count(&ns), Some(0), "{format}");
    }
}
/// One commit settling 100 cohorts, on f2 protected, f2 plain and f3: the
/// trim keeps up, history holds the newest 5.
#[test]
fn the_trim_keeps_up_with_one_commit_on_postgres() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = State::default();
        state.retention.policy.history_keep = 5;
        state.settled = (0..100)
            .map(|i| synthetic_cohort(&format!("c{i:03}"), i, &[i as u64 + 1]))
            .collect();
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        assert_eq!(
            history_run_ids(&ns),
            ["c095", "c096", "c097", "c098", "c099"],
            "{label}"
        );
        assert_eq!(history_rows(&ns), 10, "{label}");
        assert_eq!(history_count(&ns), Some(5), "{label}");
    }
}
/// Review I2 and I3 on f2 protected and f3: the trim keeps the newest runs
/// by sequence after a host clock that ran ahead, and receipts owned by no
/// run (blocked event deliveries, admin) are rolling sets of `history_keep`.
#[test]
fn history_trims_by_sequence_and_bounds_receipts_of_no_run_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rolling_all");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        // A trigger whose input is refused at acceptance: every event writes
        // a `destination_blocked` receipt.
        let mut trigger = support::artifact("trigger").content;
        trigger
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        trigger.playbook.triggers.push(aq_playbooks_model::Trigger {
            id: "on-signal".into(),
            event: aq_playbooks_model::ContractRef {
                name: "test.signal".into(),
                fingerprint: pg_event_contract().fingerprint().unwrap(),
            },
            entry: "main".into(),
            input: aq_playbooks_model::Expr::literal(Value::Null),
        });
        trigger.playbook.limits.max_payload_bytes = 1;
        let trigger = aq_playbooks_model::Artifact::seal(trigger).unwrap();
        e.install(trigger.clone()).unwrap();
        e.activate_at("trigger", &trigger.digest, pg_authority(), 0)
            .unwrap();
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 5,
            history_keep: 3,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let run = |t: i64| {
            let id = e
                .admit("a", "main", Value::Null, pg_authority(), t)
                .unwrap();
            assert!(e.advance(&id, t).unwrap(), "{format}");
            id
        };
        let pinned: Vec<String> = (0..3).map(|_| run(9_000_000_000_000)).collect();
        let later: Vec<String> = (0..2).map(|_| run(1_000)).collect();
        assert_eq!(
            history_run_ids_by_sequence(&ns),
            [pinned[2].clone(), later[0].clone(), later[1].clone()],
            "{format}"
        );
        for i in 0..40 {
            e.admit_event("host", pg_event(&format!("b{i:03}")), 2_000 + i)
                .unwrap();
        }
        for i in 0..10 {
            e.set_activation_controls("trigger", ActivationControls::default(), 3_000 + i)
                .unwrap();
        }
        let count = |pattern: &str| -> i64 {
            support::client()
                .query_one(
                    "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='receipts' AND run_id LIKE $2",
                    &[&ns, &pattern],
                )
                .unwrap()
                .get(0)
        };
        assert!(
            (1..=3).contains(&count("event:%")),
            "{format} {}",
            count("event:%")
        );
        assert!(
            (1..=3).contains(&count("admin")),
            "{format} {}",
            count("admin")
        );
        // The counts are those of the rows, also after a delete by time.
        let counted = |ns: &str| -> (i64, i64, i64) {
            let row = support::client()
                .query_one(
                    "SELECT runs,event_receipts,admin_receipts FROM aq_playbooks.run_history_counts WHERE namespace=$1",
                    &[&ns],
                )
                .unwrap();
            (row.get(0), row.get(1), row.get(2))
        };
        let actual = |ns: &str| -> (i64, i64, i64) {
            (
                history_run_ids_by_sequence(ns).len() as i64,
                count("event:%"),
                count("admin"),
            )
        };
        assert_eq!(counted(&ns), actual(&ns), "{format}");
        assert!(e.delete_history_before(2_020).unwrap() > 0, "{format}");
        assert_eq!(counted(&ns), actual(&ns), "{format}");
        assert!(e.delete_history_before(10_000).unwrap() > 0, "{format}");
        // The run finished at the pinned time (later than 10,000) stays.
        assert_eq!(actual(&ns), (1, 0, 0), "{format}");
        assert_eq!(counted(&ns), actual(&ns), "{format}");
    }
}
fn history_run_ids_by_sequence(ns: &str) -> Vec<String> {
    support::client()
        .query(
            "SELECT item_key FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' ORDER BY sequence,item_key",
            &[&ns],
        )
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

// --- Task 9: the drain of namespaces written by an older engine -------------

/// Write `rows` legacy tombstone rows of `kind` as the namespace writer.
fn legacy_rows(ns: &str, format: &str, kind: &str, rows: i64) {
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(
        "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
        &[&ns, &format],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,$2,'legacy-'||$2||'-'||i,NULL,1 FROM generate_series(1,$3::bigint) i",
        &[&ns, &kind, &rows],
    )
    .unwrap();
    tx.commit().unwrap();
}
fn format_of(label: &str) -> &'static str {
    if label == "f3" { "3" } else { "2" }
}
/// A new storage handle of the kind `label` names (as a restarted process
/// opens the namespace).
fn handle(label: &str) -> PostgresStorage {
    if label == "f3" {
        support::records()
    } else {
        support::protected()
    }
}
/// `state` without its drain record, as an older engine stored it.
fn without_drain(state: &State) -> State {
    let mut value = serde_json::to_value(state).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    serde_json::from_value(value).unwrap()
}
fn legacy_runs_left(store: &PostgresStorage, ns: &str) -> usize {
    store
        .load(ns)
        .unwrap()
        .state
        .runs
        .keys()
        .filter(|id| id.starts_with("run-0-legacy-"))
        .count()
}
fn history_runs(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs'",
            &[&ns],
        )
        .unwrap()
        .get(0)
}
/// The brief's test, on protected format 2 and format 3: a legacy State of
/// 120 settled cohorts (indexes not initialized) and 1,200 legacy event
/// tombstone rows. The open commit moves 50 cohorts and deletes 500 rows;
/// two more commits finish both; history holds the 120 runs.
#[test]
fn legacy_protected_namespace_drains_records_and_sql_tombstones() {
    for (label, store) in both() {
        let ns = support::namespace("drain");
        let mut state = State::default();
        for i in 0..120 {
            let id = format!("run-0-legacy-{i:04}");
            let mut run = completed_run(&id);
            for kind in ["admitted", "terminal"] {
                state.sequence += 1;
                state.receipts.push(Receipt {
                    kind: kind.into(),
                    ..audit(state.sequence, &id)
                });
            }
            run.terminal_sequence = Some(state.sequence);
            state.runs.insert(id.clone(), run);
            state.budgets.insert(id, Budget::default());
        }
        state.indexes.initialized = false;
        store.acquire(&ns, "writer").unwrap();
        store
            .compare_and_swap(&ns, "writer", 0, &without_drain(&state))
            .unwrap();
        store.release(&ns, "writer").unwrap();
        legacy_rows(&ns, format_of(label), "event", 1_200);
        // Artifact rows guard artifact bytes: the drain leaves them.
        legacy_rows(&ns, format_of(label), "artifact", 1);
        let e = Engine::new(handle(label), &ns, "w").unwrap();
        assert_eq!(
            (legacy_runs_left(&e.store, &ns), tombstone_rows(&ns)),
            (70, 701),
            "{label}"
        );
        e.install(support::artifact("a")).unwrap();
        e.install(support::artifact("a")).unwrap();
        assert_eq!(
            (legacy_runs_left(&e.store, &ns), tombstone_rows(&ns)),
            (0, 1),
            "{label}: the artifact row stays"
        );
        assert!(e.snapshot().unwrap().retention.drain.done, "{label}");
        assert_eq!(history_runs(&ns), 120, "{label}");
    }
}
/// Once a commit finds fewer rows than its bound, the handle issues no
/// delete for that namespace again: rows that appear behind its back stay
/// (a drained namespace pays nothing per commit); a new handle probes once.
#[test]
fn drained_tombstone_rows_cost_no_statement_per_commit() {
    for (label, store) in both() {
        let ns = support::namespace("drained-rows");
        store.acquire(&ns, "setup").unwrap();
        store
            .compare_and_swap(&ns, "setup", 0, &State::default())
            .unwrap();
        store.release(&ns, "setup").unwrap();
        legacy_rows(&ns, format_of(label), "run", 3);
        let e = Engine::new(handle(label), &ns, "w").unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
        // Drained: rows written behind the handle's back are not looked for.
        legacy_rows(&ns, format_of(label), "run", 2);
        e.install(support::artifact("a")).unwrap();
        assert_eq!(tombstone_rows(&ns), 2, "{label}: no delete once drained");
        // A new handle probes once and finds them.
        e.release().unwrap();
        let e = Engine::new(handle(label), &ns, "w2").unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
        e.release().unwrap();
    }
}
/// The fixture of the runner's tests: a namespace the engine aq-server
/// vendors (0173673) wrote on its in-memory store (see that test).
fn vendored_fixture() -> State {
    serde_json::from_str(include_str!(
        "../../aq-playbooks-runner/tests/fixtures/legacy_state_0173673.json"
    ))
    .unwrap()
}
fn vendored_authority() -> Authority {
    Authority {
        actor: "alice".into(),
        scope: "project".into(),
        capabilities: ["execute".to_string()].into(),
    }
}
/// Drive a namespace an older engine wrote (`ns` on `store`, already
/// holding it) to fully drained while new runs are admitted and served at
/// every step: no commit fails, every legacy run is listed exactly once at
/// every step, legacy duplicates are recognised, and in the end the record
/// and the legacy tombstone rows are drained and every legacy run is in
/// history. Returns the number of steps.
fn drain_while_serving(e: &Engine<PostgresStorage>, ns: &str, legacy: &State) -> usize {
    let all: std::collections::BTreeSet<String> = legacy.runs.keys().cloned().collect();
    let listed_once = |step: usize| {
        let mut ids = vec![];
        let mut after = None;
        loop {
            let page = e
                .inspect(&InspectionQuery {
                    after,
                    limit: 7,
                    ..runs_query()
                })
                .unwrap();
            ids.extend(
                page.items
                    .iter()
                    .map(|r| r["id"].as_str().unwrap().to_string())
                    .filter(|id| all.contains(id)),
            );
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "step {step}: listed twice");
        assert_eq!(unique.len(), all.len(), "step {step}: missing");
    };
    let keyed = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "done".into(),
        expected_activation_revision: legacy.bindings["done"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: vendored_authority(),
    };
    let mut host = FakeHost::default();
    let mut step = 0;
    loop {
        listed_once(step);
        if e.snapshot().unwrap().retention.drain.done && tombstone_rows(ns) == 0 {
            break;
        }
        let t = 30_000 + step as i64;
        let id = e
            .admit("done", "main", Value::Null, vendored_authority(), t)
            .unwrap();
        e.drive(&mut host, t, 20).unwrap();
        assert!(!e.snapshot().unwrap().runs.contains_key(&id), "served");
        if legacy.keyed_admissions.contains_key("k-done") {
            assert!(e.admit_once(keyed("k-done"), t).unwrap().duplicate);
            assert!(e.admit_once(keyed("k-live"), t).unwrap().duplicate);
        }
        step += 1;
        assert!(step < 400, "the drain did not finish");
    }
    let s = e.snapshot().unwrap();
    let r = &s.retention;
    assert!(r.event_tombstones.is_empty() && r.expired_correlations.is_empty());
    assert!(r.disposals.is_empty());
    // Runs of cohorts that were settled when the older engine left them.
    let finished = legacy
        .runs
        .values()
        .filter(|run| {
            legacy
                .runs
                .values()
                .all(|other| other.root != run.root || other.status.terminal())
        })
        .count();
    assert!(
        s.runs.values().all(|run| s
            .runs
            .values()
            .any(|other| other.root == run.root && !other.status.terminal())),
        "only live cohorts remain"
    );
    assert!(history_runs(ns) >= finished as i64);
    step
}
/// Settle every live run the older engine left (the fixture's shapes): the
/// waiting run and both children receive their event, the asynchronous
/// operation's handle is found by recovery and its completion arrives, the
/// admitted and keyed runs advance. Every one finishes `ok` and leaves.
fn finish_live_legacy(e: &Engine<PostgresStorage>, legacy: &State, t: i64) {
    use aq_playbooks_model::EventEnvelope;
    let day = legacy.waits.values().find_map(|w| w.event.clone()).unwrap();
    let completion = legacy
        .events
        .values()
        .find(|e| e.envelope.event_id.starts_with("async-before"))
        .unwrap()
        .envelope
        .clone();
    let wake = EventEnvelope {
        event_id: "wake".into(),
        contract: day,
        correlation: BTreeMap::from([("invocation".into(), "exact-invocation".into())]),
        subject: None,
        revision: None,
        ..completion.clone()
    };
    let wake = EventEnvelope {
        payload: Value::Null,
        ..wake
    };
    e.admit_event("host", wake, t).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    e.recover(&mut host, t + 1).unwrap();
    e.admit_event(
        "host",
        EventEnvelope {
            event_id: "completion".into(),
            correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
            ..completion
        },
        t + 2,
    )
    .unwrap();
    for t in t + 2..t + 10 {
        e.drive(&mut host, t, 50).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty(), "{:?}", s.runs.keys().collect::<Vec<_>>());
    let runs = run_ids(&e.store, &e.namespace);
    for (id, run) in &legacy.runs {
        assert!(runs.contains(&Value::from(id.as_str())), "{id}");
        if !run.status.terminal() {
            let page = e
                .inspect(&InspectionQuery {
                    run: Some(id.clone()),
                    ..runs_query()
                })
                .unwrap();
            assert_eq!(page.items[0]["outcome"], "ok", "{id}: {:?}", page.items[0]);
        }
    }
}
/// The vendored engine's namespace (fixture) on protected format 2 and on
/// format 3, with the legacy tombstone rows its GC wrote (5 runs, 3 events)
/// plus 200 more, and bounds of 4 cohorts and 3 entries per commit: it
/// drains over many commits while new runs are served.
#[test]
fn vendored_namespace_drains_on_postgres_while_new_runs_are_served() {
    for (label, store) in both() {
        let ns = support::namespace("vendored");
        let legacy = vendored_fixture();
        let mut stored = legacy.clone();
        stored.retention.policy.drain_cohorts_per_commit = 4;
        stored.retention.policy.drain_entries_per_commit = 3;
        store.acquire(&ns, "vendored").unwrap();
        store
            .compare_and_swap(&ns, "vendored", 0, &without_drain(&stored))
            .unwrap();
        store.release(&ns, "vendored").unwrap();
        legacy_rows(&ns, format_of(label), "run", 5);
        legacy_rows(&ns, format_of(label), "event", 203);
        let e = Engine::new(handle(label), &ns, "upgraded").unwrap();
        let steps = drain_while_serving(&e, &ns, &legacy);
        assert!(steps > 5, "{label}: {steps} steps");
        finish_live_legacy(&e, &legacy, 40_000);
    }
}
/// A namespace the vendored engine's own PostgreSQL crate wrote on protected
/// format 2 (key `k`, `[7; 32]`), named by `AQ_PLAYBOOKS_VENDORED_NAMESPACE`
/// in the test database: produced by the generator in
/// `.local/task9/generator/` (see the Task 9 report). Run with `--ignored`.
/// Its stored drain bounds are first lowered to 4 cohorts and 3 entries per
/// commit (one direct CAS, which drains nothing of the record), so the
/// drain spans many commits while new runs are served; then every live run
/// the vendored engine left is driven to completion.
#[test]
#[ignore = "needs a namespace written by the vendored engine"]
fn a_namespace_the_vendored_engine_wrote_on_postgres_drains() {
    let ns = std::env::var("AQ_PLAYBOOKS_VENDORED_NAMESPACE").unwrap();
    let keys = || {
        aq_playbooks_postgres::ProtectionKeyring::new(
            "k",
            BTreeMap::from([("k".into(), [7u8; 32])]),
        )
        .unwrap()
    };
    let store = PostgresStorage::connect_protected(&support::database(), keys()).unwrap();
    let legacy = store.load(&ns).unwrap().state;
    let before = tombstone_rows(&ns);
    assert!(before > 0, "the vendored GC wrote tombstone rows");
    let mut bounded = legacy.clone();
    bounded.retention.policy.drain_cohorts_per_commit = 4;
    bounded.retention.policy.drain_entries_per_commit = 3;
    store.acquire(&ns, "setup").unwrap();
    let revision = store.load(&ns).unwrap().revision;
    store
        .compare_and_swap(&ns, "setup", revision, &without_drain(&bounded))
        .unwrap();
    store.release(&ns, "setup").unwrap();
    let e = Engine::new(
        PostgresStorage::connect_protected(&support::database(), keys()).unwrap(),
        &ns,
        "upgraded",
    )
    .unwrap();
    assert!(!e.snapshot().unwrap().retention.drain.done);
    let steps = drain_while_serving(&e, &ns, &legacy);
    eprintln!(
        "VENDORED-PG legacy runs {} (live {}), rows {before}, steps {steps}, history runs {}",
        legacy.runs.len(),
        legacy
            .runs
            .values()
            .filter(|r| !r.status.terminal())
            .count(),
        history_runs(&ns)
    );
    assert!(steps > 1, "{steps} steps");
    finish_live_legacy(&e, &legacy, 40_000);
}

// --- Task 10, item A: receipt index rows keyed by sequence ------------------

/// The runtime-index rows of receipts (`receipt` and `all_receipt`) the
/// namespace holds: (kind, run, sequence) each.
fn receipt_index_rows(ns: &str) -> Vec<(String, String, i64)> {
    support::client()
        .query(
            "SELECT kind,lookup_key,sequence FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt') ORDER BY kind,lookup_key,sequence",
            &[&ns],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}
/// The same rows as the State's receipt indexes say they must be.
fn expected_receipt_index_rows(s: &State) -> Vec<(String, String, i64)> {
    let mut rows: Vec<(String, String, i64)> = s
        .indexes
        .receipts
        .iter()
        .flat_map(|(run, receipts)| {
            receipts
                .keys()
                .map(|sequence| ("receipt".to_string(), run.clone(), *sequence as i64))
        })
        .chain(
            s.indexes
                .all_receipts
                .keys()
                .map(|sequence| ("all_receipt".to_string(), String::new(), *sequence as i64)),
        )
        .collect();
    rows.sort();
    rows
}
/// The newest transaction id so far; rows a later commit inserts or updates
/// carry a larger `xmin` (a disposable test database never wraps).
fn current_xid() -> i64 {
    support::client()
        .query_one("SELECT pg_current_xact_id()::text::bigint", &[])
        .unwrap()
        .get(0)
}
/// Receipt index rows inserted or updated after `xid`.
fn receipt_rows_written_since(ns: &str, xid: i64) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt') AND xmin::text::bigint > $2",
            &[&ns, &(xid % 4_294_967_296)],
        )
        .unwrap()
        .get(0)
}
/// Rewrite the namespace's receipt index rows as an engine before this
/// change wrote them: `item_key` is the receipt's position in the State's
/// receipts (they are in sequence order).
fn rewrite_receipt_rows_by_position(ns: &str) {
    let mut c = support::client();
    let mut tx = support::marked(&mut c, ns);
    tx.execute(
        "UPDATE aq_playbooks.runtime_index r SET item_key=(o.position-1)::text FROM (SELECT sequence,row_number() OVER (ORDER BY sequence) AS position FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='all_receipt') o WHERE r.namespace=$1 AND r.kind IN ('receipt','all_receipt') AND r.sequence=o.sequence",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
}
/// Every receipt index query answers as the State's own indexes do.
fn receipt_queries_match(e: &Engine<PostgresStorage>, ns: &str) {
    let s = e.snapshot().unwrap();
    let runs: Vec<Option<String>> = std::iter::once(None)
        .chain(s.indexes.receipts.keys().cloned().map(Some))
        .collect();
    for run in runs {
        let query = IndexQuery::Receipts {
            run,
            after_sequence: 0,
            limit: 1000,
        };
        assert_eq!(
            serde_json::to_value(e.store.query(ns, &query).unwrap()).unwrap(),
            serde_json::to_value(s.indexes.query(&s, &query)).unwrap(),
            "{query:?}"
        );
    }
}
/// `waiters` live runs waiting (each with its receipts in the record), the
/// first admitted earliest; then the commit that times it out removes its
/// cohort, whose receipts are the oldest in the record. Returns the receipt
/// index rows that commit inserted or updated.
fn rows_written_when_the_oldest_cohort_leaves(waiters: usize, legacy_rows: bool) -> i64 {
    let ns = support::namespace("receipt_rows");
    let e = Engine::new(support::protected(), &ns, "w").unwrap();
    let w = pg_waiter_artifact();
    let digest = w.digest.clone();
    e.install(w).unwrap();
    e.activate_at("waiter", &digest, pg_authority(), 0).unwrap();
    let first = e
        .admit("waiter", "main", Value::Null, pg_authority(), 0)
        .unwrap();
    e.advance(&first, 0).unwrap();
    for _ in 1..waiters {
        let id = e
            .admit("waiter", "main", Value::Null, pg_authority(), 1_000)
            .unwrap();
        e.advance(&id, 1_000).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().runs.len(), waiters);
    if legacy_rows {
        rewrite_receipt_rows_by_position(&ns);
        assert!(
            support::client()
                .query_one(
                    "SELECT bool_and(item_key !~ '^[0-9]{20}$') FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt')",
                    &[&ns],
                )
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    receipt_queries_match(&e, &ns);
    let xid = current_xid();
    e.drive(&mut FakeHost::default(), 150, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.runs.contains_key(&first),
        "the first waiter timed out and left"
    );
    assert_eq!(s.runs.len(), waiters - 1);
    let written = receipt_rows_written_since(&ns, xid);
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
    // The others time out and leave too; no receipt row is left behind
    // (rows of either keying are deleted with their receipts).
    e.drive(&mut FakeHost::default(), 1_200, 10 * waiters)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty());
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
    written
}
/// Task 9 review I1: on protected format 2 the runtime-index rows of
/// receipts are keyed by the receipt's sequence, not by its position in
/// the record. A commit that removes the oldest cohort (which moves every
/// later receipt) writes the rows of the receipts it adds, never those of
/// the receipts that moved: the same for 10 and for 40 live runs.
#[test]
fn a_commit_that_removes_a_cohort_writes_receipt_index_rows_for_that_cohort_only() {
    let few = rows_written_when_the_oldest_cohort_leaves(10, false);
    let many = rows_written_when_the_oldest_cohort_leaves(40, false);
    assert_eq!(few, many, "rows written do not depend on the record");
    assert!(few <= 10, "{few} rows");
}
/// Rows an engine before this change wrote (keyed by position) are left as
/// they are: no commit rewrites them. A receipt's row, of either keying,
/// is deleted when the receipt leaves; the queries answer the same from
/// both.
#[test]
fn receipt_index_rows_keyed_by_position_are_replaced_as_their_receipts_leave() {
    let few = rows_written_when_the_oldest_cohort_leaves(10, true);
    let many = rows_written_when_the_oldest_cohort_leaves(40, true);
    assert_eq!(few, many, "rows written do not depend on the record");
    assert!(few <= 10, "{few} rows");
}
/// A commit that writes every receipt row afresh (its namespace's indexes
/// are not marked materialized) over rows keyed by position leaves one row
/// per receipt, not two.
#[test]
fn rewriting_receipt_rows_over_rows_keyed_by_position_leaves_one_row_each() {
    let ns = support::namespace("receipt_rewrite");
    let e = Engine::new(support::protected(), &ns, "w").unwrap();
    let w = pg_waiter_artifact();
    let digest = w.digest.clone();
    e.install(w).unwrap();
    e.activate_at("waiter", &digest, pg_authority(), 0).unwrap();
    for _ in 0..5 {
        let id = e
            .admit("waiter", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&id, 0).unwrap();
    }
    rewrite_receipt_rows_by_position(&ns);
    let mut c = support::client();
    let mut tx = support::marked(&mut c, &ns);
    tx.execute(
        "UPDATE aq_playbooks.namespaces SET indexes_materialized=false WHERE namespace=$1",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
    e.admit("waiter", "main", Value::Null, pg_authority(), 1)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
}

// --- Task 10, item B: collect_garbage writes no legacy record -----------------

/// On protected format 2 and format 3, `collect_garbage` over an expired
/// event writes no `disposal_tombstones` row and no tombstone, fence or
/// disposal record: the event leaves by the commit's window drop, and a
/// redelivery is new.
#[test]
fn collect_garbage_writes_no_disposal_tombstone_row_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("gc_rows");
        let e = Engine::new(store, &ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            audit_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let first = e.admit_event("host", pg_event("gone"), 0).unwrap();
        let report = e.collect_garbage(10_000, 100).unwrap();
        assert_eq!(report.events, 1, "{format}");
        let s = e.snapshot().unwrap();
        assert!(!s.events.contains_key(&first.key), "{format}");
        assert!(s.retention.event_tombstones.is_empty(), "{format}");
        assert!(s.retention.expired_correlations.is_empty(), "{format}");
        assert!(s.retention.disposals.is_empty(), "{format}");
        assert_eq!(tombstone_rows(&ns), 0, "{format}");
        assert!(
            !e.admit_event("host", pg_event("gone"), 10_001)
                .unwrap()
                .duplicate,
            "{format}"
        );
    }
}

// --- Task 10 fix round 1, I1: tables with no statistics are analyzed at open --

/// A database of its own (`CREATE DATABASE`), so the tests below can reset
/// planner statistics without touching other tests' tables. Returns its
/// name and its connection string.
fn own_database(label: &str) -> (String, String) {
    let name = format!("t10_{}_{}", label, std::process::id());
    let mut c = support::client();
    c.batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
        .unwrap();
    c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
    (
        name.clone(),
        format!("{} dbname={name}", support::database()),
    )
}
/// As a fresh `pg_restore` leaves it: PostgreSQL 17 restores no column
/// statistics, while its index builds set `reltuples`. Needs a superuser,
/// as the disposable test server's `postgres` is.
fn forget_statistics(url: &str) {
    let mut c = postgres::Client::connect(url, postgres::NoTls).unwrap();
    // `reltuples` as the restore's index builds set it, then no statistics.
    c.batch_execute("ANALYZE aq_playbooks.runtime_index")
        .unwrap();
    c.batch_execute(
        "DELETE FROM pg_statistic WHERE starelid IN (SELECT oid FROM pg_class WHERE relnamespace='aq_playbooks'::regnamespace)",
    )
    .unwrap();
}
/// Whether `table` has column statistics.
fn has_statistics(url: &str, table: &str) -> bool {
    postgres::Client::connect(url, postgres::NoTls)
        .unwrap()
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_stats WHERE schemaname='aq_playbooks' AND tablename=$1)",
            &[&table],
        )
        .unwrap()
        .get(0)
}
fn reltuples(url: &str, table: &str) -> f32 {
    postgres::Client::connect(url, postgres::NoTls)
        .unwrap()
        .query_one(
            "SELECT reltuples FROM pg_class WHERE oid=('aq_playbooks.'||$1)::regclass",
            &[&table],
        )
        .unwrap()
        .get(0)
}
/// The plan of the drain's receipt-row delete for one named `all_receipt`
/// row of namespace `ns`.
fn receipt_delete_plan(url: &str, ns: &str) -> String {
    let rows = json!([{"kind":"all_receipt","lookup_key":"","item_key":"00000000000000000005","keep":false}]);
    postgres::Client::connect(url, postgres::NoTls)
        .unwrap()
        .query(
            "EXPLAIN DELETE FROM aq_playbooks.runtime_index WHERE ctid=ANY(ARRAY(SELECT r.ctid FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,keep boolean) CROSS JOIN LATERAL (SELECT old.ctid FROM aq_playbooks.runtime_index old WHERE old.namespace=$1 AND old.kind=d.kind AND old.lookup_key=d.lookup_key AND old.sequence=d.item_key::bigint AND (NOT d.keep OR old.item_key<>d.item_key) OFFSET 0) r))",
            &[&ns, &rows],
        )
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect::<Vec<_>>()
        .join("\n")
}
/// Review I1: the receipt-row delete stays an index lookup per named row
/// only when `runtime_index` has statistics. A database restored without
/// them (and not yet reached by autovacuum) is analyzed when the storage
/// connects, before any commit: every engine table never analyzed, or
/// holding rows but no column statistics, gets its own `ANALYZE`.
#[test]
fn opening_the_storage_analyzes_engine_tables_that_have_no_statistics() {
    let (name, url) = own_database("analyze");
    let ns = support::namespace("analyze");
    let store = PostgresStorage::connect(&url).unwrap();
    store.acquire(&ns, "w").unwrap();
    let mut state = State::default();
    for sequence in 1..=4_000u64 {
        let run = format!("run-{}", sequence % 4);
        state.receipts.push(live_receipt(sequence));
        let offset = state.receipts.len() - 1;
        state.receipts[offset].run = run.clone();
        state
            .indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        state.indexes.all_receipts.insert(sequence, offset);
    }
    store.compare_and_swap(&ns, "w", 0, &state).unwrap();
    store.release(&ns, "w").unwrap();
    drop(store);
    forget_statistics(&url);
    assert!(
        reltuples(&url, "runtime_index") >= 4_000.0,
        "as a restore leaves it"
    );
    assert!(!has_statistics(&url, "runtime_index"));
    // Without statistics the planner reads every receipt row per lookup.
    assert!(!receipt_delete_plan(&url, &ns).contains("runtime_correlated_sequence"));
    let store = PostgresStorage::connect(&url).unwrap();
    assert!(has_statistics(&url, "runtime_index"));
    assert!(reltuples(&url, "run_history") >= 0.0, "empty tables too");
    let plan = receipt_delete_plan(&url, &ns);
    assert!(plan.contains("runtime_correlated_sequence"), "{plan}");
    // Tables that have statistics are not analyzed again.
    let analyzed = |c: &mut postgres::Client| -> i64 {
        c.query_one(
            "SELECT coalesce(sum(analyze_count),0)::bigint FROM pg_stat_user_tables WHERE schemaname='aq_playbooks'",
            &[],
        )
        .unwrap()
        .get(0)
    };
    let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let before = analyzed(&mut c);
    drop(PostgresStorage::connect(&url).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(600));
    let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    assert_eq!(analyzed(&mut c), before);
    drop(store);
    drop(c);
    support::client()
        .batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .unwrap();
}
/// A role that may write the engine's tables but not analyze them (it does
/// not own them) still opens: the `ANALYZE` is skipped with a warning and
/// never fails the connection.
#[test]
fn a_role_that_cannot_analyze_still_opens() {
    let (name, url) = own_database("noanalyze");
    drop(PostgresStorage::connect(&url).unwrap());
    let role = format!("t10_writer_{}", std::process::id());
    let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    c.batch_execute(&format!(
        "DROP ROLE IF EXISTS {role};
         CREATE ROLE {role} LOGIN PASSWORD 'synthetic-writer';
         GRANT CONNECT, CREATE ON DATABASE {name} TO {role};
         GRANT USAGE, CREATE ON SCHEMA aq_playbooks TO {role};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA aq_playbooks TO {role};"
    ))
    .unwrap();
    // Never analyzed, as a database the owner has not opened yet.
    postgres::Client::connect(&url, postgres::NoTls)
        .unwrap()
        .batch_execute(
            "UPDATE pg_class SET reltuples=-1 WHERE relnamespace='aq_playbooks'::regnamespace AND relkind='r'",
        )
        .unwrap();
    let writer = format!("{url} user={role} password=synthetic-writer");
    let store = PostgresStorage::connect(&writer).unwrap();
    assert!(reltuples(&url, "namespaces") < 0.0, "skipped, not analyzed");
    drop(store);
    drop(c);
    let mut admin = support::client();
    admin
        .batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .unwrap();
    admin.batch_execute(&format!("DROP ROLE {role}")).unwrap();
}

// --- Task 11: format 3 parity with the in-memory reference ----------------

fn parity_keyed<S: Storage>(e: &Engine<S>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: pg_authority(),
    }
}
/// The serialized snapshot and the Runs, Effects and Receipts pages (items
/// and cursor; the page revision is the backend's own) of an engine.
type ParityView = (Value, Vec<(Vec<Value>, Option<String>)>);
fn parity_view<S: Storage>(e: &Engine<S>) -> ParityView {
    let pages = [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ]
    .into_iter()
    .map(|kind| {
        let page = e
            .inspect(&InspectionQuery {
                kind,
                ..runs_query()
            })
            .unwrap();
        (page.items, page.next)
    })
    .collect();
    (serde_json::to_value(e.snapshot().unwrap()).unwrap(), pages)
}
/// Run `$call` with `$e` bound to the reference engine, then to the format 3
/// engine; its results (`Debug`) and both engines' views must be equal.
/// Returns the format 3 result and the format 3 store's work counters for
/// the call alone.
macro_rules! on_both {
    ($reference:expr, $records:expr, $label:expr, |$e:ident| $call:expr) => {{
        $records.store.reset_transition_stats();
        let expected = {
            let $e = &$reference;
            $call
        };
        let actual = {
            let $e = &$records;
            $call
        };
        let stats = $records.store.transition_stats();
        assert_eq!(
            format!("{actual:?}"),
            format!("{expected:?}"),
            "{}: result",
            $label
        );
        let (reference, records) = (parity_view(&$reference), parity_view(&$records));
        assert_eq!(records.0, reference.0, "{}: snapshot", $label);
        assert_eq!(records.1, reference.1, "{}: inspection pages", $label);
        (actual, stats)
    }};
}
/// `state` with `count` settled legacy cohorts, as an older engine left
/// them: a completed root with its admitted and terminal receipts and its
/// budget, runtime indexes not initialized, no drain record.
fn with_legacy_cohorts(state: &State, count: usize) -> State {
    let mut state = state.clone();
    for i in 0..count {
        let id = format!("run-0-legacy-{i:04}");
        let mut run = completed_run(&id);
        for kind in ["admitted", "terminal"] {
            state.sequence += 1;
            state.receipts.push(Receipt {
                kind: kind.into(),
                ..audit(state.sequence, &id)
            });
        }
        run.terminal_sequence = Some(state.sequence);
        state.runs.insert(id.clone(), run);
        state.budgets.insert(id, Budget::default());
    }
    state.indexes.initialized = false;
    without_drain(&state)
}
/// The same script on `Engine<MemoryStorage>` (the reference) and on
/// `Engine<PostgresStorage>` writing format 3: after every call the two
/// agree (snapshot, Runs/Effects/Receipts pages, call results), and format 3
/// takes the commit path SC-13 names: a commit that settles a cohort is
/// heavy; an admission and the commit that drops expired window entries are
/// bounded; no commit that settles nothing takes a full-container fallback.
#[test]
fn format_3_matches_the_reference_for_settle_drop_stale_and_drain() {
    let ns = support::namespace("parity");
    let reference = Engine::new(MemoryStorage::default(), &ns, "w").unwrap();
    let records = Engine::new(support::records(), &ns, "w").unwrap();
    let a = support::artifact("a");
    let digest = a.digest.clone();
    let setup = [
        on_both!(reference, records, "install", |e| e.install(a.clone())),
        on_both!(reference, records, "activate", |e| e.activate_at(
            "a",
            &digest,
            pg_authority(),
            0
        )),
        on_both!(reference, records, "policy", |e| e.set_retention_policy(
            RetentionPolicy {
                admission_replay_ms: 1_000,
                ..RetentionPolicy::default()
            }
        )),
    ];
    for (result, stats) in setup {
        result.unwrap();
        assert!(stats.fallbacks.is_empty(), "setup: {:?}", stats.fallbacks);
    }
    for i in 0..3_i64 {
        let label = format!("admit k-{i}");
        let (_, stats) = on_both!(reference, records, label, |e| e
            .admit_once(parity_keyed(e, &format!("k-{i}")), i));
        assert_eq!((stats.incremental, stats.heavy), (1, 0), "{label}");
        assert!(stats.fallbacks.is_empty(), "{label}: {:?}", stats.fallbacks);
        let label = format!("settle k-{i}");
        let (_, stats) = on_both!(reference, records, label, |e| e.drive(
            &mut FakeHost::default(),
            i,
            20
        ));
        assert_eq!(stats.heavy, 1, "{label}: {stats:?}");
        assert!(records.snapshot().unwrap().runs.is_empty(), "{label}");
    }
    // k-0..k-2 expire at 1_000..1_002: the admission at 2_000 drops all three.
    let (_, stats) = on_both!(reference, records, "drop", |e| e
        .admit_once(parity_keyed(e, "k-3"), 2_000));
    assert_eq!((stats.incremental, stats.heavy), (1, 0), "drop: {stats:?}");
    assert!(stats.fallbacks.is_empty(), "drop: {:?}", stats.fallbacks);
    let s = records.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
        vec!["k-3".to_string()]
    );
    // A forgotten key whose request time is older than the window.
    let (stale, stats) = on_both!(reference, records, "stale", |e| e.admit_once_at(
        parity_keyed(e, "k-0"),
        Some(500),
        2_001
    ));
    assert!(
        matches!(stale, Err(Error::Stale { .. })),
        "stale: {stale:?}"
    );
    assert_eq!((stats.incremental, stats.heavy), (0, 0), "stale: {stats:?}");
    // 60 legacy cohorts seeded by a direct CAS on each backend, then opened
    // again: the drain takes 50 at open and the rest in the next commit.
    let reference_store = reference.store.clone();
    let revision = reference_store.load(&ns).unwrap().revision;
    let legacy = with_legacy_cohorts(&reference.snapshot().unwrap(), 60);
    reference_store
        .compare_and_swap(&ns, "w", revision, &legacy)
        .unwrap();
    let revision = records.store.load(&ns).unwrap().revision;
    records
        .store
        .compare_and_swap(&ns, "w", revision, &legacy)
        .unwrap();
    reference.release().unwrap();
    records.release().unwrap();
    let reference = Engine::new(reference_store, &ns, "w2").unwrap();
    let records = Engine::new(support::records(), &ns, "w2").unwrap();
    let (r, p) = (parity_view(&reference), parity_view(&records));
    assert_eq!(p.0, r.0, "open: snapshot");
    assert_eq!(p.1, r.1, "open: inspection pages");
    let left = |e: &Engine<PostgresStorage>| {
        e.snapshot()
            .unwrap()
            .runs
            .keys()
            .filter(|id| id.starts_with("run-0-legacy-"))
            .count()
    };
    assert_eq!(left(&records), 10, "open drains 50");
    let (_, stats) = on_both!(reference, records, "drain", |e| e.drive(
        &mut FakeHost::default(),
        2_002,
        20
    ));
    assert_eq!(stats.heavy, 1, "drain: {stats:?}");
    assert_eq!(left(&records), 0, "drain: {stats:?}");
    let s = records.snapshot().unwrap();
    assert!(s.retention.drain.done && s.runs.is_empty(), "drained");
    assert_eq!(history_runs(&ns), 64, "60 legacy runs and k-0..k-3");
}
