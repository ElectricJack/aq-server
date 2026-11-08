//! The pure B4 replacement decision over a task's started-attempt history (moved out of the
//! aq-server lib). The server loads the facts (`Prior`) and acts on the `Assessment`.
use crate::{Effects, Retention, Status, Stop};
use aq_session_protocol::ArtifactRef;
use uuid::Uuid;

/// Server-owned lineage budgets until per-project budget configuration exists.
/// The selected intelligence policy's own attempt/time limits still fence.
pub const MAX_ATTEMPTS: u32 = 3;
pub const MAX_LINEAGE_ELAPSED_MS: u64 = 24 * 3_600_000;
pub const MAX_SANDBOX_MS: u64 = 240 * 60_000;
/// Replacement evidence is a fresh observation, matching the budget snapshot.
pub const EVIDENCE_TTL_MS: u64 = 2_000;

/// The B4 successor's evidence of `r`, fresh for `EVIDENCE_TTL_MS` (None if that overflows).
pub fn evidence(
    r: &Replacement,
    lineage: aq_selection::TaskLineage,
    now: u64,
) -> Option<aq_selection::ReplacementEvidence> {
    Some(aq_selection::ReplacementEvidence {
        lineage,
        predecessor_attempt_id: r.predecessor,
        stop_receipt: r.stop_receipt.clone(),
        retained_source_receipt: r.retained_source_receipt.clone(),
        effect_reconciliation_receipt: r.effect_reconciliation_receipt.clone(),
        observed_at_ms: now,
        expires_at_ms: now.checked_add(EVIDENCE_TTL_MS)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_attempts: u32,
    pub max_lineage_elapsed_ms: u64,
    pub max_sandbox_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            max_lineage_elapsed_ms: MAX_LINEAGE_ELAPSED_MS,
            max_sandbox_ms: MAX_SANDBOX_MS,
        }
    }
}

