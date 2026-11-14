//! After `engine_growth` has left
//! N completed runs in namespace `growth` (configs a/b/c, no GC), shorten the
//! retention windows to the minimum and call `collect_garbage` with a fixed
//! page limit until nothing is left, timing every call. Each call removes about
//! the same number of runs, while the total state shrinks; this separates GC
//! cost that depends on total state from cost that depends on what is removed.
//!
//! Usage: engine_growth_drain <config a|b|c> <page limit> <database url>
//!        engine_growth_drain upgrade <runs> <steady bytes> <database url>
//!
//! `upgrade` (spec 0003 acceptance 4) opens the restored legacy namespace
//! `growth` (protected format 2, bench key `[7u8; 32]`) written by the
//! engine before spec 0003, sets the 10-minute policy, and serves `runs`
//! new runs 10 s apart starting at `1000 * 100_000 + 86_400_000 + 1_000`
//! ms, as `engine_growth steady` serves them, never calling
//! `collect_garbage`. It prints a `commit` line for every commit (step
//! time, record bytes, the drain state) and the verdict: every legacy run
//! `run-*` is in history, the record (`SIZE_SQL` total) is within 1.2x of
//! `steady bytes` (`engine_growth steady` at run 200) once `drain.done`,
//! and every new run completed `ok`.
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::{retention::RetentionPolicy, *};
use postgres::{Client, NoTls};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

#[allow(dead_code)]
#[path = "engine_growth.rs"]
mod growth;

fn bench_keyring() -> ProtectionKeyring {
    ProtectionKeyring::new(
        "bench-key",
        BTreeMap::from([("bench-key".to_string(), [7u8; 32])]),
    )
    .unwrap()
}

