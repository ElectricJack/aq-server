//! Read-only views. A contribution marker is historical audit, not a claim that
//! its event remains the current canonical head. Source records are reports.
pub use aq_lifecycle_api::{UsageView, worker_lifecycle as wire};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use wire::{Binding, ContractError, EventRef, ObservationRecord, Text, Validate, ensure};
pub const MAX_PAGE_BYTES: usize = 262144;
pub const MAX_ITEMS: usize = 16;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccountingMark {
    ObservationOnly,
    ContributionRecorded {
        event: EventRef,
        supersedes: Option<EventRef>,
    },
    NotCharged {
        reason: Text,
    },
    Unresolved {
        reason: Text,
    },
}
impl Validate for AccountingMark {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::ObservationOnly => Ok(()),
            Self::ContributionRecorded { event, supersedes } => {
                event.validate()?;
                if let Some(old) = supersedes {
                    old.validate()?;
                    ensure(old.event_id != event.event_id, "self supersession")?;
                }
                Ok(())
            }
            Self::NotCharged { reason } | Self::Unresolved { reason } => {
                reason.validate()?;
                ensure(
                    reason.as_str().len() <= 64
                        && reason
                            .as_str()
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "accounting reason code",
                )
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventView {
    pub record: ObservationRecord,
    pub recorded_at_ms: u64,
    pub accounting: AccountingMark,
}
impl Validate for EventView {
    fn validate(&self) -> Result<(), ContractError> {
        self.record.validate()?;
        self.accounting.validate()?;
        ensure(self.recorded_at_ms > 0, "arrival time")?;
        ensure(
            self.record.body.kind.is_usage()
                != matches!(self.accounting, AccountingMark::ObservationOnly),
            "accounting marker class",
        )?;
        if let AccountingMark::ContributionRecorded { event, .. } = &self.accounting {
            ensure(
                event.event_id == self.record.event_id && event.digest == self.record.body_digest,
                "contribution event",
            )?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsPage {
    pub schema_version: u32,
    pub binding: Binding,
    pub after_sequence: u64,
    pub through_sequence: u64,
    pub events: Vec<EventView>,
    pub next_after: Option<u64>,
    pub complete_for_snapshot: bool,
    pub checked_at_ms: u64,
}
impl Validate for EventsPage {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        ensure(
            self.schema_version == 1 && self.checked_at_ms > 0,
            "view version/time",
        )?;
        ensure(
            self.events.len() <= MAX_ITEMS && self.after_sequence <= self.through_sequence,
            "page bounds",
        )?;
        let mut previous = self.after_sequence;
        for item in &self.events {
            item.validate()?;
            ensure(
                item.record.body.binding == self.binding
                    && item.record.body.sequence > previous
                    && item.record.body.sequence <= self.through_sequence,
                "page scope/order",
            )?;
            previous = item.record.body.sequence;
        }
        ensure(
            if self.complete_for_snapshot {
                self.next_after.is_none()
            } else {
                !self.events.is_empty() && self.next_after == Some(previous)
            },
            "page continuation",
        )?;
        size(self, MAX_PAGE_BYTES)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUsage {
    pub schema_version: u32,
    pub binding: Binding,
    pub revision: u64,
    pub checked_at_ms: u64,
    pub last_recorded_at_ms: Option<u64>,
    pub usage: UsageView,
    /// Order: input, output, cache read, cache write, reasoning. Categories may overlap.
    pub known_category_subtotals: [u64; 5],
    pub canonical_calls: u64,
    pub incomplete_calls: u64,
    pub unresolved_events: u64,
}
impl Validate for SessionUsage {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.usage.validate()?;
        ensure(
            self.schema_version == 1
                && self.checked_at_ms > 0
                && self.last_recorded_at_ms.is_none_or(|v| v > 0),
            "view version/time",
        )?;
        ensure(
            self.incomplete_calls <= self.canonical_calls
                && (self.canonical_calls > 0 || self.known_category_subtotals == [0; 5]),
            "usage projection bounds",
        )?;
        self.validate_known_counters()?;
        size(self, 16384)
    }
}
impl SessionUsage {
    fn validate_known_counters(&self) -> Result<(), ContractError> {
        let c = &self.usage.counters;
        for (i, counter) in [
            &c.input,
            &c.output,
            &c.cache_read,
            &c.cache_write,
            &c.reasoning,
        ]
        .iter()
        .enumerate()
        {
            if let wire::Counter::Known { value } = counter {
                ensure(
                    *value == self.known_category_subtotals[i]
                        && self.canonical_calls > 0
                        && self.incomplete_calls == 0
                        && self.unresolved_events == 0,
                    "known projection consistency",
                )?;
            }
        }
        Ok(())
    }
}
fn size<T: Serialize>(v: &T, max: usize) -> Result<(), ContractError> {
    ensure(
        serde_json::to_vec(v)
            .map_err(|_| ContractError::Invalid("view encoding"))?
            .len()
            <= max,
        "view byte bound",
    )
}
pub fn decode_events(bytes: &[u8]) -> Result<EventsPage, ContractError> {
    ensure(bytes.len() <= MAX_PAGE_BYTES, "view byte bound")?;
    let v: EventsPage =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("events shape"))?;
    v.validate()?;
    Ok(v)
}
pub fn decode_usage(bytes: &[u8]) -> Result<SessionUsage, ContractError> {
    ensure(bytes.len() <= 16384, "view byte bound")?;
    let v: SessionUsage =
        serde_json::from_slice(bytes).map_err(|_| ContractError::Invalid("usage shape"))?;
    v.validate()?;
    Ok(v)
}
pub fn events_schema() -> serde_json::Value {
    let mut s = serde_json::to_value(schemars::schema_for!(EventsPage)).unwrap();
    s["x-max-wire-bytes"] = serde_json::json!(MAX_PAGE_BYTES);
    s["properties"]["schema_version"]["const"] = serde_json::json!(1);
    s["properties"]["events"]["maxItems"] = serde_json::json!(MAX_ITEMS);
    schema_bounds(&mut s);
    s
}
pub fn usage_schema() -> serde_json::Value {
    let mut s = serde_json::to_value(schemars::schema_for!(SessionUsage)).unwrap();
    s["x-max-wire-bytes"] = serde_json::json!(16384);
    s["properties"]["schema_version"]["const"] = serde_json::json!(1);
    schema_bounds(&mut s);
    s
}
fn schema_bounds(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Object(object) => {
            let maximum = match object.get("format").and_then(Value::as_str) {
                Some("uint64") => Some(u64::MAX),
                Some("uint32") => Some(u64::from(u32::MAX)),
                _ => None,
            };
            if let Some(maximum) = maximum {
                object.insert("maximum".into(), maximum.into());
            }
            object.values_mut().for_each(schema_bounds);
        }
        Value::Array(items) => items.iter_mut().for_each(schema_bounds),
        _ => {}
    }
}
