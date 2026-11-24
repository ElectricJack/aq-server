use super::*;

pub(super) struct EvaluatedRow {
    pub(super) trace: CandidateTrace,
    pub(super) choice: Option<ConcreteChoice>,
    pub(super) revision: u64,
    pub(super) excluded: bool,
}
fn fallback(state: CandidateState) -> Option<FallbackReason> {
    match state {
        CandidateState::Eligible => None,
        CandidateState::MissingCredentials => Some(FallbackReason::MissingCredentials),
        CandidateState::Unavailable => Some(FallbackReason::Unavailable),
        CandidateState::Unsupported => Some(FallbackReason::Unsupported),
        CandidateState::StaleUnknown => Some(FallbackReason::StaleUnknown),
        CandidateState::PolicyDenied => Some(FallbackReason::PolicyDenied),
    }
}

#[derive(Default)]
struct Ranking {
    pinned: Option<String>,
    selected: Option<(ConcreteChoice, u64, usize)>,
    blocked: bool,
    pin_refused: bool,
}
impl Ranking {
    fn consider(&mut self, index: usize, row: &mut EvaluatedRow, policy: &SelectionPolicy) {
        if row.excluded {
            return;
        }
        let trace = &mut row.trace;
        if self.blocked {
            trace.state = CandidateState::PolicyDenied;
            trace.reason_codes.push("fallback_not_permitted".into());
        } else if self.pinned.as_deref().is_some_and(|p| p != trace.provider) {
            trace.state = CandidateState::PolicyDenied;
            trace.reason_codes.push("route_provider_pinned".into());
            self.pin_refused = true;
        } else if trace.state == CandidateState::Eligible {
            self.selected = row
                .choice
                .take()
                .map(|choice| (choice, row.revision, index));
        } else if fallback(trace.state).is_some_and(|r| !policy.fallback_on.contains(&r)) {
            self.blocked = true;
        }
    }

    fn reason_codes(&self) -> Vec<String> {
        if self.selected.is_some() {
            return vec![];
        }
        let reason = if self.pin_refused {
            "route_provider_pinned"
        } else if self.blocked {
            "no_permitted_fallback"
        } else {
            "no_eligible_candidate"
        };
        vec![reason.into()]
    }
}
pub(super) fn rank(
    mut rows: Vec<EvaluatedRow>,
    route: Option<&RouteLimits>,
    policy: &SelectionPolicy,
) -> CoreOutcome {
    // Spec 0011 §4: choose the first allowed provider in profile order before sorting by route preference.
    let pinned = route
        .filter(|r| r.pinned)
        .and_then(|_| rows.iter().find(|r| !r.excluded))
        .map(|r| r.trace.provider.clone());
    if let Some(route) = route {
        crate::route::preferred_first(route, &mut rows, |r| &r.trace.harness);
    }
    let mut result = Ranking {
        pinned,
        ..Ranking::default()
    };
    for (index, row) in rows.iter_mut().enumerate() {
        result.consider(index, row, policy);
        if result.selected.is_some() {
            break;
        }
    }
    CoreOutcome {
        reason_codes: result.reason_codes(),
        selected: result.selected,
        traces: rows.into_iter().map(|r| r.trace).collect(),
    }
}
