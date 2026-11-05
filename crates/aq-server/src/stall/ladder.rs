//! The pure, deterministic stall ladder. [`step`] decides at most one intent
//! from the persisted episode and fresh evidence; [`apply`] folds the effect's
//! outcome back in. Neither reads a clock or performs I/O.
use super::evidence::{Evidence, Explanation, InputState};
use super::policy::ExecutionPolicy;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    Observing,
    /// Rung 1: no progress for the evidence window.
    Suspected,
    /// Rung 2: bounded nudges sent or deferred.
    Nudging,
    /// Rung 3: the owner was notified (once per episode).
    Notified,
    /// Rung 4: a typed stop was requested and awaits confirmation.
    Stopping,
    /// Rung 4: the stop is confirmed; replacement is being admitted.
    Replacing,
    Replaced,
    /// Rung 5: recovery exhausted or the stop is uncertain.
    Quarantined,
}
impl Rung {
    pub fn is_final(self) -> bool {
        matches!(self, Self::Replaced | Self::Quarantined)
    }
    /// Destructive recovery is under way; progress no longer reverts it.
    pub fn is_recovering(self) -> bool {
        matches!(self, Self::Stopping | Self::Replacing)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Attempted,
    /// Held in the inbox only: input was unknown or a human held the pane.
    Deferred,
    Delivered,
    Unknown,
}
impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Deferred => "deferred",
            Self::Delivered => "delivered",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    MonitoringIncident {
        ordinal: u32,
    },
    /// `defer` means inbox-only: never injected at a boundary.
    Nudge {
        ordinal: u32,
        defer: bool,
    },
    Notify,
    Stop,
    Replace,
    Quarantine {
        reason: String,
    },
}
impl Intent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MonitoringIncident { .. } => "monitoring_incident",
            Self::Nudge { .. } => "nudge",
            Self::Notify => "notify",
            Self::Stop => "stop",
            Self::Replace => "replace",
            Self::Quarantine { .. } => "quarantine",
        }
    }
    pub fn ordinal(&self) -> u32 {
        match self {
            Self::MonitoringIncident { ordinal } | Self::Nudge { ordinal, .. } => *ordinal,
            _ => 1,
        }
    }
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::Stop | Self::Replace | Self::Quarantine { .. })
    }
}

/// The receipt an effect port returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Nudged {
        message_id: Uuid,
        delivery: Delivery,
    },
    Replaced {
        replacement: Option<Uuid>,
    },
    /// B4 refused the replacement; the episode quarantines with this reason.
    Blocked {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub episode_id: Uuid,
    pub attempt_id: Uuid,
    pub lineage_id: Uuid,
    pub policy_version: u32,
    pub revision: u64,
    pub rung: Rung,
    pub rung_since_ms: u64,
    /// Start of the current evidence interval (latest progress or open).
    pub interval_started_ms: u64,
    pub intervals: u32,
    /// The latest instant an explanation held the ladder.
    pub explained_through_ms: Option<u64>,
    pub explanation: Option<Explanation>,
    pub observed_through_ms: Option<u64>,
    pub nudges: u32,
    pub deferrals: u32,
    pub last_nudge_ms: Option<u64>,
    pub notified: bool,
    pub incidents: u32,
    pub incident_open: bool,
    pub stop_requested_ms: Option<u64>,
    /// Ladder replacements already spent by this lineage (store-loaded).
    pub lineage_replacements: u32,
    pub suspended: Option<String>,
    pub blocked: Option<String>,
    pub quarantine: Option<String>,
}
impl Episode {
    pub fn open(attempt_id: Uuid, lineage_id: Uuid, policy: &ExecutionPolicy, now: u64) -> Self {
        Self {
            episode_id: super::episode_id(attempt_id),
            attempt_id,
            lineage_id,
            policy_version: policy.version,
            revision: 0,
            rung: Rung::Observing,
            rung_since_ms: now,
            interval_started_ms: now,
            intervals: 1,
            explained_through_ms: None,
            explanation: None,
            observed_through_ms: None,
            nudges: 0,
            deferrals: 0,
            last_nudge_ms: None,
            notified: false,
            incidents: 0,
            incident_open: false,
            stop_requested_ms: None,
            lineage_replacements: 0,
            suspended: None,
            blocked: None,
            quarantine: None,
        }
    }
    fn enter(&mut self, rung: Rung, now: u64) {
        if self.rung != rung {
            self.rung = rung;
            self.rung_since_ms = now;
        }
    }
    fn quiet_since(&self) -> u64 {
        self.interval_started_ms
            .max(self.explained_through_ms.unwrap_or(0))
    }
}

