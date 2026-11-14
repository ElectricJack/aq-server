//! After `engine_growth` has left
//! N completed runs in namespace `growth` (configs a/b/c, no GC), shorten the
//! retention windows to the minimum and call `collect_garbage` with a fixed
//! page limit until nothing is left, timing every call. Each call removes about
//! the same number of runs, while the total state shrinks; this separates GC
//! cost that depends on total state from cost that depends on what is removed.
//!
//! Usage: engine_growth_drain <config a|b|c> <page limit> <database url>
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::{retention::RetentionPolicy, *};
use postgres::{Client, NoTls};
use serde_json::json;
use std::{collections::BTreeMap, time::Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let config = args[1].as_str();
    let limit: usize = args[2].parse().unwrap();
    let url = &args[3];
    let store = match config {
        "a" => PostgresStorage::connect_protected(
            url,
            ProtectionKeyring::new(
                "bench-key",
                BTreeMap::from([("bench-key".to_string(), [7u8; 32])]),
            )
            .unwrap(),
        )
        .unwrap(),
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
