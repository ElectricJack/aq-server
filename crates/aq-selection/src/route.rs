//! A task's route in selection (spec 0011 §4): the route's harness and provider
//! limits. The level a routed task runs on is the launch's own (spec 0014).
use crate::RouteLimits;

/// Why selection served a routed task with no candidate for a route reason (spec 0011 §4).
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

/// Whether `allowed` is a non-empty allowlist without `value`.
fn outside(allowed: &[String], value: &str) -> bool {
    !allowed.is_empty() && !allowed.iter().any(|a| a == value)
}

/// The reason a route excludes a candidate on `harness` from `provider`, if it does.
/// An empty allowlist restricts nothing.
pub(crate) fn excludes(route: &RouteLimits, harness: &str, provider: &str) -> Option<&'static str> {
    if outside(&route.harnesses, harness) {
        return Some("route_excludes_harness");
    }
    excludes_provider(route, provider)
}

/// The provider allowlist alone: for a candidate whose harness is unknown.
pub(crate) fn excludes_provider(route: &RouteLimits, provider: &str) -> Option<&'static str> {
    outside(&route.providers, provider).then_some("route_excludes_provider")
}

/// Rows on a preferred harness first; a stable sort keeps profile order otherwise.
pub(crate) fn preferred_first<T>(
    route: &RouteLimits,
    rows: &mut [T],
    harness: impl Fn(&T) -> &str,
) {
    if !route.prefer.is_empty() {
        rows.sort_by_key(|row| !route.prefer.iter().any(|p| p == harness(row)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_limits_exclude_nothing_and_harness_is_checked_before_provider() {
        let open = RouteLimits::default();
        assert_eq!(excludes(&open, "codex", "openai"), None);
        let route = RouteLimits {
            harnesses: vec!["codex".into()],
            providers: vec!["openai".into()],
            ..RouteLimits::default()
        };
        assert_eq!(excludes(&route, "codex", "openai"), None);
        assert_eq!(
            excludes(&route, "claude", "anthropic"),
            Some("route_excludes_harness")
        );
        assert_eq!(
            excludes(&route, "codex", "anthropic"),
            Some("route_excludes_provider")
        );
    }

    #[test]
    fn preferred_rows_move_first_in_stable_order() {
        let mut rows = ["a1", "b1", "a2", "b2"];
        let route = RouteLimits {
            prefer: vec!["b".into()],
            ..RouteLimits::default()
        };
        preferred_first(&route, &mut rows, |r| &r[..1]);
        assert_eq!(rows, ["b1", "b2", "a1", "a2"]);
        preferred_first(&RouteLimits::default(), &mut rows, |r| &r[..1]);
        assert_eq!(rows, ["b1", "b2", "a1", "a2"]);
    }
}
