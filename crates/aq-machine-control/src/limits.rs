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
    /// PostgreSQL connections: the general pool, the pool of worker control
    /// requests (poll, register, proposal, accept, started, stopped and the
    /// offer assembly a poll starts), and the renewal and presence pools.
    pub connections: usize,
    pub control_connections: usize,
    pub renewal_connections: usize,
    pub presence_connections: usize,
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
        connections: 10,
        control_connections: 16,
        renewal_connections: 4,
        presence_connections: 4,
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
            self.connections,
            self.control_connections,
            self.renewal_connections,
            self.presence_connections,
        ];
        if all.iter().any(|n| !(1..=4096).contains(n)) {
            return Err("each limit must be between 1 and 4096");
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
