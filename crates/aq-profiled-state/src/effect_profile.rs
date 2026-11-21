//! Classification of a stopped attempt's graph access profile and effect ledger.
//! Pure functions over the resolved profile JSON; the ledger reads stay in aq-server.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerDisposition {
    Settled,
    NeedsReconciliation,
    UnsupportedMessaging,
    UnsupportedProfile,
}
impl LedgerDisposition {
    pub fn name(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::NeedsReconciliation => "needs_reconciliation",
            Self::UnsupportedMessaging => "unsupported_messaging",
            Self::UnsupportedProfile => "unsupported_profile",
        }
    }
}

/// Every capability of the profile is a read.
pub fn read_only(profile: &Value) -> bool {
    const READS: &[&str] = &[
        "read.get",
        "read.revision",
        "read.history",
        "read.search",
        "read.context",
        "read.relationships",
    ];
    profile["capabilities"].as_array().is_some_and(|caps| {
        caps.iter()
            .all(|cap| cap.as_str().is_some_and(|c| READS.contains(&c)))
    })
}
/// The profile names a `message.*` capability.
pub fn messaging_enabled(profile: &Value) -> bool {
    profile["capabilities"].as_array().is_some_and(|caps| {
        caps.iter()
            .any(|cap| cap.as_str().is_some_and(|c| c.starts_with("message.")))
    })
}
/// The acceptance verdict of a complete ledger whose operations are all final (`terminal`) or
/// not: only a read-only profile that sent no mail is `Settled`.
pub fn verdict(mailed: bool, profile: &Value, terminal: bool) -> LedgerDisposition {
    if mailed || messaging_enabled(profile) {
        LedgerDisposition::UnsupportedMessaging
    } else if !read_only(profile) {
        LedgerDisposition::UnsupportedProfile
    } else if !terminal {
        LedgerDisposition::NeedsReconciliation
    } else {
        LedgerDisposition::Settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    /// Acceptance stays the read-only pilot (2026-11-21: a developer profile with
    /// `message.send` and `write.*` could never be replaced; quiescence is decided by the
    /// ledger's `terminal` flag, not by this verdict).
    #[test]
    fn messaging_and_write_ledgers_refuse_acceptance() {
        let developer = json!({"capabilities":["read.get","write.finding","message.send"]});
        let writer = json!({"capabilities":["read.get","write.task_content"]});
        let reader = json!({"capabilities":["read.get"]});
        use LedgerDisposition::*;
        for (mailed, profile, terminal, expected) in [
            (false, &developer, true, UnsupportedMessaging),
            (true, &reader, true, UnsupportedMessaging),
            (false, &writer, true, UnsupportedProfile),
            (false, &reader, true, Settled),
            (false, &reader, false, NeedsReconciliation),
            (false, &developer, false, UnsupportedMessaging),
        ] {
            assert_eq!(
                verdict(mailed, profile, terminal),
                expected,
                "{profile} mailed={mailed} terminal={terminal}"
            );
        }
    }
    #[test]
    fn read_only_pilot_closes_messaging_and_unknown_future_effects() {
        assert!(read_only(
            &json!({"capabilities":["read.get","read.context"]})
        ));
        for c in [
            "write.finding",
            "write.task_content",
            "message.send",
            "message.ack",
            "future.publish",
        ] {
            assert!(!read_only(&json!({"capabilities":["read.get",c]})));
        }
        assert!(messaging_enabled(&json!({"capabilities":["message.read"]})));
        assert!(!messaging_enabled(&json!({"capabilities":["read.get"]})));
    }
    #[test]
    fn disposition_names_are_stable() {
        assert_eq!(LedgerDisposition::Settled.name(), "settled");
        assert_eq!(
            LedgerDisposition::NeedsReconciliation.name(),
            "needs_reconciliation"
        );
    }
}
