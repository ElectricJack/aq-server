//! Bounded, value-free operational telemetry. Process counters reset on engine
//! construction; hosts should export them to their metrics backend. State gauges
//! and retained-receipt timings survive restart, but cover only retained history.
use crate::{IntentStatus, Pending, Result, RunStatus, State};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Duration,
};

/// Fixed noncumulative buckets in microseconds, followed by an overflow bucket.
pub const LATENCY_BUCKETS_US: [u64; 8] =
    [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000];
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Distribution {
    pub count: u64,
    pub total_us: u64,
    pub max_us: u64,
    pub buckets: [u64; 9],
}
impl Distribution {
    pub fn observe_us(&mut self, value: u64) {
        self.count = self.count.saturating_add(1);
        self.total_us = self.total_us.saturating_add(value);
        self.max_us = self.max_us.max(value);
        let bucket = LATENCY_BUCKETS_US
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(8);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
    }
    fn observe_ms(&mut self, start: i64, end: i64) {
        if end >= start {
            self.observe_us((end.saturating_sub(start) as u64).saturating_mul(1_000));
        }
    }
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProcessMetrics {
    /// Initial admission to first successful visit; excludes later wake cycles.
    pub queue_latency: Distribution,
    pub store_load: Distribution,
    pub transition: Distribution,
    pub store_commit: Distribution,
    pub host_call: Distribution,
    pub admission_attempts: u64,
    pub admission_refusals: u64,
    pub duplicate_events: u64,
}
#[derive(Debug, Clone, Copy)]
pub enum Timing {
    Load,
    Transition,
    Commit,
    Host,
}
#[derive(Debug, Default)]
pub struct Metrics(Mutex<ProcessMetrics>);
impl Metrics {
    // A failed metrics consumer must not affect workflow correctness.
    pub fn snapshot(&self) -> ProcessMetrics {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
    pub fn record_time(&self, kind: Timing, elapsed: Duration) {
        let mut data = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        let histogram = match kind {
            Timing::Load => &mut data.store_load,
            Timing::Transition => &mut data.transition,
            Timing::Commit => &mut data.store_commit,
            Timing::Host => &mut data.host_call,
        };
        histogram.observe_us(elapsed.as_micros().min(u64::MAX as u128) as u64);
    }
    pub fn record_queue(&self, elapsed_ms: u64) {
        self.0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .queue_latency
            .observe_us(elapsed_ms.saturating_mul(1_000));
    }
    pub fn admission<T>(&self, result: &Result<T>, duplicate: bool) {
        let mut data = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        data.admission_attempts = data.admission_attempts.saturating_add(1);
        if result.is_err() {
            data.admission_refusals = data.admission_refusals.saturating_add(1);
        }
        if duplicate {
            data.duplicate_events = data.duplicate_events.saturating_add(1);
        }
    }
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StateMetrics {
    pub retained_runs: usize,
    pub queued_roots: usize,
    pub active_runs: usize,
    pub open_waits: usize,
    pub outstanding_commands: usize,
    pub uncertain_commands: usize,
    pub maximum_child_depth: u32,
    pub retained_bytes: usize,
    pub retained_events: usize,
    pub retained_receipts: usize,
    pub queued_root_age: Distribution,
    pub recovery_age: Distribution,
    pub command_admission_latency: Distribution,
    pub command_completion_latency: Distribution,
}
impl StateMetrics {
    /// An explicit diagnostic scan, never used by the ready-work scheduler.
    pub fn from_state(state: &State, now_ms: i64) -> Result<Self> {
        let mut out = Self {
            retained_runs: state.runs.len(),
            active_runs: state
                .runs
                .values()
                .filter(|run| !run.status.terminal())
                .count(),
            open_waits: state
                .waits
                .values()
                .filter(|wait| wait.winner.is_none() && !wait.expired && state.runs.get(&wait.run).is_some_and(|run| !run.status.terminal() && matches!(&run.pending, Some(Pending::Event { wait: id }) if id == &wait.id)))
                .count(),
            maximum_child_depth: state.runs.values().map(|run| run.depth).max().unwrap_or(0),
            retained_bytes: aq_playbooks_model::canonical_bytes(state)?.len(),
            retained_events: state.events.len(),
            retained_receipts: state.receipts.len(),
            ..Self::default()
        };
        for run in state
            .runs
            .values()
            .filter(|run| run.parent.is_none() && run.status == RunStatus::Admitted)
        {
            out.queued_roots += 1;
            out.queued_root_age.observe_ms(run.admitted_at_ms, now_ms);
        }
        let mut prepared = BTreeMap::new();
        let mut dispatched = BTreeMap::new();
        let mut accepted = BTreeSet::new();
        let mut completed = BTreeSet::new();
        for receipt in &state.receipts {
            let Some(operation) = receipt.detail.get("operation").and_then(|v| v.as_str()) else {
                continue;
            };
            match receipt.kind.as_str() {
                "intent" => {
                    prepared.entry(operation).or_insert(receipt.at_ms);
                }
                "dispatch" => {
                    dispatched.entry(operation).or_insert(receipt.at_ms);
                }
                "operation_accepted" if accepted.insert(operation) => {
                    if let Some(start) = dispatched.get(operation) {
                        out.command_admission_latency
                            .observe_ms(*start, receipt.at_ms);
                    }
                }
                "command_result" if completed.insert(operation) => {
                    if let Some(start) = dispatched.get(operation) {
                        out.command_completion_latency
                            .observe_ms(*start, receipt.at_ms);
                    }
                }
                _ => {}
            }
        }
        for intent in state.intents.values() {
            if matches!(
                intent.status,
                IntentStatus::Prepared
                    | IntentStatus::Dispatched
                    | IntentStatus::Accepted
                    | IntentStatus::Unknown
            ) {
                out.outstanding_commands += 1;
            }
            if matches!(
                intent.status,
                IntentStatus::Dispatched | IntentStatus::Unknown
            ) {
                out.uncertain_commands += 1;
                if let Some(start) = dispatched
                    .get(intent.id.as_str())
                    .or_else(|| prepared.get(intent.id.as_str()))
                {
                    out.recovery_age.observe_ms(*start, now_ms);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, MemoryStorage};
    #[test]
    fn refusal_and_store_timings_are_observable_without_payloads() {
        let engine = Engine::new(MemoryStorage::default(), "metrics", "owner").unwrap();
        let result = engine.admit(
            "missing-secret-binding",
            "entry",
            serde_json::json!({"secret":"do-not-export"}),
            crate::Authority {
                actor: "a".into(),
                scope: "s".into(),
                capabilities: Default::default(),
            },
            0,
        );
        assert!(result.is_err());
        let data = engine.metrics.snapshot();
        assert_eq!(data.admission_attempts, 1);
        assert_eq!(data.admission_refusals, 1);
        assert!(data.store_load.count >= 2);
        assert!(data.store_commit.count >= 1);
        let serialized = serde_json::to_string(&data).unwrap();
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("do-not-export"));
        assert_eq!(
            StateMetrics::from_state(&engine.snapshot().unwrap(), 10)
                .unwrap()
                .retained_runs,
            0
        );
    }
    #[test]
    fn repeated_owner_readback_does_not_skew_command_latencies() {
        let mut state = State::default();
        for (index, (kind, at_ms)) in [
            ("intent", 0),
            ("dispatch", 10),
            ("operation_accepted", 20),
            ("operation_accepted", 40),
            ("command_result", 50),
        ]
        .into_iter()
        .enumerate()
        {
            state.receipts.push(crate::Receipt {
                sequence: index as u64,
                run: "r".into(),
                step: "s".into(),
                at_ms,
                kind: kind.into(),
                detail: serde_json::json!({"operation":"op"}),
            });
        }
        let data = StateMetrics::from_state(&state, 60).unwrap();
        assert_eq!(data.command_admission_latency.count, 1);
        assert_eq!(data.command_admission_latency.total_us, 10_000);
        assert_eq!(data.command_completion_latency.total_us, 40_000);
    }
    #[test]
    fn distribution_has_bounded_buckets_and_saturating_totals() {
        let mut data = Distribution::default();
        data.observe_us(10);
        data.observe_us(u64::MAX);
        assert_eq!(data.count, 2);
        assert_eq!(data.total_us, u64::MAX);
        assert_eq!(data.buckets[1], 1);
        assert_eq!(data.buckets[8], 1);
    }
}
