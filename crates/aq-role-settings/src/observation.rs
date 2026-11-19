//! The node observation limits: a Settings value (`observation-limits`),
//! stored beside the other Settings values and read at every use, so a
//! change applies to the next enrollment or batch without a restart. The
//! defaults are the numbers that were constants before. Every number is the
//! owner's to set; the ranges only keep a typing mistake from storing a
//! value that would refuse every batch or overflow the counters.
use serde::{Deserialize, Serialize};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// The largest count or byte total Settings accepts: the counters are BIGINT.
const MOST: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationLimits {
    /// Sources one node may have enrolled at a time. A source stops counting
    /// when its attempt ends or a newer generation replaces it.
    pub max_sources_per_node: u64,
    /// Events one node may have retained (row retention returns them).
    pub max_events_per_node: u64,
    /// Bytes of observation history one node may have retained.
    pub max_retained_bytes_per_node: u64,
    /// Canonical calls one usage or budget read may summarize.
    pub max_calls_per_summary: u64,
    /// How long after its attempt ended a source's batches are still accepted
    /// (the worker's final drain), in milliseconds.
    pub final_drain_grace_ms: u64,
}
impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            max_sources_per_node: 256,
            max_events_per_node: 65_536,
            max_retained_bytes_per_node: 134_217_728,
            max_calls_per_summary: 65_536,
            final_drain_grace_ms: 300_000,
        }
    }
}
impl ObservationLimits {
    fn fields(&self) -> [(&'static str, u64, u64); 5] {
        [
            ("max_sources_per_node", self.max_sources_per_node, MOST),
            ("max_events_per_node", self.max_events_per_node, MOST),
            (
                "max_retained_bytes_per_node",
                self.max_retained_bytes_per_node,
                MOST,
            ),
            ("max_calls_per_summary", self.max_calls_per_summary, MOST),
            ("final_drain_grace_ms", self.final_drain_grace_ms, DAY_MS),
        ]
    }
    /// Every value in its range; the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value, most) in self.fields() {
            if !(1..=most).contains(&value) {
                return Err(format!(
                    "{name} is {value}; it must be between 1 and {most}"
                ));
            }
        }
        Ok(())
    }
    /// Whether a node with `sources` in use and `retained` bytes may enroll
    /// one more source of `bytes`; `Err` is the refusal the caller reports.
    pub fn admit_source(&self, sources: i64, retained: i64, bytes: i64) -> Result<(), String> {
        admit(
            "sources",
            "max_sources_per_node",
            sources,
            1,
            self.max_sources_per_node,
        )?;
        self.admit_bytes(retained, bytes)
    }
    /// Whether a node with `events` and `retained` bytes may record a batch
    /// of `adding` events and `bytes`.
    pub fn admit_events(
        &self,
        events: i64,
        adding: i64,
        retained: i64,
        bytes: i64,
    ) -> Result<(), String> {
        admit(
            "events",
            "max_events_per_node",
            events,
            adding,
            self.max_events_per_node,
        )?;
        self.admit_bytes(retained, bytes)
    }
    fn admit_bytes(&self, retained: i64, bytes: i64) -> Result<(), String> {
        let limit = self.max_retained_bytes_per_node;
        admit(
            "retained bytes",
            "max_retained_bytes_per_node",
            retained,
            bytes,
            limit,
        )
    }
    /// The refusal of a read that would summarize more calls than allowed.
    pub fn too_many_calls(&self) -> String {
        format!(
            "more than {} calls to summarize (Settings, observation-limits, max_calls_per_summary)",
            self.max_calls_per_summary
        )
    }
}
/// The refusal names the use, the limit and the Setting that holds it.
fn admit(what: &str, setting: &str, used: i64, adding: i64, limit: u64) -> Result<(), String> {
    if used
        .checked_add(adding)
        .is_some_and(|total| u64::try_from(total).is_ok_and(|total| total <= limit))
    {
        return Ok(());
    }
    Err(format!(
        "node observation limit reached: {used} {what} in use and {adding} more would pass the limit of {limit} (Settings, observation-limits, {setting})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_former_constants_and_valid() {
        let d = ObservationLimits::default();
        assert_eq!(
            d.fields().map(|(_, value, _)| value),
            [256, 65_536, 128 * 1024 * 1024, 65_536, 300_000]
        );
        assert_eq!(d.validate(), Ok(()));
    }
    #[test]
    fn each_field_is_refused_at_zero_and_over_its_maximum() {
        let whole = serde_json::to_value(ObservationLimits::default()).unwrap();
        for (name, _, most) in ObservationLimits::default().fields() {
            for (value, valid) in [(0, false), (1, true), (most, true), (most + 1, false)] {
                let mut changed = whole.clone();
                changed[name] = value.into();
                let limits: ObservationLimits = serde_json::from_value(changed).unwrap();
                assert_eq!(limits.validate().is_ok(), valid, "{name} at {value}");
            }
        }
        let mut extra = whole.clone();
        extra["surprise"] = 1.into();
        assert!(serde_json::from_value::<ObservationLimits>(extra).is_err());
    }
    #[test]
    fn a_refusal_names_the_use_the_limit_and_the_setting() {
        let limits = ObservationLimits {
            max_sources_per_node: 2,
            max_events_per_node: 4,
            max_retained_bytes_per_node: 100,
            ..Default::default()
        };
        assert_eq!(limits.admit_source(1, 90, 10), Ok(()));
        assert_eq!(limits.admit_events(3, 1, 0, 100), Ok(()));
        for (refusal, parts) in [
            (
                limits.admit_source(2, 0, 1),
                ["2 sources", "limit of 2", "max_sources_per_node"],
            ),
            (
                limits.admit_source(0, 90, 11),
                [
                    "90 retained bytes",
                    "limit of 100",
                    "max_retained_bytes_per_node",
                ],
            ),
            (
                limits.admit_events(4, 1, 0, 1),
                ["4 events", "limit of 4", "max_events_per_node"],
            ),
            (
                limits.admit_events(0, 1, i64::MAX, 1),
                [
                    "retained bytes",
                    "limit of 100",
                    "max_retained_bytes_per_node",
                ],
            ),
        ] {
            let refusal = refusal.unwrap_err();
            for part in parts {
                assert!(refusal.contains(part), "{refusal}");
            }
        }
        assert!(limits.too_many_calls().contains("max_calls_per_summary"));
    }
}
