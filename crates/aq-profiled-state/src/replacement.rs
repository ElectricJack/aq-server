//! The pure B4 replacement decision over a task's started-attempt history (moved out of the
//! aq-server lib). The server loads the facts (`Prior`) and acts on the `Assessment`.
use crate::{Effects, Retention, Status, Stop};
use aq_profiles::roles::LaunchRetryBackoff;
use aq_session_protocol::ArtifactRef;
use uuid::Uuid;

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

/// A task's retry budget and launch wait. The budget comes from the latest
/// pinned policy where available. The wait follows current Selection defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_attempts: u32,
    pub max_uncharged_continuations: u32,
    pub launch_retry_backoff: LaunchRetryBackoff,
}
impl Limits {
    /// The retry budgets come from the latest attempt's pinned policy when available.
    /// The launch backoff and any absent budget values come from current Settings.
    pub fn of(
        pinned: Option<&aq_profiles::SelectionPolicy>,
        settings: &aq_profiles::roles::SelectionDefaults,
    ) -> Self {
        Self {
            max_attempts: pinned.map_or(settings.max_attempts, |p| p.max_attempts),
            max_uncharged_continuations: pinned
                .and_then(|p| p.max_uncharged_continuations)
                .unwrap_or(settings.max_uncharged_continuations),
            launch_retry_backoff: settings.launch_retry_backoff,
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
    pub released_at_ms: Option<u64>,
    pub status: Status,
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
    /// Server receipt time of the current, verified worker interruption report.
    pub platform_interrupted_at_ms: Option<u64>,
    /// Offered before the owner's latest override of a budget hold: the budget started again
    /// after it, so the attempt is charged to nothing (and uses up no exemption).
    pub forgiven: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub predecessor: Uuid,
    pub attempts_started: u32,
    /// Started attempts not charged to the budgets: exempt (spec 0007 Q1) or forgiven by an
    /// owner override.
    pub exempt_attempts: u32,
    /// Lineage time the charged attempts used, as the selection decision records it.
    pub elapsed_ms: u64,
    pub stop_receipt: ArtifactRef,
    pub retained_source_receipt: ArtifactRef,
    pub effect_reconciliation_receipt: ArtifactRef,
    /// The predecessor still holds the task claim and must be released first.
    pub needs_release: bool,
}
impl Replacement {
    /// The closure owner frees a verified nonstart before scheduling can
    /// admit its successor. That release starts any launch backoff.
    pub fn waiting_for_launch_release(&self) -> bool {
        self.needs_release && launch_stop_receipt(&self.stop_receipt)
    }
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
    /// The decision as an inspector shows it: its kind, and the hold code or predecessor.
    pub fn view(&self) -> serde_json::Value {
        match self {
            Self::Fresh => serde_json::json!({"kind": "fresh"}),
            Self::Replace(r) => serde_json::json!({"kind": "replace",
                "predecessor": r.predecessor, "needs_release": r.needs_release,
                "attempts_started": r.attempts_started, "exempt_attempts": r.exempt_attempts}),
            Self::Blocked(code) => serde_json::json!({"kind": "blocked", "code": code}),
        }
    }
}

fn failed(prior: &Prior, now: u64) -> bool {
    prior.cancelled_settled
        || prior.rejected
        || matches!(prior.status, Status::Finished | Status::Declined)
        || prior.execution_deadline_ms.is_some_and(|d| now >= d)
}

/// Which attempts (in order) are not charged: forgiven by an owner override, or exempt up to
/// the cap, which counts only exemptions since that override.
fn uncharged(ordered: &[&Prior], limits: &Limits) -> Vec<bool> {
    let exempt = ordered
        .iter()
        .map(|p| p.exempt && !p.forgiven && !p.uncharged_platform_failure());
    let capped = aq_machine_control::cap_exempt(exempt, limits.max_uncharged_continuations);
    (ordered.iter().zip(capped))
        .map(|(p, exempt)| p.forgiven || p.uncharged_platform_failure() || exempt)
        .collect()
}

impl Prior {
    fn uncharged_platform_failure(&self) -> bool {
        self.proven_nonstart() || self.platform_interrupted()
    }

    fn platform_interrupted(&self) -> bool {
        self.platform_interrupted_at_ms.is_some()
    }

    /// The closure settlement owner writes this state only after verifying the
    /// launch never ran. A failed or absent result does not establish nonstart.
    pub fn proven_nonstart(&self) -> bool {
        self.status == Status::Finished
            && matches!(&self.stop, Stop::NotStarted { receipt } if launch_stop_receipt(receipt))
    }
}

fn launch_stop_receipt(receipt: &ArtifactRef) -> bool {
    let id = receipt.id.as_str();
    id.starts_with(aq_machine_control::lapse::RECEIPT) || id.starts_with("aq.launch-closure/")
}

pub const LAUNCH_BACKOFF: &str = "launch_retry_backoff";

/// The first verified worker failure can continue right away. Repeated nonstarts
/// wait from claim release. Repeated stopped interruptions wait from the receipt
/// of the original result, including when the scheduler can release and offer in one poll.
pub fn launch_retry_after(priors: &[Prior], backoff: LaunchRetryBackoff) -> Option<(Uuid, u64)> {
    let mut ordered = priors.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|p| (p.offered_at_ms, p.attempt_id));
    launch_retry_after_ordered(&ordered, backoff)
}

fn launch_retry_after_ordered(
    ordered: &[&Prior],
    backoff: LaunchRetryBackoff,
) -> Option<(Uuid, u64)> {
    let last = ordered.last()?;
    let (anchor, same_kind): (u64, fn(&Prior) -> bool) =
        if let Some(at) = last.platform_interrupted_at_ms {
            last.verify_settlement().ok()?;
            last.receipts().ok()?;
            (at, Prior::platform_interrupted)
        } else {
            if last.holds_task {
                return None;
            }
            (last.released_at_ms?, Prior::proven_nonstart)
        };
    let consecutive = ordered
        .iter()
        .rev()
        .take_while(|p| same_kind(p) && !p.forgiven)
        // A positive initial delay has reached any u64 cap after 64 doublings.
        .take(u64::BITS as usize + 2)
        .count();
    let exponent = consecutive.checked_sub(2)? as u32;
    let delay = backoff
        .initial_delay_ms
        .saturating_mul(2u64.saturating_pow(exponent))
        .min(backoff.max_delay_ms);
    Some((last.attempt_id, anchor.saturating_add(delay)))
}
/// The started attempts charged to the retry budget, counted as [`assess`] counts them (owner
/// and service stops may be exempt under spec 0007 Q1, and verified platform
/// failures are always uncharged), and the latest attempt.
pub fn charged(priors: &[Prior], limits: &Limits) -> (u32, Option<Uuid>) {
    let mut ordered = priors.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|p| (p.offered_at_ms, p.attempt_id));
    let free = uncharged(&ordered, limits);
    let n = u32::try_from(free.iter().filter(|e| !**e).count()).unwrap_or(u32::MAX);
    (n, ordered.last().map(|p| p.attempt_id))
}
/// The latest started attempt: the one whose pinned policy holds the task's [`Limits`].
pub fn latest(priors: &[Prior]) -> Option<Uuid> {
    let last = priors
        .iter()
        .max_by_key(|p| (p.offered_at_ms, p.attempt_id));
    last.map(|p| p.attempt_id)
}

