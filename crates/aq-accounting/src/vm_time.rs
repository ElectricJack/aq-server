//! VM running-time estimates, separate from provider token/subscription costs.
//! Prices are pinned per sandbox; stopped/archived time is never charged.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Small,
    Default,
    Large,
    Xlarge,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeRate {
    pub size: Size,
    pub microunits_per_sandbox_hour: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    pub revision: String,
    pub currency: String,
    pub rates: Vec<SizeRate>,
}
impl RateCard {
    /// Architecture 0005 §5.2, verified amendment facts of 2026-11-03.
    pub fn boat() -> Self {
        Self {
            revision: "boat-2026-11-03".into(),
            currency: "USD".into(),
            rates: [
                (Size::Small, 18_000),
                (Size::Default, 36_000),
                (Size::Large, 72_000),
                (Size::Xlarge, 200_000),
            ]
            .into_iter()
            .map(|(size, microunits_per_sandbox_hour)| SizeRate {
                size,
                microunits_per_sandbox_hour,
            })
            .collect(),
        }
    }
    pub fn cost(&self, size: Size, duration_ms: u64) -> Result<u64> {
        if self.revision.is_empty()
            || self.revision.len() > 128
            || self.currency.len() != 3
            || !self.currency.bytes().all(|c| c.is_ascii_uppercase())
            || self.rates.len() != 4
            || [Size::Small, Size::Default, Size::Large, Size::Xlarge]
                .iter()
                .any(|s| self.rates.iter().filter(|r| r.size == *s).count() != 1)
        {
            return Err(Error::Invalid);
        }
        let rate = self
            .rates
            .iter()
            .find(|r| r.size == size)
            .ok_or(Error::Invalid)?;
        // Integer arithmetic, rounding up once per sandbox/window to a microunit.
        let weighted = u128::from(duration_ms) * u128::from(rate.microunits_per_sandbox_hour);
        u64::try_from(weighted.div_ceil(3_600_000)).map_err(|_| Error::Overflow)
    }
}

/// One sandbox's running interval. Replays keep the same identity, size and
/// start; a stop may be filled once. No end means charge through read time,
/// even if the node disappears or its grant expires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub attempt_id: Uuid,
    pub sandbox_id: String,
    pub size: Size,
    pub started_at_ms: u64,
    pub stopped_at_ms: Option<u64>,
}
impl Fact {
    pub fn validate(&self, now_ms: u64) -> Result<()> {
        if self.attempt_id.is_nil()
            || self.sandbox_id.is_empty()
            || self.sandbox_id.len() > 256
            || self.sandbox_id.chars().any(char::is_control)
            || self.started_at_ms == 0
            || self.started_at_ms > now_ms
            || self
                .stopped_at_ms
                .is_some_and(|end| end < self.started_at_ms || end > now_ms)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn succeeds(&self, previous: &Self) -> bool {
        self.attempt_id == previous.attempt_id
            && self.sandbox_id == previous.sandbox_id
            && self.size == previous.size
            && self.started_at_ms == previous.started_at_ms
            && (previous.stopped_at_ms.is_none() || self.stopped_at_ms == previous.stopped_at_ms)
    }
}

/// Half-open UTC calendar-month interval, calculated by the host's calendar.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonthlyBudget {
    pub month_start_ms: u64,
    pub next_month_start_ms: u64,
    pub ceiling_microunits: u64,
}
impl MonthlyBudget {
    pub fn cost(&self, fact: &Fact, card: &RateCard, now_ms: u64) -> Result<u64> {
        fact.validate(now_ms)?;
        if self.month_start_ms >= self.next_month_start_ms
            || now_ms < self.month_start_ms
            || now_ms >= self.next_month_start_ms
        {
            return Err(Error::Invalid);
        }
        let start = fact.started_at_ms.max(self.month_start_ms);
        let end = fact
            .stopped_at_ms
            .unwrap_or(now_ms)
            .min(now_ms)
            .min(self.next_month_start_ms);
        card.cost(fact.size, end.saturating_sub(start))
    }
    pub fn reached(&self, spent: u64) -> bool {
        spent >= self.ceiling_microunits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fact(start: u64, end: Option<u64>) -> Fact {
        Fact {
            attempt_id: Uuid::new_v4(),
            sandbox_id: "vm-1".into(),
            size: Size::Small,
            started_at_ms: start,
            stopped_at_ms: end,
        }
    }
    #[test]
    fn rate_times_running_duration_for_each_boat_size() {
        let card = RateCard::boat();
        for (size, rate) in [
            (Size::Small, 18_000),
            (Size::Default, 36_000),
            (Size::Large, 72_000),
            (Size::Xlarge, 200_000),
        ] {
            assert_eq!(card.cost(size, 3_600_000).unwrap(), rate);
            assert_eq!(card.cost(size, 1_800_000).unwrap(), rate / 2);
            assert_eq!(card.cost(size, 0).unwrap(), 0);
        }
        assert_eq!(card.cost(Size::Small, 1).unwrap(), 1);
        assert_eq!(
            card.cost(Size::Xlarge, u64::MAX).unwrap(),
            u64::MAX.div_ceil(18)
        );
    }
    #[test]
    fn month_rollover_splits_running_and_stopped_intervals() {
        let card = RateCard::boat();
        let old = MonthlyBudget {
            month_start_ms: 1,
            next_month_start_ms: 7_200_001,
            ceiling_microunits: 18_000,
        };
        let new = MonthlyBudget {
            month_start_ms: 7_200_001,
            next_month_start_ms: 14_400_001,
            ceiling_microunits: 18_000,
        };
        let running = fact(3_600_001, None);
        assert_eq!(old.cost(&running, &card, 7_200_000).unwrap(), 18_000);
        assert_eq!(new.cost(&running, &card, 7_200_001).unwrap(), 0);
        assert_eq!(new.cost(&running, &card, 10_800_001).unwrap(), 18_000);
        let stopped = fact(3_600_001, Some(9_000_001));
        assert_eq!(new.cost(&stopped, &card, 10_800_001).unwrap(), 9_000);
        assert_eq!(
            new.cost(&fact(1, Some(3_600_001)), &card, 10_800_001)
                .unwrap(),
            0
        );
        assert!(!new.reached(17_999));
        assert!(new.reached(18_000));
    }
    #[test]
    fn stop_is_monotonic_and_replays_never_add_intervals() {
        let running = fact(1, None);
        let mut stopped = running.clone();
        stopped.stopped_at_ms = Some(2);
        assert!(stopped.succeeds(&running));
        assert!(stopped.succeeds(&stopped));
        assert!(!running.succeeds(&stopped));
        assert!(stopped.validate(1).is_err());
        let mut changed = stopped.clone();
        changed.size = Size::Xlarge;
        assert!(!changed.succeeds(&stopped));
    }
}
