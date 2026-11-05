//! Versioned execution policy for the stall ladder.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub version: u32,
    /// No progress for this long opens a suspected stall (rung 1).
    pub evidence_window_ms: u64,
    /// Time a suspected stall is held before the first nudge.
    pub grace_ms: u64,
    /// Nudges per episode; never reset by progress.
    pub nudge_limit: u32,
    pub nudge_backoff_ms: u64,
    /// Deferred nudges per episode before escalating to notify.
    pub max_deferrals: u32,
    /// Time after the notification before a typed stop.
    pub notify_grace_ms: u64,
    /// A stop not confirmed within this window quarantines.
    pub stop_confirm_ms: u64,
    /// Ladder replacements per lineage; B4 budgets still fence each one.
    pub replacement_ceiling: u32,
    /// An open tool call younger than this is a known long check.
    pub long_check_ms: u64,
    /// Escalation destination recorded on notify and quarantine.
    pub escalation: String,
}

const MINUTE: u64 = 60_000;

impl ExecutionPolicy {
    pub fn v1() -> Self {
        Self {
            version: 1,
            evidence_window_ms: 15 * MINUTE,
            grace_ms: 5 * MINUTE,
            nudge_limit: 2,
            nudge_backoff_ms: 5 * MINUTE,
            max_deferrals: 3,
            notify_grace_ms: 10 * MINUTE,
            stop_confirm_ms: 5 * MINUTE,
            replacement_ceiling: 1,
            long_check_ms: 60 * MINUTE,
            escalation: "owner".into(),
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let positive = [
            self.evidence_window_ms,
            self.grace_ms,
            self.nudge_backoff_ms,
            self.notify_grace_ms,
            self.stop_confirm_ms,
            self.long_check_ms,
        ];
        if self.version == 0 || positive.contains(&0) {
            return Err("stall_policy_invalid_window");
        }
        if self.nudge_limit == 0 || self.nudge_limit > 8 || self.max_deferrals == 0 {
            return Err("stall_policy_invalid_nudges");
        }
        if self.replacement_ceiling > 3 {
            return Err("stall_policy_invalid_replacement_ceiling");
        }
        if self.escalation.trim().is_empty() || self.escalation.len() > 64 {
            return Err("stall_policy_invalid_escalation");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn v1_is_valid_and_bounds_are_enforced() {
        assert_eq!(ExecutionPolicy::v1().validate(), Ok(()));
        let mut p = ExecutionPolicy::v1();
        p.nudge_limit = 0;
        assert!(p.validate().is_err());
        let mut p = ExecutionPolicy::v1();
        p.evidence_window_ms = 0;
        assert!(p.validate().is_err());
        let mut p = ExecutionPolicy::v1();
        p.replacement_ceiling = 4;
        assert!(p.validate().is_err());
    }
}
