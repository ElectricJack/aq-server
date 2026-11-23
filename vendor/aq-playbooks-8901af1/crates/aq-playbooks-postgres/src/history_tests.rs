//! `run_history` against an actual PostgreSQL: page plans and commit
//! atomicity. Feature-gated like the storage suite; the caller supplies an
//! explicitly disposable database.
use super::*;
use crate::catalog_tests::support::{self, client, database, namespace};
use aq_playbooks_runner::{InspectionKind, Receipt, SettledCohort};
use serde_json::json;
use std::sync::atomic::Ordering;

fn cohort(id: &str, sequence: u64) -> SettledCohort {
    let mut run = support::boundary("history").runs["parent"].clone();
    run.id = id.into();
    run.root = id.into();
    SettledCohort {
        root: id.into(),
        finished_at_ms: 10,
        runs: vec![run],
        intents: vec![],
        receipts: vec![Receipt {
            sequence,
            run: id.into(),
            step: "s".into(),
            at_ms: 10,
            kind: "terminal".into(),
            detail: json!({}),
        }],
    }
}
fn history_rows(ns: &str) -> i64 {
    client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}

/// Every scan of `run_history` in `plan` is an index scan, and no `Sort`
/// node has `run_history` below it.
fn check_plan(plan: &Value, under_sort: bool, problems: &mut Vec<String>) {
    let node = plan["Node Type"].as_str().unwrap_or_default();
    if plan["Relation Name"] == "run_history" {
        if !matches!(node, "Index Scan" | "Index Only Scan") {
            problems.push(format!("{node} on run_history"));
        }
        if under_sort {
            problems.push("Sort over run_history".into());
        }
    }
    for child in plan["Plans"].as_array().into_iter().flatten() {
        check_plan(child, under_sort || node == "Sort", problems);
    }
}

/// Whether some node of `plan` reads `index` (an index scan, an index-only
/// scan, or the bitmap index scan under a bitmap heap scan, which names the
/// index but not the table).
fn uses_index(plan: &Value, index: &str) -> bool {
    plan["Index Name"] == index
        || plan["Plans"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|child| uses_index(child, index))
}

