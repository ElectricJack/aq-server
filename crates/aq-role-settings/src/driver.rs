//! The playbook driver's limits: what one drive quantum may admit, collect,
//! deliver and tick. Part of the store's limits Settings value
//! (`/api/settings/profiles/limits`, field `driver`), read at every quantum,
//! so a change applies at the next quantum without a restart. Settings
//! stored before this value existed read as the defaults.
use serde::{Deserialize, Serialize};

/// The largest `events` Settings accepts: the collectors of the driver take
/// no more per call.
pub const EVENTS_MAX: u32 = 16;
/// The largest `admissions` Settings accepts.
pub const ADMISSIONS_MAX: u32 = 16;
/// The largest `outbox_events` Settings accepts.
pub const OUTBOX_EVENTS_MAX: u32 = 64;
/// The largest `delivery_operations` Settings accepts.
pub const DELIVERY_OPERATIONS_MAX: u32 = 8;

/// Every per-quantum bound of the playbook driver. A quantum that reaches a
/// bound with work left is followed at once by the next one, so a bound sets
/// the size of a step, not how fast the backlog drains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverLimits {
    /// Keyed admissions delivered to the engine per quantum (1 to 16).
    pub admissions: u32,
    /// Events of each internal kind (decision, graph, settlement, schedule,
    /// work completion) and expiries collected per quantum (1 to 16).
    pub events: u32,
    /// Outbox events delivered to the engine per quantum (1 to 64).
    pub outbox_events: u32,
    /// Waiting tasks asked about (`aq.work.route_needed`) per quantum
    /// (1 to 1000).
    pub routing_asks: u32,
    /// Results and base changes announced per quantum (1 to 1000).
    pub announcements: u32,
    /// Delivery operations collected per quantum (1 to 8).
    pub delivery_operations: u32,
    /// Run transitions of the one tick of a quantum, for the whole server
    /// (1 to 4096). Every command a transition creates is handed to the host
    /// before the next transition, so its deadline covers the host call.
    pub tick_transitions: u32,
    /// The longest a tick keeps starting transitions, in milliseconds
    /// (1000 to 120000). The work left waits for the next quantum.
    pub tick_budget_ms: u64,
    /// How many times a routing run that ended in an error is retired and
    /// the task asked about again, for one task version (0 to 20). Zero
    /// leaves such a task to the owner.
    pub route_error_retries: u32,
}
impl Default for DriverLimits {
    fn default() -> Self {
        Self {
            admissions: 8,
            events: 8,
            outbox_events: 32,
            routing_asks: 32,
            announcements: 16,
            delivery_operations: 8,
            tick_transitions: 256,
            tick_budget_ms: 15_000,
            route_error_retries: 3,
        }
    }
}
impl DriverLimits {
    /// Every value in its range; the message names the first that is not.
    pub fn validate(&self) -> Result<(), String> {
        let fields: [(&str, u64, u64, u64); 9] = [
            (
                "admissions",
                self.admissions.into(),
                1,
                ADMISSIONS_MAX.into(),
            ),
            ("events", self.events.into(), 1, EVENTS_MAX.into()),
            (
                "outbox_events",
                self.outbox_events.into(),
                1,
                OUTBOX_EVENTS_MAX.into(),
            ),
            ("routing_asks", self.routing_asks.into(), 1, 1000),
            ("announcements", self.announcements.into(), 1, 1000),
            (
                "delivery_operations",
                self.delivery_operations.into(),
                1,
                DELIVERY_OPERATIONS_MAX.into(),
            ),
            ("tick_transitions", self.tick_transitions.into(), 1, 4096),
            ("tick_budget_ms", self.tick_budget_ms, 1000, 120_000),
            (
                "route_error_retries",
                self.route_error_retries.into(),
                0,
                20,
            ),
        ];
        for (name, value, least, most) in fields {
            if !(least..=most).contains(&value) {
                return Err(format!(
                    "driver {name} is {value}; it must be between {least} and {most}"
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
    fn defaults_are_valid_and_each_range_is_checked() {
        assert_eq!(DriverLimits::default().validate(), Ok(()));
        let cases: [fn(&mut DriverLimits); 9] = [
            |l| l.admissions = 0,
            |l| l.events = EVENTS_MAX + 1,
            |l| l.outbox_events = OUTBOX_EVENTS_MAX + 1,
            |l| l.routing_asks = 0,
            |l| l.announcements = 1001,
            |l| l.delivery_operations = DELIVERY_OPERATIONS_MAX + 1,
            |l| l.tick_transitions = 0,
            |l| l.tick_budget_ms = 999,
            |l| l.route_error_retries = 21,
        ];
        for change in cases {
            let mut limits = DriverLimits::default();
            change(&mut limits);
            assert!(limits.validate().is_err(), "{limits:?}");
        }
        let off = DriverLimits {
            route_error_retries: 0,
            ..DriverLimits::default()
        };
        assert_eq!(off.validate(), Ok(()));
    }

    #[test]
    fn settings_stored_without_driver_limits_read_as_the_defaults() {
        let stored = serde_json::json!({
            "documents": crate::StoreLimits::default().documents,
            "snapshots_kept": 5,
            "operations_kept": 6,
        });
        let read: crate::StoreLimits = serde_json::from_value(stored).unwrap();
        assert_eq!(read.driver, DriverLimits::default());
        assert_eq!(read.snapshots_kept, 5);
    }
}