/// Every fact of one started attempt that keeps [`assess`] from replacing it, in [`assess`]'s
/// order; empty when that attempt alone blocks nothing (lineage budgets are not per attempt).
/// This is what an inspector names for a `replacement_effects_unreconciled` or
/// `replacement_prior_execution_uncertain` hold: which owner fact is missing.
pub fn blockers(prior: &Prior, now: u64) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut add = |blocked: bool, code| {
        if blocked {
            out.push(code);
        }
    };
    add(prior.offered_at_ms.is_none(), "lineage_history_unknown");
    add(prior.accepted, "settlement_accepted");
    add(!prior.stop.known_stopped(), "stop_not_proven");
    add(matches!(prior.effects, Effects::Unknown), "effects_unknown");
    add(
        matches!(prior.effects, Effects::Pending { .. }),
        "effects_pending",
    );
    add(
        prior.needs_reconciliation,
        "settlement_needs_reconciliation",
    );
    add(prior.completion.is_some(), "task_completion_recorded");
    add(
        !matches!(prior.retention, Retention::Retained { .. }),
        "source_not_retained",
    );
    add(
        !prior.holds_task && !prior.released,
        "claim_release_unknown",
    );
    add(!failed(prior, now), "not_failed");
    out
}

/// Pure replacement decision over the complete started-attempt history.
pub fn assess(priors: &[Prior], limits: &Limits, now: u64) -> Assessment {
    assess_lineage(priors, limits, now, true)
}

