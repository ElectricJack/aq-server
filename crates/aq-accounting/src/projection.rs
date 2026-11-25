//! Constant-size per-owner projection. Only canonical contribution replacements
//! reach this reducer; raw transcript events and duplicate facts do not.
use crate::{Category, Error, Result, wire};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Projection {
    pub canonical_calls: u64,
    pub known_subtotals: [u64; 5],
    pub unknown_contributions: [u64; 5],
    pub incomplete_calls: u64,
    pub unresolved_events: u64,
}
impl Projection {
    pub fn validate(&self) -> Result<()> {
        let invalid_counts = self
            .unknown_contributions
            .iter()
            .chain(std::iter::once(&self.incomplete_calls))
            .any(|n| *n > self.canonical_calls);
        let unexpected_subtotals = self.canonical_calls == 0 && self.known_subtotals != [0; 5];
        if invalid_counts || unexpected_subtotals {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// Replace a canonical fact and its completeness together. Category presence
    /// alone cannot turn a parser's partial report into complete accounting.
    pub fn replace_fact(
        &self,
        old: Option<&crate::reconcile::Fact>,
        new: &crate::reconcile::Fact,
    ) -> Result<Self> {
        new.validate()?;
        if let Some(old) = old {
            old.validate()?;
            if old.key != new.key || old.lineage != new.lineage {
                return Err(Error::Conflict);
            }
        }
        let mut next = self.replace(old.map(|v| &v.contribution), &new.contribution)?;
        if old.is_some_and(crate::reconcile::Fact::source_incomplete) {
            next.incomplete_calls = next
                .incomplete_calls
                .checked_sub(1)
                .ok_or(Error::Conflict)?;
        }
        if new.source_incomplete() {
            next.incomplete_calls = next
                .incomplete_calls
                .checked_add(1)
                .ok_or(Error::Overflow)?;
        }
        next.validate()?;
        Ok(next)
    }
    /// Proposed replacement only. The adapter must CAS the exact canonical head
    /// plus this projection in the same transaction as the append-only fact.
    pub fn replace(&self, old: Option<&wire::Counters>, new: &wire::Counters) -> Result<Self> {
        self.validate()?;
        let mut next = self.clone();
        if old.is_none() {
            next.canonical_calls = next.canonical_calls.checked_add(1).ok_or(Error::Overflow)?;
        } else if self.canonical_calls == 0 {
            return Err(Error::Conflict);
        }
        for (i, category) in Category::ALL.into_iter().enumerate() {
            if let Some(old) = old {
                match category.get(old) {
                    wire::Counter::Known { value } => {
                        next.known_subtotals[i] = next.known_subtotals[i]
                            .checked_sub(*value)
                            .ok_or(Error::Conflict)?
                    }
                    wire::Counter::Unknown { .. } => {
                        next.unknown_contributions[i] = next.unknown_contributions[i]
                            .checked_sub(1)
                            .ok_or(Error::Conflict)?
                    }
                }
            }
            match category.get(new) {
                wire::Counter::Known { value } => {
                    next.known_subtotals[i] = next.known_subtotals[i]
                        .checked_add(*value)
                        .ok_or(Error::Overflow)?
                }
                wire::Counter::Unknown { .. } => {
                    next.unknown_contributions[i] = next.unknown_contributions[i]
                        .checked_add(1)
                        .ok_or(Error::Overflow)?
                }
            }
        }
        next.validate()?;
        Ok(next)
    }
    pub fn counters(&self) -> Result<wire::Counters> {
        self.validate()?;
        let counter = |i: usize| {
            if self.unresolved_events > 0 {
                wire::Counter::Unknown {
                    reason: wire::UnknownReason::Incomplete,
                }
            } else if self.canonical_calls == 0 {
                wire::Counter::Unknown {
                    reason: wire::UnknownReason::NotReported,
                }
            } else if self.unknown_contributions[i] > 0 || self.incomplete_calls > 0 {
                wire::Counter::Unknown {
                    reason: wire::UnknownReason::Incomplete,
                }
            } else {
                wire::Counter::Known {
                    value: self.known_subtotals[i],
                }
            }
        };
        Ok(wire::Counters {
            input: counter(0),
            output: counter(1),
            cache_read: counter(2),
            cache_write: counter(3),
            reasoning: counter(4),
        })
    }
    pub fn all_categories_represented(&self) -> Result<bool> {
        self.validate()?;
        Ok(self.canonical_calls > 0
            && self.unresolved_events == 0
            && self.incomplete_calls == 0
            && self.unknown_contributions == [0; 5])
    }
}
