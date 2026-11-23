//! The delivery limits (spec 0016 §4): a Settings value, stored beside the
//! delivery document. Every number is the owner's to set; the ranges below
//! only keep a typing mistake from storing a value no sync or transfer could
//! meaningfully use.
use serde::{Deserialize, Serialize};

const SECOND_MS: u64 = 1000;
const DAY_MS: u64 = 24 * 60 * 60 * SECOND_MS;
const WEEK_MS: u64 = 7 * DAY_MS;
/// The largest `max_push_bytes` Settings accepts: 64 GiB.
const MAX_PUSH_BYTES_CEILING: u64 = 64 * 1024 * 1024 * 1024;
/// The largest `check_log_bytes` Settings accepts: 1 MiB.
const CHECK_LOG_BYTES_CEILING: u64 = 1024 * 1024;

/// What the delivery path waits for, repeats and carries. Fields are in
/// milliseconds unless the name says bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryLimits {
    /// How often central aq-git syncs each bound project's default branch.
    pub main_sync_interval_ms: u64,
    /// How old the observation of `main` may be before an offer syncs it.
    pub main_freshness_ms: u64,
    /// How soon a playbook decision on the same result may be taken again.
    pub decision_repeat_ms: u64,
    /// The longest a delivery run waits for the checks of a pull request.
    pub longest_check_wait_ms: u64,
    /// The longest one git fetch or push to or from central aq-git may take.
    pub git_transfer_timeout_ms: u64,
    /// The most bytes one push to central aq-git may carry.
    pub max_push_bytes: u64,
    /// Whether a dependent task waits until each completed blocker that pushed
    /// a result has that result merged into the followed branch. A blocker
    /// that pushed nothing is satisfied at once. Settings stored before this
    /// field existed read as `true`.
    #[serde(default = "require_landed_results")]
    pub require_landed_results: bool,
    /// The most bytes of failed-job log the host keeps for a failed required
    /// check, read through the GitHub App and redacted.
    /// Settings stored before this field existed read as the default.
    #[serde(default = "check_log_bytes")]
    pub check_log_bytes: u64,
}
fn require_landed_results() -> bool {
    true
}
fn check_log_bytes() -> u64 {
    16_384
}
impl Default for DeliveryLimits {
    fn default() -> Self {
        Self {
            main_sync_interval_ms: 60_000,
            main_freshness_ms: 30_000,
            decision_repeat_ms: 120_000,
            longest_check_wait_ms: 3_600_000,
            git_transfer_timeout_ms: 600_000,
            max_push_bytes: 268_435_456,
            require_landed_results: true,
            check_log_bytes: check_log_bytes(),
        }
    }
}
impl DeliveryLimits {
    /// Every value in its range; the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        let fields: [(&str, u64, u64, u64); 7] = [
            (
                "main_sync_interval_ms",
                self.main_sync_interval_ms,
                SECOND_MS,
                DAY_MS,
            ),
            (
                "main_freshness_ms",
                self.main_freshness_ms,
                SECOND_MS,
                DAY_MS,
            ),
            (
                "decision_repeat_ms",
                self.decision_repeat_ms,
                SECOND_MS,
                DAY_MS,
            ),
            (
                "longest_check_wait_ms",
                self.longest_check_wait_ms,
                SECOND_MS,
                WEEK_MS,
            ),
            (
                "git_transfer_timeout_ms",
                self.git_transfer_timeout_ms,
                SECOND_MS,
                DAY_MS,
            ),
            (
                "max_push_bytes",
                self.max_push_bytes,
                1,
                MAX_PUSH_BYTES_CEILING,
            ),
            (
                "check_log_bytes",
                self.check_log_bytes,
                1,
                CHECK_LOG_BYTES_CEILING,
            ),
        ];
        for (name, value, least, most) in fields {
            if !(least..=most).contains(&value) {
                return Err(format!(
                    "{name} is {value}; it must be between {least} and {most}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_specified_numbers_and_valid() {
        let d = DeliveryLimits::default();
        assert_eq!(
            (
                d.main_sync_interval_ms,
                d.main_freshness_ms,
                d.decision_repeat_ms,
                d.longest_check_wait_ms,
                d.git_transfer_timeout_ms,
                d.max_push_bytes
            ),
            (60_000, 30_000, 120_000, 3_600_000, 600_000, 268_435_456)
        );
        assert!(d.require_landed_results);
        assert_eq!(d.check_log_bytes, 16_384);
        assert_eq!(d.validate(), Ok(()));
    }
    #[test]
    fn each_field_is_refused_at_zero_and_over_its_maximum() {
        type Set = fn(&mut DeliveryLimits, u64);
        let setters: [(&str, Set, u64); 7] = [
            (
                "main_sync_interval_ms",
                |l, v| l.main_sync_interval_ms = v,
                DAY_MS,
            ),
            ("main_freshness_ms", |l, v| l.main_freshness_ms = v, DAY_MS),
            (
                "decision_repeat_ms",
                |l, v| l.decision_repeat_ms = v,
                DAY_MS,
            ),
            (
                "longest_check_wait_ms",
                |l, v| l.longest_check_wait_ms = v,
                WEEK_MS,
            ),
            (
                "git_transfer_timeout_ms",
                |l, v| l.git_transfer_timeout_ms = v,
                DAY_MS,
            ),
            (
                "max_push_bytes",
                |l, v| l.max_push_bytes = v,
                MAX_PUSH_BYTES_CEILING,
            ),
            (
                "check_log_bytes",
                |l, v| l.check_log_bytes = v,
                CHECK_LOG_BYTES_CEILING,
            ),
        ];
        for (name, set, most) in setters {
            for bad in [0, most + 1, u64::MAX] {
                let mut limits = DeliveryLimits::default();
                set(&mut limits, bad);
                let message = limits.validate().unwrap_err();
                assert!(message.starts_with(name), "{name} at {bad}: {message}");
            }
            let mut limits = DeliveryLimits::default();
            set(&mut limits, most);
            assert_eq!(limits.validate(), Ok(()), "{name} at its maximum");
        }
    }
    #[test]
    fn limits_stored_before_landed_results_existed_require_them() {
        let mut stored = serde_json::to_value(DeliveryLimits::default()).unwrap();
        stored
            .as_object_mut()
            .unwrap()
            .remove("require_landed_results");
        let read: DeliveryLimits = serde_json::from_value(stored).unwrap();
        assert!(read.require_landed_results);
    }
    #[test]
    fn check_log_bytes_is_bounded_and_defaults_when_not_stored() {
        let mut limits = DeliveryLimits::default();
        for ok in [1, CHECK_LOG_BYTES_CEILING] {
            limits.check_log_bytes = ok;
            assert_eq!(limits.validate(), Ok(()), "{ok}");
        }
        for bad in [0, CHECK_LOG_BYTES_CEILING + 1, u64::MAX] {
            limits.check_log_bytes = bad;
            assert!(
                limits
                    .validate()
                    .unwrap_err()
                    .starts_with("check_log_bytes")
            );
        }
        let mut stored = serde_json::to_value(DeliveryLimits::default()).unwrap();
        stored.as_object_mut().unwrap().remove("check_log_bytes");
        let read: DeliveryLimits = serde_json::from_value(stored).unwrap();
        assert_eq!(read.check_log_bytes, 16_384);
    }
    #[test]
    fn unknown_and_missing_fields_are_refused() {
        let whole = serde_json::to_value(DeliveryLimits::default()).unwrap();
        let mut extra = whole.clone();
        extra["surprise"] = 1.into();
        assert!(serde_json::from_value::<DeliveryLimits>(extra).is_err());
        let mut short = whole;
        short.as_object_mut().unwrap().remove("max_push_bytes");
        assert!(serde_json::from_value::<DeliveryLimits>(short).is_err());
    }
}
