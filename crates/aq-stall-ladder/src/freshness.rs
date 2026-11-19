//! What a session's observations say right now, for the session reads. It is
//! decided from the ledger facts and the two windows the ladder itself uses
//! ([`ExecutionPolicy`]), so a read and the ladder cannot disagree about what
//! "no progress" or "no observations" means.
use crate::policy::ExecutionPolicy;

/// The attempt's process is over: every end path (stopped receipt, lost
/// release, cancellation, decline, never granted) stores one of these
/// statuses. `a` is `execution_attempts`, `h` its `profiled_authority_hot`.
pub const ENDED_SQL: &str = "COALESCE(h.state->>'status',a.state->>'status','unknown') IN ('finished','declined','never_granted')";

/// The closed set a session read shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Nothing can be said: see the reason.
    Unknown,
    /// A source is enrolled and no event has been recorded yet.
    Awaiting,
    /// Progress was observed within the evidence window.
    Working,
    /// Observations arrive, but none was progress within the evidence window.
    Quiet,
    /// No observation arrived within the grace window: the session cannot be
    /// judged, which the ladder treats as a monitoring incident, not a stall.
    Stale,
    /// The attempt's process is over.
    Ended,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Awaiting => "awaiting",
            Self::Working => "working",
            Self::Quiet => "quiet",
            Self::Stale => "stale",
            Self::Ended => "ended",
        }
    }
}

/// The ledger facts of one attempt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub ended: bool,
    /// An observation source was enrolled for the attempt.
    pub enrolled: bool,
    /// When the server last recorded an event of the attempt, and one that
    /// counts as progress ([`crate::evidence::is_progress`]).
    pub last_observation_ms: Option<u64>,
    pub last_progress_ms: Option<u64>,
}

/// The state and, when it needs one, the reason code.
pub fn state(facts: &Facts, policy: &ExecutionPolicy, now: u64) -> (State, Option<&'static str>) {
    let within =
        |at: Option<u64>, window: u64| at.is_some_and(|at| now.saturating_sub(at) <= window);
    if facts.ended {
        (State::Ended, None)
    } else if facts.last_observation_ms.is_none() {
        if facts.enrolled {
            (State::Awaiting, None)
        } else {
            (State::Unknown, Some("no_observation_source"))
        }
    } else if !within(facts.last_observation_ms, policy.grace_ms) {
        (State::Stale, Some("no_observation_within_grace"))
    } else if within(facts.last_progress_ms, policy.evidence_window_ms) {
        (State::Working, None)
    } else {
        (State::Quiet, Some("no_progress_within_evidence_window"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIN: u64 = 60_000;

    #[test]
    fn each_state_follows_from_its_ledger_facts_and_the_ladder_windows() {
        let policy = ExecutionPolicy::v1();
        let now = 100 * MIN;
        let facts = |ended, enrolled, observed: u64, progress: u64| Facts {
            ended,
            enrolled,
            last_observation_ms: Some(observed).filter(|at| *at > 0),
            last_progress_ms: Some(progress).filter(|at| *at > 0),
        };
        let grace = policy.grace_ms;
        let window = policy.evidence_window_ms;
        for (facts, state, reason) in [
            (
                facts(false, false, 0, 0),
                State::Unknown,
                Some("no_observation_source"),
            ),
            (facts(false, true, 0, 0), State::Awaiting, None),
            (facts(false, true, now, now - window), State::Working, None),
            (
                facts(false, true, now - grace, now - grace),
                State::Working,
                None,
            ),
            (
                facts(false, true, now, now - window - 1),
                State::Quiet,
                Some("no_progress_within_evidence_window"),
            ),
            (
                facts(false, true, now, 0),
                State::Quiet,
                Some("no_progress_within_evidence_window"),
            ),
            (
                facts(false, true, now - grace - 1, now - grace - 1),
                State::Stale,
                Some("no_observation_within_grace"),
            ),
            (facts(true, true, now, now), State::Ended, None),
            (facts(true, false, 0, 0), State::Ended, None),
        ] {
            assert_eq!(
                super::state(&facts, &policy, now),
                (state, reason),
                "{facts:?}"
            );
        }
    }
}
