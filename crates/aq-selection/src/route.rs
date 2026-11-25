//! Historical task route limits are ordered placement preferences. The level a routed task runs on is the launch's own (spec 0014).
use crate::RouteLimits;

/// Refusals that can appear in retained decisions from older releases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteRefusal {
    /// The pinned provider cannot serve and the route refuses fallback.
    ProviderPinned,
    /// The route excludes every candidate.
    AllExcluded,
}

/// The route reason an unresolved selection was refused for, read from its outcome code
/// and the reasons recorded on its candidate rows.
pub fn route_refusal(outcome: &crate::SelectionOutcome) -> Option<RouteRefusal> {
    if outcome.resolved.is_some() {
        return None;
    }
    if outcome
        .reason_codes
        .iter()
        .any(|c| c == "route_provider_pinned")
    {
        return Some(RouteRefusal::ProviderPinned);
    }
    let excluded = |t: &crate::CandidateTrace| {
        t.state == crate::CandidateState::PolicyDenied
            && (t.reason_codes.first()).is_some_and(|r| r.starts_with("route_excludes_"))
    };
    (!outcome.traces.is_empty() && outcome.traces.iter().all(excluded))
        .then_some(RouteRefusal::AllExcluded)
}

/// Convert both current and legacy route lists into a stable preference key.
/// Equally preferred candidates retain their configured profile order.
pub(crate) fn preference(
    route: &RouteLimits,
    provider: &str,
    harness: &str,
) -> (usize, usize, usize) {
    let position = |list: &[String], value: &str| {
        list.iter()
            .position(|entry| entry == value)
            .unwrap_or(usize::MAX)
    };
    (
        position(&route.providers, provider),
        position(&route.prefer, harness),
        position(&route.harnesses, harness),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_names_order_preferences_without_excluding_unlisted_candidates() {
        let route = RouteLimits {
            providers: vec!["first".into(), "second".into()],
            prefer: vec!["other-harness".into()],
            harnesses: vec!["historical-harness".into()],
            pinned: true,
        };
        let mut entries = [
            ("other", "unlisted"),
            ("second", "other-harness"),
            ("first", "unlisted"),
            ("first", "historical-harness"),
            ("first", "other-harness"),
        ];
        entries.sort_by_key(|(p, h)| preference(&route, p, h));
        assert_eq!(
            entries,
            [
                ("first", "other-harness"),
                ("first", "historical-harness"),
                ("first", "unlisted"),
                ("second", "other-harness"),
                ("other", "unlisted")
            ]
        );
    }
}