/// Decide the next state and at most one intent. Deterministic in its inputs.
pub fn step(
    current: &Episode,
    evidence: &Evidence,
    policy: &ExecutionPolicy,
    now: u64,
) -> (Episode, Option<Intent>) {
    let mut ep = current.clone();
    if ep.rung.is_final() {
        return (ep, None);
    }
    ep.observed_through_ms = evidence.observed_through_ms;
    ep.explanation = evidence.explanation;
    // Admission recheck. Our own stop cancels the attempt and may release it,
    // so only holds and hibernation suspend recovery already under way.
    let a = evidence.admission;
    let blocker = if ep.rung.is_recovering() {
        [(a.held, "held"), (a.hibernated, "hibernated")]
            .into_iter()
            .find_map(|(set, why)| set.then_some(why))
    } else {
        a.blocker()
    };
    ep.suspended = blocker.map(str::to_owned);
    if blocker.is_some() {
        return (ep, None);
    }
    match ep.rung {
        Rung::Stopping => return stopping(ep, evidence, policy, now),
        Rung::Replacing => {
            let intent = ep
                .blocked
                .clone()
                .map(|reason| Intent::Quarantine { reason });
            return (ep, intent);
        }
        _ => {}
    }
    // Progress opens a new interval; per-episode and lineage ceilings persist.
    if let Some(progress) = evidence.last_progress_ms
        && progress > ep.interval_started_ms
    {
        ep.interval_started_ms = progress;
        ep.intervals += 1;
        ep.incident_open = false;
        ep.enter(Rung::Observing, now);
    }
    if let Some(explanation) = evidence.explanation {
        ep.explained_through_ms = Some(now);
        if explanation == Explanation::ObservationsUnavailable && !ep.incident_open {
            ep.incident_open = true;
            ep.incidents += 1;
            let ordinal = ep.incidents;
            return (ep, Some(Intent::MonitoringIncident { ordinal }));
        }
        return (ep, None);
    }
    ep.incident_open = false;
    let quiet = now.saturating_sub(ep.quiet_since());
    if quiet < policy.evidence_window_ms {
        ep.enter(Rung::Observing, now);
        return (ep, None);
    }
    if ep.rung == Rung::Observing {
        ep.enter(Rung::Suspected, now);
        return (ep, None);
    }
    if ep.rung == Rung::Suspected && now - ep.rung_since_ms < policy.grace_ms {
        return (ep, None);
    }
    if ep.rung == Rung::Notified {
        let intent = (now - ep.rung_since_ms >= policy.notify_grace_ms).then_some(Intent::Stop);
        return (ep, intent);
    }
    let escalate = ep.nudges >= policy.nudge_limit || ep.deferrals >= policy.max_deferrals;
    if !escalate {
        let due = ep.last_nudge_ms.is_none_or(|last| {
            let backoff = policy.nudge_backoff_ms << ep.nudges.saturating_sub(1).min(16);
            now - last >= backoff
        });
        let intent = due.then(|| Intent::Nudge {
            ordinal: ep.nudges + 1,
            defer: evidence.input != InputState::Ready,
        });
        return (ep, intent);
    }
    if ep
        .last_nudge_ms
        .is_some_and(|last| now - last < policy.nudge_backoff_ms)
    {
        return (ep, None);
    }
    if ep.notified {
        ep.enter(Rung::Notified, now);
        return (ep, None);
    }
    (ep, Some(Intent::Notify))
}