/// Scheduling assessment for one Graph-ready task.
/// The decision attempt counts alone settle: an exhausted never-granted
/// retry bound blocks, and no started attempt is an ordinary first attempt.
pub fn precheck(counts: &aq_machine_control::attempts::AttemptCounts) -> Option<Assessment> {
    if counts.never_granted_bound_exhausted() {
        return Some(Assessment::Blocked("never_granted_retry_bound_exhausted"));
    }
    (counts.started == 0).then_some(Assessment::Fresh)
}

/// A fresh start closes the old lineage, so its limits do not charge the new source.
/// Stop, effect, retention, settlement and release proofs still apply in full.
pub fn assess_for_fresh_start(priors: &[Prior], limits: &Limits, now: u64) -> Assessment {
    assess_lineage(priors, limits, now, false)
}

fn assess_lineage(priors: &[Prior], limits: &Limits, now: u64, charge_budget: bool) -> Assessment {
    use Assessment::Blocked;
    let mut ordered = priors.iter().collect::<Vec<_>>();
    if ordered.is_empty() {
        return Assessment::Fresh;
    }
    if ordered.iter().any(|p| p.offered_at_ms.is_none()) {
        return Blocked("replacement_lineage_history_unknown");
    }
    ordered.sort_by_key(|p| (p.offered_at_ms, p.attempt_id));
    if let Err(code) = verify_history(&ordered) {
        return Blocked(code);
    }
    let predecessor = ordered[ordered.len() - 1];
    if !failed(predecessor, now) {
        return Blocked("replacement_predecessor_not_failed");
    }
    let receipts = match predecessor.receipts() {
        Ok(receipts) => receipts,
        Err(code) => return Blocked(code),
    };
    let budget = match lineage_budget(&ordered, limits, now, charge_budget) {
        Ok(budget) => budget,
        Err(code) => return Blocked(code),
    };
    Assessment::Replace(Box::new(Replacement {
        predecessor: predecessor.attempt_id,
        attempts_started: budget.started,
        exempt_attempts: budget.exempt,
        elapsed_ms: budget.elapsed_ms,
        stop_receipt: receipts.stop.clone(),
        retained_source_receipt: receipts.retained.clone(),
        effect_reconciliation_receipt: receipts.effects.clone(),
        needs_release: predecessor.holds_task,
    }))
}

