//! `[playbooks.engine_retention]`: the engine's own rolling sizes and replay
//! windows (engine spec 0003). The defaults are the engine's; the owner
//! thread writes them into the namespace's retention policy at open, in one
//! commit, only when they differ from what the namespace stores.
use aq_playbooks_runner::RetentionPolicy;
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineRetention {
    /// How long a delivered event is remembered for duplicate detection (an
    /// event contract that declares a longer `retention_ms` wins).
    pub event_replay_ms: u64,
    /// How long a keyed admission receipt is remembered.
    pub admission_replay_ms: u64,
    /// How many remembered events and receipts that no live work holds.
    pub replay_keep: usize,
    /// How many finished runs history keeps (`0` keeps none).
    pub history_keep: u64,
    /// How long administrative receipts are kept.
    pub audit_ms: u64,
}
impl Default for EngineRetention {
    fn default() -> Self {
        let p = RetentionPolicy::default();
        Self {
            event_replay_ms: p.event_replay_ms,
            admission_replay_ms: p.admission_replay_ms,
            replay_keep: p.replay_keep,
            history_keep: p.history_keep,
            audit_ms: p.audit_ms,
        }
    }
}
impl EngineRetention {
    /// What the engine itself requires: positive windows and `replay_keep`.
    /// No upper bound.
    pub fn valid(&self) -> bool {
        self.event_replay_ms > 0
            && self.admission_replay_ms > 0
            && self.audit_ms > 0
            && self.replay_keep > 0
    }
    /// The stored policy with these values, or `None` when it already has
    /// them. The per-commit bounds keep their stored values.
    pub fn changed(&self, stored: &RetentionPolicy) -> Option<RetentionPolicy> {
        let next = RetentionPolicy {
            event_replay_ms: self.event_replay_ms,
            admission_replay_ms: self.admission_replay_ms,
            replay_keep: self.replay_keep,
            history_keep: self.history_keep,
            audit_ms: self.audit_ms,
            ..stored.clone()
        };
        let same = |a: &RetentionPolicy, b: &RetentionPolicy| {
            (
                a.event_replay_ms,
                a.admission_replay_ms,
                a.replay_keep,
                a.history_keep,
                a.audit_ms,
            ) == (
                b.event_replay_ms,
                b.admission_replay_ms,
                b.replay_keep,
                b.history_keep,
                b.audit_ms,
            )
        };
        (!same(&next, stored)).then_some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_are_the_engines_and_leave_a_new_namespace_untouched() {
        let parsed: EngineRetention = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, EngineRetention::default());
        assert!(parsed.valid());
        assert!(parsed.changed(&RetentionPolicy::default()).is_none());
        let zero: EngineRetention = serde_json::from_str(r#"{"replay_keep":0}"#).unwrap();
        assert!(!zero.valid());
    }
    #[test]
    fn a_configured_window_changes_only_its_own_values() {
        let set: EngineRetention =
            serde_json::from_str(r#"{"event_replay_ms":600000,"admission_replay_ms":600000}"#)
                .unwrap();
        let stored = RetentionPolicy {
            gc_page_limit: 7,
            ..RetentionPolicy::default()
        };
        let next = set.changed(&stored).unwrap();
        assert_eq!(
            (next.event_replay_ms, next.admission_replay_ms),
            (600_000, 600_000)
        );
        assert_eq!((next.gc_page_limit, next.history_keep), (7, 10_000));
        assert!(set.changed(&next).is_none());
    }
}
