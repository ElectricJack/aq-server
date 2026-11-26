//! Concurrency limits of the launch path, read from `[workers.limits]` in the
//! server configuration. They bound work in this process; they are not
//! execution grants. The defaults suit nodes that run several agents: every
//! slot of a few eight-slot nodes can poll, prepare, download and start at
//! once. A limit that is reached answers "retry", as before.
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LaunchLimits {
    /// Offers assembled at once, over all nodes; one per polling slot.
    pub offer_assemblies: usize,
    /// Proposal, accept and refresh requests in progress.
    pub preparations: usize,
    /// Credential material reads for preparation and the initial grant.
    pub material_reads: usize,
    /// Phase execution requests in progress, and delegated renewals.
    pub phase_executions: usize,
    /// Reads of a project's source and of its external binding.
    pub source_reads: usize,
    /// Reads and inspections of retained source exports.
    pub export_reads: usize,
    /// Request classes of the HTTP admission gate.
    pub control_requests: usize,
    pub general_requests: usize,
    pub bulk_transfers: usize,
    pub renewal_requests: usize,
    pub presence_requests: usize,
    pub inventory_requests: usize,
    /// Observation batches and transport reports from nodes, and observation
    /// reads; each has its own lane.
    pub observation_requests: usize,
    pub observation_read_requests: usize,
    /// PostgreSQL connections: the general pool, the pool of worker control
    /// requests (poll, register, proposal, accept, started, stopped and the
    /// offer assembly a poll starts), and the renewal and presence pools.
    pub connections: usize,
    pub control_connections: usize,
    pub renewal_connections: usize,
    pub presence_connections: usize,
    /// Waits in milliseconds. The renewal and presence pools (renewals, worker
    /// inventory and presence reports) wait this long for a connection, and
    /// their statements, row locks and idle transactions are bounded by the
    /// next three. 0 turns one of those three off.
    pub reserved_acquire_timeout_ms: u64,
    pub reserved_statement_timeout_ms: u64,
    pub reserved_lock_timeout_ms: u64,
    pub reserved_idle_timeout_ms: u64,
    /// How long after its provisioning was observed an inventory may still be recorded.
    pub inventory_freshness_ms: u64,
    /// Owner actions that lock node rows (machine stop or drain, owner
    /// availability, revoke) wait this long for each lock; 0 waits without bound.
    pub owner_lock_timeout_ms: u64,
    /// After a revoke commits, it waits for worker requests authorized before
    /// it to finish: this long per try (0: without bound), at most this many tries.
    pub revoke_drain_wait_ms: u64,
    pub revoke_drain_tries: u32,
}
impl LaunchLimits {
    pub const DEFAULT: Self = Self {
        offer_assemblies: 32,
        preparations: 16,
        material_reads: 16,
        phase_executions: 16,
        source_reads: 16,
        export_reads: 16,
        control_requests: 64,
        general_requests: 16,
        bulk_transfers: 16,
        renewal_requests: 32,
        presence_requests: 32,
        inventory_requests: 8,
        observation_requests: 2,
        observation_read_requests: 2,
        connections: 10,
        control_connections: 16,
        renewal_connections: 4,
        presence_connections: 4,
        reserved_acquire_timeout_ms: 250,
        reserved_statement_timeout_ms: 750,
        reserved_lock_timeout_ms: 150,
        reserved_idle_timeout_ms: 2000,
        inventory_freshness_ms: 2000,
        owner_lock_timeout_ms: 1000,
        revoke_drain_wait_ms: 1000,
        revoke_drain_tries: 10,
    };
    /// Every limit admits at least one request: zero would stop the launch path.
    pub fn validate(&self) -> Result<(), &'static str> {
        let all = [
            self.offer_assemblies,
            self.preparations,
            self.material_reads,
            self.phase_executions,
            self.source_reads,
            self.export_reads,
            self.control_requests,
            self.general_requests,
            self.bulk_transfers,
            self.renewal_requests,
            self.presence_requests,
            self.inventory_requests,
            self.observation_requests,
            self.observation_read_requests,
            self.connections,
            self.control_connections,
            self.renewal_connections,
            self.presence_connections,
        ];
        if all.iter().any(|n| !(1..=4096).contains(n)) {
            return Err("each limit must be between 1 and 4096");
        }
        if self.reserved_acquire_timeout_ms == 0
            || self.inventory_freshness_ms == 0
            || self.revoke_drain_tries == 0
        {
            return Err(
                "reserved_acquire_timeout_ms, inventory_freshness_ms and revoke_drain_tries must be at least 1",
            );
        }
        Ok(())
    }
}
impl Default for LaunchLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}
static CONFIGURED: OnceLock<LaunchLimits> = OnceLock::new();
/// Called once at startup, before the first request; a later call changes nothing.
pub fn configure(limits: LaunchLimits) {
    let _ = CONFIGURED.set(limits);
}
/// The configured limits, or the defaults when none were configured.
pub fn current() -> LaunchLimits {
    CONFIGURED.get().copied().unwrap_or_default()
}
/// A wait in milliseconds as a PostgreSQL setting value (`0` turns it off).
pub fn pg_ms(ms: u64) -> String {
    format!("{ms}ms")
}
/// A process-wide gate, sized by its limit when it is first entered.
pub struct Gate {
    permits: OnceLock<std::sync::Arc<tokio::sync::Semaphore>>,
    limit: fn(&LaunchLimits) -> usize,
}
impl Gate {
    pub const fn new(limit: fn(&LaunchLimits) -> usize) -> Self {
        let permits = OnceLock::new();
        Self { permits, limit }
    }
    /// A permit held until dropped, or None when the gate is full.
    pub fn enter(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let size = || std::sync::Arc::new(tokio::sync::Semaphore::new((self.limit)(&current())));
        (self.permits.get_or_init(size).clone())
            .try_acquire_owned()
            .ok()
    }
}
/// The gates of the launch path. Each is its own bound; several share a limit.
pub static PREPARATIONS: Gate = Gate::new(|l| l.preparations);
pub static ACCEPT_MATERIAL: Gate = Gate::new(|l| l.material_reads);
pub static PREPARATION_MATERIAL: Gate = Gate::new(|l| l.material_reads);
pub static GRANT_MATERIAL: Gate = Gate::new(|l| l.material_reads);
pub static MATERIAL_REQUESTS: Gate = Gate::new(|l| l.material_reads);
pub static PHASE_EXECUTIONS: Gate = Gate::new(|l| l.phase_executions);
pub static DELEGATED_RENEWALS: Gate = Gate::new(|l| l.phase_executions);
pub static SOURCE_READS: Gate = Gate::new(|l| l.source_reads);
pub static EXPORT_FILES: Gate = Gate::new(|l| l.export_reads);
pub static EXPORT_INSPECTIONS: Gate = Gate::new(|l| l.export_reads);
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_are_configuration_with_defaults_and_a_gate_honours_its_limit() {
        let parsed: LaunchLimits = serde_json::from_str(r#"{"preparations":3}"#).unwrap();
        assert_eq!(parsed.preparations, 3);
        assert_eq!(
            parsed.offer_assemblies,
            LaunchLimits::DEFAULT.offer_assemblies
        );
        assert!(parsed.validate().is_ok());
        let zero = LaunchLimits {
            bulk_transfers: 0,
            ..parsed
        };
        assert!(zero.validate().is_err());
        assert!(serde_json::from_str::<LaunchLimits>(r#"{"unknown":1}"#).is_err());
        // The process-wide configuration is what a gate is sized by.
        configure(LaunchLimits {
            preparations: 2,
            ..LaunchLimits::DEFAULT
        });
        configure(LaunchLimits::DEFAULT);
        assert_eq!(current().preparations, 2);
        let first = PREPARATIONS.enter().unwrap();
        let _second = PREPARATIONS.enter().unwrap();
        assert!(PREPARATIONS.enter().is_none());
        drop(first);
        assert!(PREPARATIONS.enter().is_some());
        // Another gate has its own permits.
        assert!(PHASE_EXECUTIONS.enter().is_some());
    }
}