/// Owner facts for one started (not never-granted) attempt of a task.
#[derive(Debug, Clone)]
pub struct Prior {
    pub attempt_id: Uuid,
    /// Original publication instant; `None` means lineage history is unknown.
    pub offered_at_ms: Option<u64>,
    pub holds_task: bool,
    pub released: bool,
    pub status: Status,
    pub granted: bool,
    pub execution_deadline_ms: Option<u64>,
    pub stop: Stop,
    pub retention: Retention,
    pub effects: Effects,
    pub accepted: bool,
    pub rejected: bool,
    pub needs_reconciliation: bool,
    pub cancelled_settled: bool,
    pub completion: Option<String>,
    /// An owner_stop or service_stop handoff (spec 0007 Q1): not charged to
    /// any lineage budget.
    pub exempt: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub predecessor: Uuid,
    pub attempts_started: u32,
    /// Started attempts exempt from the budgets (spec 0007 Q1).
    pub exempt_attempts: u32,
    pub elapsed_ms: u64,
    pub sandbox_ms: u64,
    pub stop_receipt: ArtifactRef,
    pub retained_source_receipt: ArtifactRef,
    pub effect_reconciliation_receipt: ArtifactRef,
    /// The predecessor still holds the task claim and must be released first.
    pub needs_release: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Assessment {
    /// No started attempt exists; an ordinary first attempt.
    Fresh,
    Replace(Box<Replacement>),
    Blocked(&'static str),
}
impl Assessment {
    pub fn replacement(&self) -> Option<&Replacement> {
        match self {
            Self::Replace(r) => Some(r.as_ref()),
            _ => None,
        }
    }
}

fn failed(prior: &Prior, now: u64) -> bool {
    prior.cancelled_settled
        || prior.rejected
        || matches!(prior.status, Status::Finished | Status::Declined)
        || prior.execution_deadline_ms.is_some_and(|d| now >= d)
}

/// Pure replacement decision over the complete started-attempt history.
pub fn assess(priors: &[Prior], limits: &Limits, now: u64) -> Assessment {
    use Assessment::Blocked;
    let mut ordered = priors.iter().collect::<Vec<_>>();
    if ordered.is_empty() {
        return Assessment::Fresh;
    }
    if ordered.iter().any(|p| p.offered_at_ms.is_none()) {
        return Blocked("replacement_lineage_history_unknown");
    }
    ordered.sort_by_key(|p| (p.offered_at_ms, p.attempt_id));
    if ordered.iter().any(|p| p.accepted) {
        return Blocked("task_already_settled_accepted");
    }
    if ordered.iter().filter(|p| p.holds_task).count() > 1 {
        return Blocked("replacement_prior_execution_uncertain");
    }
    for prior in &ordered {
        if !prior.stop.known_stopped() {
            return Blocked("replacement_prior_execution_uncertain");
        }
        if !matches!(prior.effects, Effects::Quiescent { .. })
            || prior.needs_reconciliation
            || prior.completion.is_some()
        {
            return Blocked("replacement_effects_unreconciled");
        }
    }
    let predecessor = ordered[ordered.len() - 1];
    // Every earlier attempt must have released its claim through this owner.
    if ordered[..ordered.len() - 1]
        .iter()
        .any(|p| p.holds_task || !p.released)
    {
        return Blocked("replacement_prior_execution_uncertain");
    }
    if !predecessor.holds_task && !predecessor.released {
        return Blocked("replacement_prior_execution_uncertain");
    }
    if !failed(predecessor, now) {
        return Blocked("replacement_predecessor_not_failed");
    }
    let (Stop::Stopped { receipt: stop } | Stop::NotStarted { receipt: stop }) = &predecessor.stop
    else {
        return Blocked("replacement_prior_execution_uncertain");
    };
    let Retention::Retained { receipt: retained } = &predecessor.retention else {
        return Blocked("replacement_source_retention_unknown");
    };
    let Effects::Quiescent { receipt: effects } = &predecessor.effects else {
        return Blocked("replacement_effects_unreconciled");
    };
    let attempts_started = u32::try_from(ordered.len()).unwrap_or(u32::MAX);
    let exempt = aq_machine_control::cap_exempt(ordered.iter().map(|p| p.exempt));
    let exempt_attempts = u32::try_from(exempt.iter().filter(|e| **e).count()).unwrap_or(0);
    if attempts_started - exempt_attempts >= limits.max_attempts {
        return Blocked("lineage_retry_budget_exhausted");
    }
    // Lineage time is the sum of the charged attempts' windows, each from its
    // publication to the next one (or now); without exemptions, now - first.
    let elapsed_ms = (0..ordered.len())
        .filter(|&i| !exempt[i])
        .map(|i| {
            let end = ordered.get(i + 1).and_then(|n| n.offered_at_ms);
            end.unwrap_or(now)
                .saturating_sub(ordered[i].offered_at_ms.unwrap_or(now))
        })
        .fold(0u64, u64::saturating_add);
    if elapsed_ms >= limits.max_lineage_elapsed_ms {
        return Blocked("lineage_time_budget_exhausted");
    }
    // Conservative sandbox charge: each granted attempt is billed its full
    // admitted window from publication to execution deadline.
    let sandbox_ms = (ordered.iter().zip(&exempt))
        .filter(|(p, e)| p.granted && !**e)
        .map(|(p, _)| {
            p.execution_deadline_ms
                .unwrap_or(u64::MAX)
                .saturating_sub(p.offered_at_ms.unwrap_or(0))
        })
        .fold(0u64, u64::saturating_add);
    if sandbox_ms >= limits.max_sandbox_ms {
        return Blocked("task_sandbox_minutes_budget_exhausted");
    }
    Assessment::Replace(Box::new(Replacement {
        predecessor: predecessor.attempt_id,
        attempts_started,
        exempt_attempts,
        elapsed_ms,
        sandbox_ms,
        stop_receipt: stop.clone(),
        retained_source_receipt: retained.clone(),
        effect_reconciliation_receipt: effects.clone(),
        needs_release: predecessor.holds_task,
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn evidence_carries_the_predecessor_receipts_for_its_ttl() {
        let receipt = |id: &str| ArtifactRef {
            id: aq_session_protocol::Text::new(id).unwrap(),
            version: aq_session_protocol::Text::new("1").unwrap(),
            digest: aq_session_protocol::Digest::of(id.as_bytes()),
        };
        let r = Replacement {
            predecessor: Uuid::from_u128(1),
            attempts_started: 1,
            exempt_attempts: 0,
            elapsed_ms: 1,
            sandbox_ms: 0,
            stop_receipt: receipt("stop"),
            retained_source_receipt: receipt("retained"),
            effect_reconciliation_receipt: receipt("effects"),
            needs_release: true,
        };
        let lineage = aq_selection::TaskLineage {
            project_id: Uuid::from_u128(2),
            workspace_id: Uuid::from_u128(3),
            task_id: Uuid::from_u128(4),
        };
        let e = evidence(&r, lineage.clone(), 1_000).unwrap();
        assert_eq!(e.predecessor_attempt_id, r.predecessor);
        assert_eq!(e.stop_receipt, receipt("stop"));
        assert_eq!(e.retained_source_receipt, receipt("retained"));
        assert_eq!(e.effect_reconciliation_receipt, receipt("effects"));
        assert_eq!(e.expires_at_ms, 1_000 + EVIDENCE_TTL_MS);
        assert!(evidence(&r, lineage, u64::MAX).is_none());
    }
    use super::*;
    use aq_session_protocol::{Digest, Text};

    pub(crate) fn receipt(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    pub(crate) fn failed_prior(offered_at_ms: u64) -> Prior {
        Prior {
            attempt_id: Uuid::new_v4(),
            offered_at_ms: Some(offered_at_ms),
            holds_task: true,
            released: false,
            status: Status::Finished,
            granted: true,
            execution_deadline_ms: Some(offered_at_ms + 60_000),
            stop: Stop::Stopped {
                receipt: receipt("stop"),
            },
            retention: Retention::Retained {
                receipt: receipt("retained"),
            },
            effects: Effects::Quiescent {
                receipt: receipt("effects"),
            },
            accepted: false,
            rejected: true,
            needs_reconciliation: false,
            cancelled_settled: false,
            completion: None,
            exempt: false,
        }
    }
    fn released(mut prior: Prior) -> Prior {
        prior.holds_task = false;
        prior.released = true;
        prior
    }

    #[test]
    fn failed_attempt_is_replaced_within_retry_budget() {
        let now = 1_000_000;
        let limits = Limits::default();
        assert_eq!(assess(&[], &limits, now), Assessment::Fresh);
        let first = failed_prior(900_000);
        let Assessment::Replace(replacement) = assess(std::slice::from_ref(&first), &limits, now)
        else {
            panic!("a stopped, retained, quiescent failed attempt is replaceable");
        };
        assert_eq!(replacement.predecessor, first.attempt_id);
        assert_eq!(replacement.attempts_started, 1);
        assert_eq!(replacement.elapsed_ms, 100_000);
        assert!(replacement.needs_release);
        // (The server's `replacement::evidence` carries these receipts to the successor.)
        assert_eq!(replacement.predecessor, first.attempt_id);
        assert_eq!(replacement.stop_receipt, receipt("stop"));
        assert_eq!(replacement.retained_source_receipt, receipt("retained"));
        assert_eq!(
            replacement.effect_reconciliation_receipt,
            receipt("effects")
        );

        // After the durable release, the second failure replaces from the newest
        // predecessor; history order is by publication, not attempt id.
        let second = failed_prior(950_000);
        let Assessment::Replace(again) =
            assess(&[second.clone(), released(first.clone())], &limits, now)
        else {
            panic!("second attempt remains within the default budget");
        };
        assert_eq!(again.predecessor, second.attempt_id);
        assert_eq!(again.attempts_started, 2);
        // An already released predecessor needs no second release.
        let Assessment::Replace(idem) = assess(&[released(first.clone())], &limits, now) else {
            panic!("released predecessor stays replaceable");
        };
        assert!(!idem.needs_release);

        // Cancellation settlement and an expired deadline also prove failure.
        let mut cancelled = failed_prior(900_000);
        cancelled.rejected = false;
        cancelled.status = Status::Cancelling;
        cancelled.cancelled_settled = true;
        assert!(matches!(
            assess(&[cancelled], &limits, now),
            Assessment::Replace(_)
        ));
        let mut expired = failed_prior(900_000);
        expired.rejected = false;
        expired.status = Status::Granted;
        assert!(matches!(
            assess(&[expired.clone()], &limits, now),
            Assessment::Replace(_)
        ));
        expired.execution_deadline_ms = Some(now + 1);
        assert_eq!(
            assess(&[expired], &limits, now),
            Assessment::Blocked("replacement_predecessor_not_failed")
        );
    }

    #[test]
    fn closed_unstarted_attempt_is_replaceable() {
        // B5b/AQ-08: a settled launch closure (not started, its own export
        // retained, no effects) is a failed predecessor like any other.
        let now = 1_000_000;
        let mut closed = failed_prior(900_000);
        closed.rejected = false;
        closed.stop = Stop::NotStarted {
            receipt: receipt("closure"),
        };
        let Assessment::Replace(r) = assess(std::slice::from_ref(&closed), &Limits::default(), now)
        else {
            panic!("settled closure must be replaceable");
        };
        assert_eq!(r.stop_receipt, receipt("closure"));
        assert!(r.needs_release);
        // An unsettled closure (stop still unknown) blocks.
        closed.stop = Stop::Unknown;
        closed.status = Status::Cancelling;
        assert_eq!(
            assess(&[closed], &Limits::default(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    #[test]
    fn retry_budget_exhaustion_blocks_task() {
        let now = 1_000_000;
        let limits = Limits {
            max_attempts: 2,
            ..Limits::default()
        };
        let history = [released(failed_prior(800_000)), failed_prior(900_000)];
        assert_eq!(
            assess(&history, &limits, now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        assert_eq!(
            assess(
                &history,
                &Limits {
                    max_lineage_elapsed_ms: 200_000,
                    ..Limits::default()
                },
                now
            ),
            Assessment::Blocked("lineage_time_budget_exhausted")
        );
        assert_eq!(
            assess(
                &history,
                &Limits {
                    max_sandbox_ms: 120_000,
                    ..Limits::default()
                },
                now
            ),
            Assessment::Blocked("task_sandbox_minutes_budget_exhausted")
        );
        // Uncertain prior execution blocks before any budget question.
        let mut running = failed_prior(900_000);
        running.stop = Stop::Running;
        assert_eq!(
            assess(&[running], &Limits::default(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
        let mut pending = failed_prior(900_000);
        pending.effects = Effects::Pending { count: 1 };
        assert_eq!(
            assess(&[pending], &Limits::default(), now),
            Assessment::Blocked("replacement_effects_unreconciled")
        );
        let mut unretained = failed_prior(900_000);
        unretained.retention = Retention::Pending;
        assert_eq!(
            assess(&[unretained], &Limits::default(), now),
            Assessment::Blocked("replacement_source_retention_unknown")
        );
        let mut accepted = failed_prior(900_000);
        accepted.accepted = true;
        assert_eq!(
            assess(&[accepted], &Limits::default(), now),
            Assessment::Blocked("task_already_settled_accepted")
        );
        let mut unreleased = failed_prior(800_000);
        unreleased.holds_task = false;
        assert_eq!(
            assess(
                &[unreleased, failed_prior(900_000)],
                &Limits::default(),
                now
            ),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    fn exempt(mut prior: Prior) -> Prior {
        prior.exempt = true;
        prior
    }
    const MIN: u64 = 60_000;
    const HOUR: u64 = 3_600_000;

    /// Review Focus 6: owner/service handoffs never exhaust the retry budget;
    /// a crash restart (not exempt) is still charged.
    #[test]
    fn owner_and_service_handoffs_are_exempt_restart_counts() {
        let now = 30 * MIN;
        let history = |exempt_first_two: bool| {
            let mark = |p: Prior| if exempt_first_two { exempt(p) } else { p };
            [
                released(mark(failed_prior(0))),
                released(mark(failed_prior(10 * MIN))),
                failed_prior(20 * MIN),
            ]
        };
        let Assessment::Replace(r) = assess(&history(true), &Limits::default(), now) else {
            panic!("two exempt handoffs leave one charged attempt");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (3, 2));
        assert_eq!(
            assess(&history(false), &Limits::default(), now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }
    #[test]
    fn exempt_windows_are_not_lineage_time() {
        let now = 25 * HOUR;
        let history = |first: Prior| [released(first), failed_prior(23 * HOUR)];
        let Assessment::Replace(r) =
            assess(&history(exempt(failed_prior(0))), &Limits::default(), now)
        else {
            panic!("the exempt window is not charged");
        };
        assert_eq!(r.elapsed_ms, 2 * HOUR);
        assert_eq!(
            assess(&history(failed_prior(0)), &Limits::default(), now),
            Assessment::Blocked("lineage_time_budget_exhausted")
        );
    }
    #[test]
    fn exempt_attempts_cost_no_sandbox_minutes() {
        let now = 10 * HOUR;
        let long = |offered: u64| {
            let mut p = failed_prior(offered);
            p.execution_deadline_ms = Some(offered + 150 * MIN);
            p
        };
        let history = |e: bool| {
            let first = if e { exempt(long(0)) } else { long(0) };
            [released(first), long(3 * HOUR)]
        };
        let Assessment::Replace(r) = assess(&history(true), &Limits::default(), now) else {
            panic!("an exempt attempt costs no sandbox minutes");
        };
        assert_eq!(r.sandbox_ms, 150 * MIN);
        assert_eq!(
            assess(&history(false), &Limits::default(), now),
            Assessment::Blocked("task_sandbox_minutes_budget_exhausted")
        );
    }
    #[test]
    fn no_exempt_priors_match_previous_values() {
        let now = 1_000_000;
        let (first, second) = (failed_prior(800_000), failed_prior(900_000));
        let Assessment::Replace(r) = assess(&[released(first), second], &Limits::default(), now)
        else {
            panic!("within the default budget");
        };
        assert_eq!(r.exempt_attempts, 0);
        assert_eq!(r.elapsed_ms, now - 800_000);
        assert_eq!(r.sandbox_ms, 2 * 60_000);
    }
}
