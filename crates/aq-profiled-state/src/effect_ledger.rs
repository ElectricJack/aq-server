//! Acceptance classification of a stopped attempt’s validated Graph and mailbox ledger.
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

fn read_only(profile: &Value) -> bool {
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
fn messaging_enabled(profile: &Value) -> bool {
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
}