/// Spec 0003 acceptance 4 (see the module docs).
fn upgrade(runs: usize, steady_bytes: i64, url: &str) {
    let inner = PostgresStorage::connect_protected(url, bench_keyring()).unwrap();
    // The legacy runs, read before the open commit drains any of them.
    let legacy_state = inner.load(growth::NS).unwrap().state;
    let legacy: BTreeSet<String> = legacy_state
        .runs
        .keys()
        .filter(|id| id.starts_with("run-"))
        .cloned()
        .collect();
    let legacy_finished = legacy_state
        .runs
        .values()
        .filter(|r| r.status.terminal())
        .count();
    drop(legacy_state);
    let started = Instant::now();
    let e = Engine::new(
        growth::Measured::new(inner, url, u64::MAX),
        growth::NS,
        "bench-upgrade",
    )
    .unwrap();
    let mut policy = e.snapshot().unwrap().retention.policy;
    policy.event_replay_ms = growth::ACCEPTANCE_WINDOW_MS;
    policy.admission_replay_ms = growth::ACCEPTANCE_WINDOW_MS;
    e.set_retention_policy(policy.clone()).unwrap();
    let state = e.snapshot().unwrap();
    let binding = &state.bindings["growth"];
    assert_eq!(
        binding.artifact,
        growth::playbook().digest,
        "the dump's playbook is the benchmark's"
    );
    println!(
        "{}",
        json!({"kind":"upgrade_setup","runs":runs,"steady_bytes":steady_bytes,
               "legacy_runs":legacy.len(),"legacy_finished":legacy_finished,"policy":policy})
    );
    let bench = growth::Bench {
        e: &e,
        activation_revision: binding.revision,
        prefix: "upgrade-".into(),
        timed: true,
    };
    drop(state);
    let mut host = growth::BenchHost;
    let base = 1000 * 100_000 + 86_400_000 + 1_000;
    let mut new_ids = vec![];
    for k in 1..=runs {
        let now = base + (k as i64 - 1) * growth::STEADY_SPACING_MS;
        new_ids.push(growth::serve_run(&bench, &mut host, k, now, 10).0);
        e.store.close_unchanged();
        e.store.drain();
    }
    let elapsed_s = started.elapsed().as_secs_f64();
    let items = growth::all_runs(&e);
    let by_id: BTreeMap<&str, &serde_json::Value> = items
        .iter()
        .map(|v| (v["id"].as_str().unwrap(), v))
        .collect();
    let state = e.snapshot().unwrap();
    let missing_legacy = legacy
        .iter()
        .filter(|id| !by_id.contains_key(id.as_str()) || state.runs.contains_key(*id))
        .count();
    let completed = serde_json::to_value(RunStatus::Completed).unwrap();
    let new_ok = new_ids
        .iter()
        .filter(|id| {
            by_id
                .get(id.as_str())
                .is_some_and(|v| v["status"] == completed && v["outcome"] == "ok")
        })
        .count();
    let commits = e.store.commits.lock().unwrap().clone();
    let done_at = commits.iter().find(|c| c.2).map(|c| c.0);
    let after_done: Vec<i64> = commits.iter().filter(|c| c.2).map(|c| c.1).collect();
    let max_after_done = after_done.iter().copied().max();
    let final_bytes = commits.last().map(|c| c.1);
    let limit = steady_bytes as f64 * 1.2;
    let bounded = max_after_done.is_some_and(|m| m as f64 <= limit);
    let pass = done_at.is_some() && missing_legacy == 0 && new_ok == runs && bounded;
    println!(
        "{}",
        json!({"kind":"verdict","acceptance":4,"pass":pass,
               "legacy_runs":legacy.len(),"legacy_missing_from_history":missing_legacy,
               "new_runs":runs,"new_runs_ok":new_ok,
               "drain_done_at_commit":done_at,"commits":commits.len(),
               "record_bytes_at_done":done_at.and_then(|n| commits.iter().find(|c| c.0 == n)).map(|c| c.1),
               "record_bytes_max_after_done":max_after_done,"record_bytes_final":final_bytes,
               "steady_bytes":steady_bytes,"limit_bytes":limit,
               "max_after_done_ratio":max_after_done.map(|m| m as f64 / steady_bytes as f64),
               "final_ratio":final_bytes.map(|m| m as f64 / steady_bytes as f64),
               "live_runs_left":state.runs.len(),"elapsed_s":elapsed_s})
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config = args[1].as_str();
    if config == "upgrade" {
        return upgrade(args[2].parse().unwrap(), args[3].parse().unwrap(), &args[4]);
    }
    let limit: usize = args[2].parse().unwrap();
    let url = &args[3];
    let store = match config {
        "a" => PostgresStorage::connect_protected(url, bench_keyring()).unwrap(),
        "c" => PostgresStorage::connect_records(url).unwrap(),
        _ => PostgresStorage::connect(url).unwrap(),
    };
    let mut monitor = Client::connect(url, NoTls).unwrap();
    let mut lsn = || -> i64 {
        monitor
            .query_one(
                "SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(),'0/0')::bigint",
                &[],
            )
            .unwrap()
            .get(0)
    };
    let e = Engine::new(store, "growth", "bench-drain").unwrap();
    let state = e.snapshot().unwrap();
    let runs0 = state.runs.len();
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 100_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    // Past the last run's activity (runs are spaced 100 s apart) and past the
    // replay horizon each event captured at admission (default policy: 1 day).
    let mut now = ((runs0 as i64 + 10) * 100_000 + 86_400_000 + 1_000)
        .max(state.retention.last_gc_at_ms.unwrap_or(0) + 86_400_000 + 1_000);
    println!(
        "{}",
        json!({"kind":"drain_setup","config":config,"limit":limit,"runs_before":runs0,
               "events_before":state.events.len(),"receipts_before":state.receipts.len()})
    );
    drop(state);
    loop {
        let before = e.store.transition_stats();
        let l0 = lsn();
        let t0 = Instant::now();
        let report = e.collect_garbage(now, limit).unwrap();
        let us = t0.elapsed().as_micros() as u64;
        let wal = lsn() - l0;
        let after = e.store.transition_stats();
        let s = e.snapshot().unwrap();
        println!(
            "{}",
            json!({"kind":"drain_call","us":us,"wal":wal,"report":report,
                   "runs_after":s.runs.len(),"events_after":s.events.len(),
                   "receipts_after":s.receipts.len(),
                   "state_json_bytes": serde_json::to_vec(&s).unwrap().len(),
                   "heavy":after.heavy-before.heavy,"incremental":after.incremental-before.incremental,
                   "records_written":after.records_written-before.records_written,
                   "record_reads":after.record_reads-before.record_reads})
        );
        now += 1;
        if !report.more
            && ((s.runs.is_empty() && s.events.is_empty())
                || (report.runs == 0 && report.events == 0))
        {
            break;
        }
    }
}