fn verify_history(ordered: &[&Prior]) -> Result<(), &'static str> {
    if ordered.iter().any(|p| p.accepted) {
        return Err("task_already_settled_accepted");
    }
    if ordered.iter().filter(|p| p.holds_task).count() > 1 {
        return Err("replacement_prior_execution_uncertain");
    }
    for prior in ordered {
        prior.verify_settlement()?;
    }
    let predecessor = ordered[ordered.len() - 1];
    // Every earlier attempt must have released its claim through this owner.
    if ordered[..ordered.len() - 1]
        .iter()
        .any(|p| p.holds_task || !p.released)
    {
        return Err("replacement_prior_execution_uncertain");
    }
    if !predecessor.holds_task && !predecessor.released {
        return Err("replacement_prior_execution_uncertain");
    }
    Ok(())
}
struct ReplacementReceipts<'a> {
    stop: &'a ArtifactRef,
    retained: &'a ArtifactRef,
    effects: &'a ArtifactRef,
}
impl Prior {
    fn verify_settlement(&self) -> Result<(), &'static str> {
        if !self.stop.known_stopped() {
            return Err("replacement_prior_execution_uncertain");
        }
        let settled = !self.needs_reconciliation && self.completion.is_none();
        if !settled || self.effects.receipt().is_none() {
            return Err("replacement_effects_unreconciled");
        }
        Ok(())
    }
    fn receipts(&self) -> Result<ReplacementReceipts<'_>, &'static str> {
        Ok(ReplacementReceipts {
            stop: self
                .stop
                .receipt()
                .ok_or("replacement_prior_execution_uncertain")?,
            retained: self
                .retention
                .receipt()
                .ok_or("replacement_source_retention_unknown")?,
            effects: self
                .effects
                .receipt()
                .ok_or("replacement_effects_unreconciled")?,
        })
    }
}
struct LineageBudget {
    started: u32,
    exempt: u32,
    elapsed_ms: u64,
}
fn lineage_budget(
    ordered: &[&Prior],
    limits: &Limits,
    now: u64,
    charge_budget: bool,
) -> Result<LineageBudget, &'static str> {
    let attempts_started = u32::try_from(ordered.len()).unwrap_or(u32::MAX);
    let exempt = uncharged(ordered, limits);
    let exempt_attempts = u32::try_from(exempt.iter().filter(|e| **e).count()).unwrap_or(0);
    if charge_budget && attempts_started - exempt_attempts >= limits.max_attempts {
        return Err("lineage_retry_budget_exhausted");
    }
    if charge_budget
        && launch_retry_after_ordered(ordered, limits.launch_retry_backoff)
            .is_some_and(|(_, next)| now < next)
    {
        return Err(LAUNCH_BACKOFF);
    }
    // Lineage time is recorded, never charged: each attempt has its own window (its execution
    // profile's wall time) and a retry is limited by `max_attempts` alone. It is the sum of the
    // charged attempts' windows, each from its publication to the next one (or now).
    let elapsed_ms = (0..ordered.len())
        .filter(|&i| !exempt[i])
        .map(|i| {
            let end = ordered.get(i + 1).and_then(|n| n.offered_at_ms);
            end.unwrap_or(now)
                .saturating_sub(ordered[i].offered_at_ms.unwrap_or(now))
        })
        .fold(0u64, u64::saturating_add);
    Ok(LineageBudget {
        started: attempts_started,
        exempt: exempt_attempts,
        elapsed_ms,
    })
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
            released_at_ms: None,
            status: Status::Finished,
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
            platform_interrupted_at_ms: None,
            forgiven: false,
        }
    }
    /// The budget the tests were written for.
    fn budget() -> Limits {
        Limits {
            max_attempts: 3,
            max_uncharged_continuations: 5,
            launch_retry_backoff: LaunchRetryBackoff::default(),
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
        let limits = budget();
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
            receipt: receipt("aq.launch-closure/verified"),
        };
        let Assessment::Replace(r) = assess(std::slice::from_ref(&closed), &budget(), now) else {
            panic!("settled closure must be replaceable");
        };
        assert_eq!(r.stop_receipt, receipt("aq.launch-closure/verified"));
        assert!(r.needs_release);
        assert!(r.waiting_for_launch_release());
        // An unsettled closure (stop still unknown) blocks.
        closed.stop = Stop::Unknown;
        closed.status = Status::Cancelling;
        assert_eq!(
            assess(&[closed], &budget(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    #[test]
    fn repeated_verified_nonstarts_wait_without_spending_the_execution_budget() {
        let limits = Limits {
            max_attempts: 1,
            max_uncharged_continuations: 0,
            ..budget()
        };
        let first = released(failed_prior(1_000));
        let second = released(failed_prior(2_000));
        let mut failures = [first, second];
        for attempt in &mut failures {
            attempt.stop = Stop::NotStarted {
                receipt: receipt(&format!(
                    "{}{}",
                    aq_machine_control::lapse::RECEIPT,
                    attempt.attempt_id
                )),
            };
            attempt.released_at_ms = Some(3_000);
        }
        assert_eq!(charged(&failures, &limits).0, 0);
        assert_eq!(
            launch_retry_after(&failures, limits.launch_retry_backoff),
            Some((failures[1].attempt_id, 33_000))
        );
        assert_eq!(
            assess(&failures, &limits, 32_999),
            Assessment::Blocked(LAUNCH_BACKOFF)
        );
        let Assessment::Replace(retry) = assess(&failures, &limits, 33_000) else {
            panic!("the same ticket can continue when the backoff elapses");
        };
        assert_eq!((retry.attempts_started, retry.exempt_attempts), (2, 2));
        assert!(!retry.needs_release);
        let mut actual_failure = failures.to_vec();
        actual_failure.push(failed_prior(34_000));
        assert_eq!(
            assess(&actual_failure, &limits, 35_000),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }

    #[test]
    fn repeated_platform_interruptions_wait_before_same_poll_release() {
        let mut first = released(failed_prior(1_000));
        first.platform_interrupted_at_ms = Some(1_500);
        let mut second = failed_prior(3_000);
        second.platform_interrupted_at_ms = Some(3_500);
        let limits = budget();
        let history = [first, second];
        let next = 3_500 + limits.launch_retry_backoff.initial_delay_ms;
        assert_eq!(
            launch_retry_after(&history, limits.launch_retry_backoff),
            Some((history[1].attempt_id, next))
        );
        assert_eq!(charged(&history, &limits).0, 0);
        assert_eq!(
            assess(&history, &limits, next - 1),
            Assessment::Blocked(LAUNCH_BACKOFF)
        );
        let Assessment::Replace(retry) = assess(&history, &limits, next) else {
            panic!("the next poll after the delay must admit continuation");
        };
        assert!(retry.needs_release);
        assert_eq!(retry.predecessor, history[1].attempt_id);
    }

    #[test]
    fn a_platform_interruption_spends_neither_repair_nor_handoff_allowance() {
        let mut interrupted = released(failed_prior(1_000));
        interrupted.platform_interrupted_at_ms = Some(1_500);
        interrupted.exempt = true;
        let mut owner_stop = released(failed_prior(2_000));
        owner_stop.exempt = true;
        let limits = Limits {
            max_attempts: 3,
            max_uncharged_continuations: 1,
            launch_retry_backoff: LaunchRetryBackoff::default(),
        };
        let mut history = vec![
            interrupted,
            owner_stop,
            released(failed_prior(3_000)),
            failed_prior(4_000),
        ];
        assert_eq!(charged(&history, &limits).0, 2);
        assert!(assess(&history, &limits, 4_001).replacement().is_some());

        history[3] = released(failed_prior(4_000));
        history.push(failed_prior(5_000));
        assert_eq!(charged(&history, &limits).0, 3);
        assert_eq!(
            assess(&history, &limits, 5_001),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }

    #[test]
    fn launch_backoff_uses_current_settings_and_the_latest_release_time() {
        let settings = aq_profiles::roles::SelectionDefaults {
            launch_retry_backoff: LaunchRetryBackoff {
                initial_delay_ms: 10_000,
                max_delay_ms: 15_000,
            },
            ..Default::default()
        };
        let pinned = aq_profiles::roles::SelectionDefaults::default().policy(1, Vec::new());
        let limits = Limits::of(Some(&pinned), &settings);
        let mut failures = Vec::new();
        for index in 1..=3 {
            let mut attempt = released(failed_prior(index * 1_000));
            attempt.stop = Stop::NotStarted {
                receipt: receipt("aq.launch-closure/verified"),
            };
            attempt.released_at_ms = Some(index * 1_000 + 500);
            failures.push(attempt);
        }
        assert_eq!(
            launch_retry_after(&failures[..1], settings.launch_retry_backoff),
            None
        );
        assert!(matches!(
            assess(&failures[..1], &limits, 1_500),
            Assessment::Replace(_)
        ));
        for (index, wake_at) in [(2, 12_500), (3, 18_500)] {
            let history = &failures[..index];
            assert_eq!(
                launch_retry_after(history, settings.launch_retry_backoff),
                Some((history[index - 1].attempt_id, wake_at))
            );
            assert_eq!(charged(history, &limits).0, 0);
            assert_eq!(
                assess(history, &limits, wake_at - 1),
                Assessment::Blocked(LAUNCH_BACKOFF)
            );
            assert!(matches!(
                assess(history, &limits, wake_at),
                Assessment::Replace(_)
            ));
        }
    }

    #[test]
    fn retry_budget_exhaustion_blocks_task() {
        let now = 1_000_000;
        let limits = Limits {
            max_attempts: 2,
            ..budget()
        };
        let history = [released(failed_prior(800_000)), failed_prior(900_000)];
        assert_eq!(
            assess(&history, &limits, now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        // The refusal names the charged count the budget used up, and the latest attempt.
        assert_eq!(charged(&history, &limits), (2, Some(history[1].attempt_id)));
        let mut stopped = history.clone();
        stopped[0].exempt = true;
        assert_eq!(
            charged(&stopped, &limits).0,
            1,
            "an owner stop is not charged"
        );
        // Uncertain prior execution blocks before any budget question.
        let mut running = failed_prior(900_000);
        running.stop = Stop::Running;
        assert_eq!(
            assess(&[running], &budget(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
        let mut pending = failed_prior(900_000);
        pending.effects = Effects::Pending { count: 1 };
        assert_eq!(
            assess(&[pending], &budget(), now),
            Assessment::Blocked("replacement_effects_unreconciled")
        );
        let mut unretained = failed_prior(900_000);
        unretained.retention = Retention::Pending;
        assert_eq!(
            assess(&[unretained], &budget(), now),
            Assessment::Blocked("replacement_source_retention_unknown")
        );
        let mut accepted = failed_prior(900_000);
        accepted.accepted = true;
        assert_eq!(
            assess(&[accepted], &budget(), now),
            Assessment::Blocked("task_already_settled_accepted")
        );
        let mut unreleased = failed_prior(800_000);
        unreleased.holds_task = false;
        assert_eq!(
            assess(&[unreleased, failed_prior(900_000)], &budget(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    /// A finished attempt whose result was returned but whose effects were never recorded
    /// quiescent (the 2026-11-21 attachment-task hold) names exactly that fact, and every
    /// per-attempt refusal of `assess` has a blocker name.
    #[test]
    fn blockers_name_the_missing_owner_fact() {
        let now = 1_000_000;
        assert!(blockers(&failed_prior(900_000), now).is_empty());
        let mut held = failed_prior(900_000);
        held.effects = Effects::Unknown;
        assert_eq!(blockers(&held, now), ["effects_unknown"]);
        assert_eq!(
            assess(std::slice::from_ref(&held), &budget(), now),
            Assessment::Blocked("replacement_effects_unreconciled")
        );
        for (edit, code) in [
            (
                (|p| p.effects = Effects::Pending { count: 2 }) as fn(&mut Prior),
                "effects_pending",
            ),
            (
                |p| p.needs_reconciliation = true,
                "settlement_needs_reconciliation",
            ),
            (
                |p| p.completion = Some("conflict".into()),
                "task_completion_recorded",
            ),
            (|p| p.stop = Stop::Running, "stop_not_proven"),
            (|p| p.retention = Retention::Pending, "source_not_retained"),
            (|p| p.accepted = true, "settlement_accepted"),
            (|p| p.offered_at_ms = None, "lineage_history_unknown"),
            (|p| p.holds_task = false, "claim_release_unknown"),
            (
                |p| {
                    p.rejected = false;
                    p.status = Status::Granted;
                    p.execution_deadline_ms = Some(u64::MAX);
                },
                "not_failed",
            ),
        ] {
            let mut p = failed_prior(900_000);
            edit(&mut p);
            assert_eq!(blockers(&p, now), [code]);
            assert!(
                !matches!(assess(&[p], &budget(), now), Assessment::Replace(_)),
                "{code} must also block assess"
            );
        }
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
        let Assessment::Replace(r) = assess(&history(true), &budget(), now) else {
            panic!("two exempt handoffs leave one charged attempt");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (3, 2));
        assert_eq!(
            assess(&history(false), &budget(), now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }
    #[test]
    fn exempt_windows_are_not_lineage_time() {
        let now = 25 * HOUR;
        let history = |first: Prior| [released(first), failed_prior(23 * HOUR)];
        let Assessment::Replace(r) = assess(&history(exempt(failed_prior(0))), &budget(), now)
        else {
            panic!("the exempt window is not charged");
        };
        assert_eq!(r.elapsed_ms, 2 * HOUR);
        // Lineage time is recorded and never refuses a retry.
        let Assessment::Replace(r) = assess(&history(failed_prior(0)), &budget(), now) else {
            panic!("lineage time limits nothing");
        };
        assert_eq!(r.elapsed_ms, 25 * HOUR);
    }
    /// Each attempt has its own window: however long earlier attempts were admitted for or ran,
    /// only `max_attempts` limits a retry.
    #[test]
    fn however_long_attempts_ran_only_max_attempts_limits_a_retry() {
        let long = |offered: u64| {
            let mut p = failed_prior(offered);
            p.execution_deadline_ms = Some(offered + 250 * MIN);
            p
        };
        let limits = Limits {
            max_attempts: 4,
            ..budget()
        };
        let mut history = Vec::new();
        for attempt in 0..4u64 {
            let now = (attempt * 50 + 5) * HOUR;
            let outcome = assess(&history, &limits, now);
            if attempt == 0 {
                assert_eq!(outcome, Assessment::Fresh);
            } else {
                assert!(
                    matches!(outcome, Assessment::Replace(_)),
                    "attempt {attempt} of 4 is refused: {outcome:?}"
                );
            }
            if let Some(last) = history.pop() {
                history.push(released(last));
            }
            history.push(long(now));
        }
        assert_eq!(
            assess(&history, &limits, 300 * HOUR),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }
    #[test]
    fn no_exempt_priors_match_previous_values() {
        let now = 1_000_000;
        let (first, second) = (failed_prior(800_000), failed_prior(900_000));
        let Assessment::Replace(r) = assess(&[released(first), second], &budget(), now) else {
            panic!("within the default budget");
        };
        assert_eq!(r.exempt_attempts, 0);
        assert_eq!(r.elapsed_ms, now - 800_000);
    }

    /// Owner decision 2026-11-18: an owner override of a budget hold starts the budget again.
    /// Attempts before it are charged to nothing: not attempts and not time.
    fn long(offered: u64) -> Prior {
        let mut prior = failed_prior(offered);
        prior.execution_deadline_ms = Some(offered + 100 * MIN);
        prior
    }
    #[test]
    fn an_owner_override_gives_a_fresh_budget() {
        let now = 30 * HOUR;
        // Three attempts: the retry budget is used up.
        let history = [released(long(0)), released(long(HOUR)), long(29 * HOUR)];
        assert_eq!(
            assess(&history, &budget(), now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        let forgiven = history.clone().map(|mut p| {
            p.forgiven = true;
            p
        });
        let Assessment::Replace(r) = assess(&forgiven, &budget(), now) else {
            panic!("the override releases the task");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (3, 3));
        assert_eq!(r.elapsed_ms, 0);
        assert_eq!(charged(&forgiven, &budget()).0, 0);
        // The next attempt is charged from its own offer on, against the whole budget.
        let mut after = forgiven.clone().map(released).to_vec();
        after.push(long(now));
        let Assessment::Replace(r) = assess(&after, &budget(), now + HOUR) else {
            panic!("one charged attempt of three");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (4, 3));
        assert_eq!(r.elapsed_ms, HOUR);
        // Forgiven exemptions do not use up the cap on uncharged continuations.
        let one = Limits {
            max_uncharged_continuations: 1,
            max_attempts: 1,
            ..budget()
        };
        let mut stopped = forgiven.clone().map(released).map(exempt).to_vec();
        stopped.push(exempt(long(now)));
        assert!(matches!(
            assess(&stopped, &one, now + HOUR),
            Assessment::Replace(_)
        ));
    }
    #[test]
    fn an_owner_override_never_lifts_a_safety_hold() {
        let now = 30 * HOUR;
        let forgiven =
            [released(long(0)), released(long(HOUR)), long(29 * HOUR)].map(|mut prior| {
                prior.forgiven = true;
                prior
            });
        // What happened to the task still refuses, override or not.
        let unsafe_last = |edit: fn(&mut Prior)| {
            let mut h = forgiven.clone();
            edit(&mut h[2]);
            assess(&h, &budget(), now)
        };
        for (edit, code) in [
            (
                (|p| p.stop = Stop::Running) as fn(&mut Prior),
                "replacement_prior_execution_uncertain",
            ),
            (
                |p| p.retention = Retention::Pending,
                "replacement_source_retention_unknown",
            ),
            (
                |p| p.effects = Effects::Pending { count: 1 },
                "replacement_effects_unreconciled",
            ),
            (
                |p| p.needs_reconciliation = true,
                "replacement_effects_unreconciled",
            ),
            (|p| p.accepted = true, "task_already_settled_accepted"),
            (
                |p| p.offered_at_ms = None,
                "replacement_lineage_history_unknown",
            ),
            (
                |p| {
                    p.rejected = false;
                    p.status = Status::Granted;
                    p.execution_deadline_ms = Some(u64::MAX);
                },
                "replacement_predecessor_not_failed",
            ),
        ] {
            assert_eq!(unsafe_last(edit), Assessment::Blocked(code));
        }
    }
    /// The limits are the values passed in (Settings); nothing is built in.
    #[test]
    fn every_limit_is_the_value_passed_in() {
        let now = 10 * MIN;
        let history = [released(failed_prior(0)), failed_prior(5 * MIN)];
        let wide = Limits {
            max_attempts: 32,
            max_uncharged_continuations: 0,
            ..budget()
        };
        assert!(matches!(
            assess(&history, &wide, now),
            Assessment::Replace(_)
        ));
        let limits = Limits {
            max_attempts: 2,
            ..wide
        };
        assert_eq!(
            assess(&history, &limits, now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        // No uncharged continuation is allowed: an owner stop is charged like any attempt.
        let stopped = [released(exempt(failed_prior(0))), failed_prior(5 * MIN)];
        let two = Limits {
            max_attempts: 2,
            ..wide
        };
        assert_eq!(
            assess(&stopped, &two, now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        let allowed = Limits {
            max_uncharged_continuations: 1,
            ..two
        };
        assert!(matches!(
            assess(&stopped, &allowed, now),
            Assessment::Replace(_)
        ));
    }
    #[test]
    fn limits_are_the_pinned_policy_and_else_the_current_settings() {
        let settings = aq_profiles::roles::SelectionDefaults {
            max_attempts: 4,
            max_uncharged_continuations: 9,
            ..Default::default()
        };
        let all = |n: u64| Limits {
            max_attempts: n as u32,
            max_uncharged_continuations: n as u32 + 5,
            ..budget()
        };
        assert_eq!(Limits::of(None, &settings), all(4));
        let mut pinned = aq_profiles::roles::SelectionDefaults {
            max_attempts: 1,
            max_uncharged_continuations: 6,
            ..Default::default()
        }
        .policy(1, Vec::new());
        assert_eq!(Limits::of(Some(&pinned), &settings), all(1));
        // A policy pinned before the continuation limit was a Setting.
        pinned.max_uncharged_continuations = None;
        let mixed = Limits {
            max_uncharged_continuations: 9,
            ..all(1)
        };
        assert_eq!(Limits::of(Some(&pinned), &settings), mixed);
    }
}
