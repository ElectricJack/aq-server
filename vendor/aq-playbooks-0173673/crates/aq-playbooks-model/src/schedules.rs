//! Fixed-interval occurrence planning for a host-owned scheduler.
//!
//! A host MUST atomically persist the returned cursor and occurrence outbox rows
//! before delivering any occurrence. Replaying an outbox row preserves its ID.
//! This helper neither dispatches events nor owns timezone/calendar semantics.
use crate::{Hash, ModelError, digest};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MisfirePolicy {
    Skip,
    CoalesceOnce,
    BoundedCatchUp { max_occurrences: u32 },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedIntervalSchedule {
    pub id: String,
    pub scope: String,
    pub revision: String,
    pub start_at_ms: i64,
    pub interval_ms: u64,
    /// An occurrence within this delay is timely under Skip. This explicit
    /// tolerance avoids requiring scheduler wakeups at an exact millisecond.
    pub grace_ms: u64,
    pub misfire: MisfirePolicy,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCursor {
    pub schedule_fingerprint: Hash,
    pub next_index: u64,
    pub last_observed_ms: Option<i64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleOccurrence {
    pub id: Hash,
    pub schedule_id: String,
    pub schedule_revision: String,
    pub scope: String,
    pub index: u64,
    pub scheduled_at_ms: i64,
    /// Coalescing records the complete represented range; it does not pretend
    /// that each missed occurrence was independently delivered.
    pub first_index: u64,
    pub represented_count: u64,
    pub misfired: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulePlan {
    pub cursor: ScheduleCursor,
    pub occurrences: Vec<ScheduleOccurrence>,
    pub skipped_count: u64,
    pub clock_moved_backwards: bool,
    pub due_remaining: u64,
}
impl FixedIntervalSchedule {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.scope.is_empty()
            || self.scope.len() > 128
            || self.revision.is_empty()
            || self.revision.len() > 128
            || self.interval_ms == 0
            || self.interval_ms > 31_536_000_000
            || self.grace_ms >= self.interval_ms
            || matches!(self.misfire, MisfirePolicy::BoundedCatchUp { max_occurrences } if max_occurrences == 0 || max_occurrences > 1024)
        {
            return Err(ModelError::Invalid(
                "invalid bounded fixed-interval schedule".into(),
            ));
        }
        Ok(())
    }
    pub fn cursor(&self) -> Result<ScheduleCursor, ModelError> {
        self.validate()?;
        Ok(ScheduleCursor {
            schedule_fingerprint: digest(self)?,
            next_index: 0,
            last_observed_ms: None,
        })
    }
    /// Plan bounded work without iterating through a potentially huge backlog.
    /// Catch-up retains the newest configured finite suffix, skipping older work.
    /// `batch_limit` caps emitted rows; un-emitted suffix rows remain due.
    pub fn plan(
        &self,
        cursor: &ScheduleCursor,
        now_ms: i64,
        batch_limit: usize,
    ) -> Result<SchedulePlan, ModelError> {
        self.validate()?;
        if batch_limit == 0 || batch_limit > 1024 {
            return Err(ModelError::Invalid(
                "schedule batch bound must be 1..1024".into(),
            ));
        }
        if cursor.schedule_fingerprint != digest(self)? {
            return Err(ModelError::Integrity(
                "schedule cursor revision/fingerprint".into(),
            ));
        }
        let backwards = cursor.last_observed_ms.is_some_and(|old| now_ms < old);
        let mut next = cursor.clone();
        if !backwards {
            next.last_observed_ms = Some(now_ms);
        }
        let mut plan = SchedulePlan {
            cursor: next,
            occurrences: vec![],
            skipped_count: 0,
            clock_moved_backwards: backwards,
            due_remaining: 0,
        };
        if backwards || now_ms < self.start_at_ms {
            return Ok(plan);
        }
        let elapsed = i128::from(now_ms) - i128::from(self.start_at_ms);
        let last = u64::try_from(elapsed / i128::from(self.interval_ms))
            .map_err(|_| ModelError::Invalid("schedule index overflow".into()))?;
        if last < cursor.next_index {
            return Ok(plan);
        }
        let due = last
            .checked_sub(cursor.next_index)
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| ModelError::Invalid("schedule backlog overflow".into()))?;
        let after_last = last
            .checked_add(1)
            .ok_or_else(|| ModelError::Invalid("schedule index exhausted".into()))?;
        match self.misfire {
            MisfirePolicy::Skip => {
                let at = self.at(last)?;
                if now_ms.saturating_sub(at) as u64 <= self.grace_ms {
                    plan.occurrences.push(self.occurrence(
                        last,
                        last,
                        1,
                        now_ms,
                        &cursor.schedule_fingerprint,
                    )?);
                }
                plan.skipped_count = due - plan.occurrences.len() as u64;
                plan.cursor.next_index = after_last;
            }
            MisfirePolicy::CoalesceOnce => {
                plan.occurrences.push(self.occurrence(
                    last,
                    cursor.next_index,
                    due,
                    now_ms,
                    &cursor.schedule_fingerprint,
                )?);
                plan.cursor.next_index = after_last;
            }
            MisfirePolicy::BoundedCatchUp { max_occurrences } => {
                let kept = due.min(u64::from(max_occurrences));
                plan.skipped_count = due - kept;
                let first = cursor.next_index + plan.skipped_count;
                let emitted = kept.min(batch_limit as u64);
                for index in first..first + emitted {
                    plan.occurrences.push(self.occurrence(
                        index,
                        index,
                        1,
                        now_ms,
                        &cursor.schedule_fingerprint,
                    )?);
                }
                plan.cursor.next_index = first + emitted;
                plan.due_remaining = kept - emitted;
            }
        }
        Ok(plan)
    }
    fn at(&self, index: u64) -> Result<i64, ModelError> {
        i64::try_from(
            i128::from(self.start_at_ms) + i128::from(index) * i128::from(self.interval_ms),
        )
        .map_err(|_| ModelError::Invalid("schedule timestamp overflow".into()))
    }
    fn occurrence(
        &self,
        index: u64,
        first_index: u64,
        count: u64,
        now: i64,
        fingerprint: &str,
    ) -> Result<ScheduleOccurrence, ModelError> {
        let at = self.at(index)?;
        Ok(ScheduleOccurrence {
            id: digest(&(fingerprint, index))?,
            schedule_id: self.id.clone(),
            schedule_revision: self.revision.clone(),
            scope: self.scope.clone(),
            index,
            scheduled_at_ms: at,
            first_index,
            represented_count: count,
            misfired: count > 1 || now.saturating_sub(at) as u64 > self.grace_ms,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn schedule(misfire: MisfirePolicy) -> FixedIntervalSchedule {
        FixedIntervalSchedule {
            id: "heartbeat".into(),
            scope: "project".into(),
            revision: "1".into(),
            start_at_ms: 0,
            interval_ms: 100,
            grace_ms: 5,
            misfire,
        }
    }
    #[test]
    fn skip_records_missed_work_and_emits_only_timely_latest() {
        let s = schedule(MisfirePolicy::Skip);
        let p = s.plan(&s.cursor().unwrap(), 503, 5).unwrap();
        assert_eq!(p.skipped_count, 5);
        assert_eq!(p.occurrences[0].index, 5);
        assert!(!p.occurrences[0].misfired);
        assert_eq!(p.cursor.next_index, 6);
        let late = s.plan(&s.cursor().unwrap(), 550, 5).unwrap();
        assert!(late.occurrences.is_empty());
        assert_eq!(late.skipped_count, 6);
    }
    #[test]
    fn coalescing_replay_has_stable_identity_and_complete_range() {
        let s = schedule(MisfirePolicy::CoalesceOnce);
        let c = s.cursor().unwrap();
        let p = s.plan(&c, 550, 1).unwrap();
        assert_eq!(p, s.plan(&c, 550, 1).unwrap());
        assert_eq!(p.occurrences.len(), 1);
        assert_eq!(p.occurrences[0].first_index, 0);
        assert_eq!(p.occurrences[0].represented_count, 6);
        assert!(p.occurrences[0].misfired);
        let restored: ScheduleCursor =
            serde_json::from_str(&serde_json::to_string(&p.cursor).unwrap()).unwrap();
        assert!(s.plan(&restored, 550, 1).unwrap().occurrences.is_empty());
    }
    #[test]
    fn catchup_is_bounded_paged_and_does_not_scan_huge_backlogs() {
        let s = schedule(MisfirePolicy::BoundedCatchUp { max_occurrences: 3 });
        let p = s.plan(&s.cursor().unwrap(), 1_000_000_000, 2).unwrap();
        assert_eq!(
            p.occurrences.iter().map(|o| o.index).collect::<Vec<_>>(),
            vec![9_999_998, 9_999_999]
        );
        assert_eq!(p.skipped_count, 9_999_998);
        assert_eq!(p.due_remaining, 1);
        let next = s.plan(&p.cursor, 1_000_000_000, 2).unwrap();
        assert_eq!(next.occurrences[0].index, 10_000_000);
        assert_eq!(next.due_remaining, 0);
        assert_eq!(next.skipped_count, 0);
    }
    #[test]
    fn clock_jumps_and_changed_revisions_are_explicit() {
        let s = schedule(MisfirePolicy::CoalesceOnce);
        let p = s.plan(&s.cursor().unwrap(), 500, 1).unwrap();
        let backwards = s.plan(&p.cursor, 100, 1).unwrap();
        assert!(backwards.clock_moved_backwards);
        assert!(backwards.occurrences.is_empty());
        assert_eq!(backwards.cursor, p.cursor);
        let mut changed = s.clone();
        changed.interval_ms = 200;
        assert!(changed.plan(&p.cursor, 600, 1).is_err());
        assert!(s.plan(&p.cursor, 600, 0).is_err());
    }
}