fn stopping(
    ep: Episode,
    evidence: &Evidence,
    policy: &ExecutionPolicy,
    now: u64,
) -> (Episode, Option<Intent>) {
    let requested = ep.stop_requested_ms.unwrap_or(ep.rung_since_ms);
    let intent = if evidence.stop_confirmed {
        Some(if ep.lineage_replacements >= policy.replacement_ceiling {
            Intent::Quarantine {
                reason: "replacement_ceiling_reached".into(),
            }
        } else {
            Intent::Replace
        })
    } else if now - requested >= policy.stop_confirm_ms {
        Some(Intent::Quarantine {
            reason: "stop_uncertain".into(),
        })
    } else {
        None
    };
    (ep, intent)
}

/// Fold an effect's outcome into the episode.
pub fn apply(ep: &mut Episode, intent: &Intent, outcome: &Outcome, now: u64) {
    match (intent, outcome) {
        (Intent::MonitoringIncident { .. }, _) => {}
        (Intent::Nudge { .. }, Outcome::Nudged { delivery, .. }) => {
            ep.nudges += 1;
            ep.last_nudge_ms = Some(now);
            if *delivery == Delivery::Deferred {
                ep.deferrals += 1;
            }
            ep.enter(Rung::Nudging, now);
        }
        (Intent::Notify, _) => {
            ep.notified = true;
            ep.enter(Rung::Notified, now);
        }
        (Intent::Stop, _) => {
            ep.stop_requested_ms = Some(now);
            ep.enter(Rung::Stopping, now);
        }
        (Intent::Replace, Outcome::Blocked { reason }) => {
            ep.blocked = Some(reason.clone());
            ep.enter(Rung::Replacing, now);
        }
        (Intent::Replace, _) => {
            ep.lineage_replacements += 1;
            ep.enter(Rung::Replaced, now);
        }
        (Intent::Quarantine { reason }, _) => {
            ep.quarantine = Some(reason.clone());
            ep.enter(Rung::Quarantined, now);
        }
        (Intent::Nudge { .. }, _) => {
            ep.nudges += 1;
            ep.last_nudge_ms = Some(now);
            ep.enter(Rung::Nudging, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stall::evidence::{Admission, Evidence, InputState, is_progress};
    use aq_accounting::wire::{ObservationKind, TextRole};

    const MIN: u64 = 60_000;

    fn policy() -> ExecutionPolicy {
        ExecutionPolicy::v1()
    }
    fn fresh() -> Episode {
        Episode::open(Uuid::from_u128(1), Uuid::from_u128(2), &policy(), 0)
    }
    fn outcome_for(intent: &Intent) -> Outcome {
        match intent {
            Intent::Nudge { defer, .. } => Outcome::Nudged {
                message_id: Uuid::from_u128(9),
                delivery: if *defer {
                    Delivery::Deferred
                } else {
                    Delivery::Attempted
                },
            },
            Intent::Replace => Outcome::Replaced { replacement: None },
            _ => Outcome::Done,
        }
    }
    /// Step once each minute until `until`, applying default outcomes.
    fn run(ep: &mut Episode, ev: &Evidence, from: u64, until: u64) -> Vec<Intent> {
        let mut seen = vec![];
        let mut now = from;
        while now <= until {
            let (next, intent) = step(ep, ev, &policy(), now);
            *ep = next;
            if let Some(intent) = intent {
                apply(ep, &intent, &outcome_for(&intent), now);
                seen.push(intent);
            }
            now += MIN;
        }
        seen
    }

    #[test]
    fn ladder_walks_every_rung_in_order() {
        let mut ep = fresh();
        let mut ev = Evidence::quiet(0);
        let mut rungs = vec![ep.rung];
        let mut intents = vec![];
        for minute in 0..=120 {
            let now = minute * MIN;
            ev.stop_confirmed = ep.rung == Rung::Stopping && now >= ep.rung_since_ms + MIN;
            let (next, intent) = step(&ep, &ev, &policy(), now);
            ep = next;
            if let Some(intent) = intent {
                apply(&mut ep, &intent, &outcome_for(&intent), now);
                intents.push(intent);
            }
            if rungs.last() != Some(&ep.rung) {
                rungs.push(ep.rung);
            }
        }
        assert_eq!(
            rungs,
            [
                Rung::Observing,
                Rung::Suspected,
                Rung::Nudging,
                Rung::Notified,
                Rung::Stopping,
                Rung::Replaced
            ]
        );
        assert_eq!(
            intents,
            [
                Intent::Nudge {
                    ordinal: 1,
                    defer: false
                },
                Intent::Nudge {
                    ordinal: 2,
                    defer: false
                },
                Intent::Notify,
                Intent::Stop,
                Intent::Replace
            ]
        );
        assert_eq!(ep.lineage_replacements, 1);
    }

    #[test]
    fn explanations_hold_the_ladder() {
        for explanation in [
            Explanation::ProviderBackoff,
            Explanation::RegisteredWait,
            Explanation::KnownLongCheck,
            Explanation::HumanControl,
        ] {
            let mut ep = fresh();
            let mut ev = Evidence::quiet(0);
            ev.explanation = Some(explanation);
            assert!(run(&mut ep, &ev, 0, 240 * MIN).is_empty());
            assert_eq!(ep.rung, Rung::Observing);
            assert_eq!(ep.explanation, Some(explanation));
            // Once the explanation clears, the evidence window restarts.
            ev.explanation = None;
            assert!(run(&mut ep, &ev, 241 * MIN, 250 * MIN).is_empty());
            assert_eq!(ep.rung, Rung::Observing);
        }
    }

    #[test]
    fn unavailable_raises_incident_once_never_destroys() {
        let mut ep = fresh();
        let mut ev = Evidence::quiet(0);
        ev.last_progress_ms = None;
        ev.observed_through_ms = None;
        ev.explanation = Some(Explanation::ObservationsUnavailable);
        let intents = run(&mut ep, &ev, 0, 600 * MIN);
        assert_eq!(intents, [Intent::MonitoringIncident { ordinal: 1 }]);
        assert!(!intents.iter().any(Intent::is_destructive));
        assert_eq!(ep.rung, Rung::Observing);
        assert!(ep.incident_open);
    }

    #[test]
    fn aq_nudge_text_does_not_reset_clock() {
        let content: aq_accounting::wire::ArtifactRef = serde_json::from_value(serde_json::json!({
            "id": "nudge", "version": "1", "digest": "a".repeat(64)
        }))
        .unwrap();
        assert!(!is_progress(&ObservationKind::TextReference {
            role: TextRole::User,
            content: content.clone(),
            utf8_bytes: 1
        }));
        assert!(is_progress(&ObservationKind::TextReference {
            role: TextRole::Assistant,
            content,
            utf8_bytes: 1
        }));
        assert!(is_progress(&ObservationKind::TurnBoundary {
            in_turn: true
        }));
        // After a nudge, only user text and usage arrive: evidence keeps the
        // old progress instant, so the ladder keeps climbing.
        let mut ep = fresh();
        let ev = Evidence::quiet(0);
        let intents = run(&mut ep, &ev, 0, 40 * MIN);
        assert!(intents.contains(&Intent::Nudge {
            ordinal: 2,
            defer: false
        }));
        assert_eq!(ep.interval_started_ms, 0);
        assert_eq!(ep.intervals, 1);
    }

    #[test]
    fn progress_opens_interval_keeps_ceilings() {
        let mut ep = fresh();
        ep.lineage_replacements = 1;
        let mut ev = Evidence::quiet(0);
        run(&mut ep, &ev, 0, 21 * MIN);
        assert_eq!(ep.nudges, 1);
        ev.last_progress_ms = Some(22 * MIN);
        run(&mut ep, &ev, 22 * MIN, 22 * MIN);
        assert_eq!(ep.rung, Rung::Observing);
        assert_eq!(ep.intervals, 2);
        assert_eq!(ep.interval_started_ms, 22 * MIN);
        assert_eq!((ep.nudges, ep.lineage_replacements), (1, 1));
        // The next stall resumes from the spent nudge count.
        ev.stop_confirmed = true;
        let intents = run(&mut ep, &ev, 23 * MIN, 200 * MIN);
        assert_eq!(intents.iter().filter(|i| i.kind() == "nudge").count(), 1);
        // The lineage ceiling (1) is already spent: quarantine, not replace.
        assert!(!intents.contains(&Intent::Replace));
        assert_eq!(
            intents.last(),
            Some(&Intent::Quarantine {
                reason: "replacement_ceiling_reached".into()
            })
        );
    }

    #[test]
    fn unconfirmed_stop_quarantines() {
        let mut ep = fresh();
        let ev = Evidence::quiet(0);
        let intents = run(&mut ep, &ev, 0, 120 * MIN);
        assert!(intents.contains(&Intent::Stop));
        assert!(!intents.contains(&Intent::Replace));
        assert_eq!(
            intents.last(),
            Some(&Intent::Quarantine {
                reason: "stop_uncertain".into()
            })
        );
        assert_eq!(ep.rung, Rung::Quarantined);
        // Final: nothing further, ever.
        assert!(run(&mut ep, &ev, 121 * MIN, 200 * MIN).is_empty());
    }

    #[test]
    fn admission_recheck_suspends() {
        for admission in [
            Admission {
                cancelled: true,
                ..Default::default()
            },
            Admission {
                held: true,
                ..Default::default()
            },
            Admission {
                hibernated: true,
                ..Default::default()
            },
            Admission {
                ownership_lost: true,
                ..Default::default()
            },
        ] {
            let mut ep = fresh();
            let mut ev = Evidence::quiet(0);
            ev.admission = admission;
            assert!(run(&mut ep, &ev, 0, 200 * MIN).is_empty());
            assert_eq!(ep.suspended.as_deref(), admission.blocker());
        }
        // A hold during recovery suspends the replacement too.
        let mut ep = fresh();
        let mut ev = Evidence::quiet(0);
        let intents = run(&mut ep, &ev, 0, 40 * MIN);
        assert_eq!(intents.last(), Some(&Intent::Stop));
        ev.stop_confirmed = true;
        ev.admission.held = true;
        assert!(run(&mut ep, &ev, 41 * MIN, 60 * MIN).is_empty());
        assert_eq!(ep.rung, Rung::Stopping);
    }

    #[test]
    fn deferrals_escalate_to_notify() {
        let mut p = policy();
        p.nudge_limit = 8;
        p.max_deferrals = 2;
        let mut ep = fresh();
        let mut ev = Evidence::quiet(0);
        ev.input = InputState::Unknown;
        let mut intents = vec![];
        let mut now = 0;
        while intents.len() < 3 {
            let (next, intent) = step(&ep, &ev, &p, now);
            ep = next;
            if let Some(intent) = intent {
                apply(&mut ep, &intent, &outcome_for(&intent), now);
                intents.push(intent);
            }
            now += MIN;
        }
        assert_eq!(
            intents,
            [
                Intent::Nudge {
                    ordinal: 1,
                    defer: true
                },
                Intent::Nudge {
                    ordinal: 2,
                    defer: true
                },
                Intent::Notify
            ]
        );
        assert_eq!(ep.deferrals, 2);
    }
}