/// An inspection page costs the same however much history the namespace
/// holds: PostgreSQL reads `run_history` through an ordered index scan for
/// every kind and cursor, never a sort or a sequential scan. A run-filtered
/// page reads it through the run index.
#[test]
fn inspection_pages_read_history_through_ordered_index_scans() {
    let (ns, store) = (
        namespace("plans"),
        PostgresStorage::connect(&database()).unwrap(),
    );
    store.acquire(&ns, "w").unwrap();
    let mut live = State::default();
    for sequence in 1..=50 {
        live.receipts.push(Receipt {
            sequence: 1_000_000 + sequence,
            run: format!("r{}", sequence % 5),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
    }
    store.compare_and_swap(&ns, "w", 0, &live).unwrap();
    let mut c = client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(WRITE_MARKER_SQL, &[&ns]).unwrap();
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,k,lpad(i::text,20,'0'),'r'||(i%50),'r'||(i%50),i,i,jsonb_build_object('sequence',i) FROM generate_series(1,30000) i, unnest(ARRAY['runs','effects','receipts']) k",&[&ns]).unwrap();
    tx.commit().unwrap();
    c.batch_execute("ANALYZE aq_playbooks.run_history; ANALYZE aq_playbooks.inspection_rows")
        .unwrap();
    let mut problems = vec![];
    for kind in [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ] {
        for run in [None, Some("r7".to_string())] {
            for after in [None, Some(format!("{:020}", 15_000))] {
                for limit in [3_i64, 1001] {
                    for (name, sql) in [("inspect", INSPECT_SQL), ("page", history::PAGE_SQL)] {
                        let explain = format!("EXPLAIN (FORMAT JSON) {sql}");
                        let plan: Value = c
                            .query_one(explain.as_str(), &[&ns, &kind.key(), &run, &after, &limit])
                            .unwrap()
                            .get(0);
                        let mut found = vec![];
                        if run.is_some() {
                            // A run's history is small: any plan that reads
                            // it through the run index is bounded, sorted
                            // or not.
                            if !uses_index(&plan[0]["Plan"], "run_history_by_run") {
                                found.push("run_history_by_run not used".into());
                            }
                        } else {
                            check_plan(&plan[0]["Plan"], false, &mut found);
                        }
                        problems.extend(found.into_iter().map(|p| {
                            format!(
                                "{name} {} run={run:?} after={after:?} limit={limit}: {p}",
                                kind.key()
                            )
                        }));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    // The page itself: live receipts after every history receipt, in order.
    let page = store
        .inspect(
            &ns,
            &aq_playbooks_runner::InspectionQuery {
                kind: InspectionKind::Receipts,
                run: None,
                after: Some(format!("{:020}", 29_999)),
                limit: 3,
            },
        )
        .unwrap();
    let sequences: Vec<_> = page.items.iter().map(|v| v["sequence"].clone()).collect();
    assert_eq!(
        sequences,
        vec![json!(30_000), json!(1_000_001), json!(1_000_002)]
    );
}

/// A failure after the history insert rolls the history rows back with the
/// rest of the commit, and the cohort can be written by the retry.
#[test]
fn a_commit_failing_after_the_history_insert_leaves_no_rows() {
    for records in [false, true] {
        let ns = namespace("rollback");
        let store = if records {
            PostgresStorage::connect_records(&database()).unwrap()
        } else {
            PostgresStorage::connect(&database()).unwrap()
        };
        store.acquire(&ns, "w").unwrap();
        let state = State {
            settled: vec![cohort("a", 1)],
            ..State::default()
        };
        store
            .fail_catalog_before_snapshot
            .store(true, Ordering::SeqCst);
        let refused = store.compare_and_swap(&ns, "w", 0, &state).unwrap_err();
        assert!(
            refused.to_string().contains("synthetic rollback"),
            "{refused}"
        );
        assert_eq!(history_rows(&ns), 0, "records={records}");
        assert_eq!(store.compare_and_swap(&ns, "w", 0, &state).unwrap(), 1);
        assert_eq!(history_rows(&ns), 2, "records={records}");
    }
}

/// The trim (Task 8b R2, fix round 1) is index-backed: the oldest runs,
/// event receipts and admin receipts come from their partial sequence
/// indexes in order (no sort, no other scan); a run's items and an event's
/// receipts are deleted through `run_history_by_run`, run rows and admin
/// receipts through the primary key. Over 90,000 run rows and items plus
/// 30,000 event and admin receipts.
#[test]
fn history_trim_reads_and_deletes_through_indexes() {
    let (ns, store) = (
        namespace("trim_plans"),
        PostgresStorage::connect(&database()).unwrap(),
    );
    store.acquire(&ns, "w").unwrap();
    store
        .compare_and_swap(&ns, "w", 0, &State::default())
        .unwrap();
    let mut c = client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(WRITE_MARKER_SQL, &[&ns]).unwrap();
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,k,CASE k WHEN 'runs' THEN 'r'||lpad(i::text,6,'0') ELSE lpad(i::text,20,'0') END,'r'||lpad(i::text,6,'0'),'r'||lpad(i::text,6,'0'),i,i,jsonb_build_object('sequence',i) FROM generate_series(1,30000) i, unnest(ARRAY['runs','effects','receipts']) k",&[&ns]).unwrap();
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,'receipts',lpad(i::text,20,'0'),CASE WHEN i<=50000 THEN 'event:e'||(i/2) ELSE 'admin' END,'x',i,i,'{}'::jsonb FROM generate_series(30001,60000) i",&[&ns]).unwrap();
    tx.commit().unwrap();
    c.batch_execute("ANALYZE aq_playbooks.run_history").unwrap();
    let mut plan = |sql: &str, params: &[&(dyn postgres::types::ToSql + Sync)]| -> Value {
        let explain = format!("EXPLAIN (FORMAT JSON) {sql}");
        let plan: Value = c.query_one(explain.as_str(), params).unwrap().get(0);
        plan[0]["Plan"].clone()
    };
    let mut problems = vec![];
    for limit in [1_i64, 64, 1_000] {
        for (name, sql, index) in [
            (
                "runs",
                history::OLDEST_RUNS_SQL,
                "run_history_runs_by_sequence",
            ),
            (
                "event receipts",
                history::OLDEST_EVENT_RECEIPTS_SQL,
                "run_history_event_receipts_by_sequence",
            ),
            (
                "admin receipts",
                history::OLDEST_ADMIN_RECEIPTS_SQL,
                "run_history_admin_receipts_by_sequence",
            ),
        ] {
            let oldest = plan(sql, &[&ns, &limit]);
            if !uses_index(&oldest, index) {
                problems.push(format!(
                    "oldest {name}, limit {limit}: {index} not used: {oldest}"
                ));
            }
            check_plan(&oldest, false, &mut problems);
        }
    }
    let owners: Vec<String> = (15_001..15_065).map(|i| format!("event:e{i}")).collect();
    let events = plan(history::TRIM_EVENT_RECEIPTS_SQL, &[&ns, &owners]);
    if !uses_index(&events, "run_history_by_run") {
        problems.push(format!("trim event receipts: run index not used: {events}"));
    }
    let admin: Vec<String> = (50_001..50_065).map(|i| format!("{i:020}")).collect();
    let admin = plan(history::TRIM_ADMIN_RECEIPTS_SQL, &[&ns, &admin]);
    if !uses_index(&admin, "run_history_pkey") {
        problems.push(format!(
            "trim admin receipts: primary key not used: {admin}"
        ));
    }
    let victims: Vec<String> = (1..=64).map(|i| format!("r{i:06}")).collect();
    let items = plan(history::TRIM_ITEMS_SQL, &[&ns, &victims]);
    if !uses_index(&items, "run_history_by_run") {
        problems.push(format!("trim items: run index not used: {items}"));
    }
    let runs = plan(history::TRIM_RUNS_SQL, &[&ns, &victims]);
    if !uses_index(&runs, "run_history_pkey") {
        problems.push(format!("trim runs: primary key not used: {runs}"));
    }
    for (name, p) in [
        ("items", &items),
        ("runs", &runs),
        ("event receipts", &events),
        ("admin receipts", &admin),
    ] {
        let mut found = vec![];
        no_sequential_scan(p, &mut found);
        problems.extend(found.into_iter().map(|f| format!("trim {name}: {f}")));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}
/// No node of `plan` is a sequential scan of `run_history`.
fn no_sequential_scan(plan: &Value, problems: &mut Vec<String>) {
    if plan["Relation Name"] == "run_history" && plan["Node Type"] == "Seq Scan" {
        problems.push("Seq Scan on run_history".into());
    }
    for child in plan["Plans"].as_array().into_iter().flatten() {
        no_sequential_scan(child, problems);
    }
}
/// The delete of receipt rows by sequence (`indexes::sync`) reads the
/// runtime index's `(namespace, kind, lookup_key, sequence, item_key)`
/// index once per row it names, then deletes by tuple id: no scan of the
/// namespace's receipt rows, on a namespace holding 4,000 receipts with
/// planner statistics, in a database of its own (the plan must not depend
/// on what other tests left in the tables).
#[test]
fn receipt_rows_are_deleted_by_sequence_through_an_index() {
    let db = format!("receipt_plan_{}", std::process::id());
    client()
        .batch_execute(&format!("DROP DATABASE IF EXISTS {db}"))
        .unwrap();
    client()
        .batch_execute(&format!("CREATE DATABASE {db}"))
        .unwrap();
    let url = format!("{} dbname={db}", database());
    let client = || postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let (ns, store) = (
        namespace("receipt_plan"),
        PostgresStorage::connect(&url).unwrap(),
    );
    store.acquire(&ns, "w").unwrap();
    let mut state = State::default();
    for sequence in 1..=4_000u64 {
        let run = format!("run-{}", sequence % 4);
        state.receipts.push(Receipt {
            sequence,
            run: run.clone(),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
        let offset = state.receipts.len() - 1;
        state
            .indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        state.indexes.all_receipts.insert(sequence, offset);
    }
    store.compare_and_swap(&ns, "w", 0, &state).unwrap();
    let mut c = client();
    c.batch_execute("ANALYZE aq_playbooks.runtime_index")
        .unwrap();
    for (kind, lookup) in [("all_receipt", ""), ("receipt", "run-1")] {
        let rows = json!([{"kind":kind,"lookup_key":lookup,"item_key":"00000000000000000005","keep":false}]);
        let plan: Vec<String> = c
            .query(
                &format!(
                    "EXPLAIN {}",
                    crate::indexes::RECEIPT_ROWS_BY_SEQUENCE_DELETE
                ),
                &[&ns, &rows],
            )
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains("runtime_correlated_sequence"), "{plan}");
        assert!(plan.contains("Tid Scan"), "{plan}");
        assert!(!plan.contains("Seq Scan on runtime_index"), "{plan}");
    }
    drop(store);
    drop(c);
    super::catalog_tests::support::client()
        .batch_execute(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .unwrap();
}
