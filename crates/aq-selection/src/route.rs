//! A task's route in selection (spec 0011 §4): the class profile in place of the
//! automatic router, and the route's harness and provider limits.
use crate::RouteLimits;
use aq_profiles::{Catalog, Intelligence, key};

fn profile<'a>(catalog: &'a Catalog, reference: &str) -> Option<&'a Intelligence> {
    (catalog.intelligence.iter()).find(|i| key(&i.id, i.version) == reference)
}

/// Whether `reference` names an automatic router in `catalog`.
pub fn is_automatic_router(catalog: &Catalog, reference: &str) -> bool {
    profile(catalog, reference).is_some_and(|i| i.automatic_routing == Some(true))
}

/// The intelligence profile a selection uses: `routed` in place of the agent's
/// own, which must then be an automatic router while `routed` is not one. `None`
/// when that does not hold or a profile is absent.
pub fn routed_intelligence<'a>(
    catalog: &'a Catalog,
    agent: &str,
    routed: Option<&str>,
) -> Option<&'a Intelligence> {
    let Some(routed) = routed else {
        return profile(catalog, agent);
    };
    if !is_automatic_router(catalog, agent) || is_automatic_router(catalog, routed) {
        return None;
    }
    profile(catalog, routed)
}

/// The automatic router a launch on `launched` stands in for: `agent`'s intelligence
/// profile when that is an automatic router and `launched` is another profile that
/// [`routed_intelligence`] accepts in its place. `None` for an unrouted launch.
pub fn router_of_launch<'a>(catalog: &'a Catalog, agent: &str, launched: &str) -> Option<&'a str> {
    let agent = (catalog.agents.iter()).find(|a| key(&a.id, a.version) == agent)?;
    let router = agent.intelligence.as_str();
    (router != launched && routed_intelligence(catalog, router, Some(launched)).is_some())
        .then_some(router)
}

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
            excludes(&route, "claude-code", "anthropic"),
            Some("route_excludes_harness")
        );
        assert_eq!(
            excludes(&route, "codex", "anthropic"),
            Some("route_excludes_provider")
        );
    }

    fn catalog() -> Catalog {
        let raw = include_str!("../tests/fixtures/pre-selection-policy-catalog.json");
        let mut c = aq_profiles::ValidatedCatalog::from_artifact(raw.as_bytes())
            .unwrap()
            .artifact()
            .catalog
            .clone();
        let mut class = c.intelligence[0].clone();
        class.id = "standard-high".into();
        c.intelligence[0].automatic_routing = Some(true);
        c.intelligence.push(class);
        c
    }

    #[test]
    fn a_class_profile_stands_in_only_for_an_automatic_router() {
        let mut c = catalog();
        let (agent, router) = ("implementer@1", "engineering@1");
        assert!(is_automatic_router(&c, router));
        assert!(!is_automatic_router(&c, "standard-high@1"));
        assert_eq!(
            routed_intelligence(&c, router, None).unwrap().id,
            "engineering"
        );
        let routed = routed_intelligence(&c, router, Some("standard-high@1"));
        assert_eq!(routed.unwrap().id, "standard-high");
        // Never the router in place of the router, never an absent profile.
        assert!(routed_intelligence(&c, router, Some(router)).is_none());
        assert!(routed_intelligence(&c, router, Some("deep-max@1")).is_none());
        assert_eq!(router_of_launch(&c, agent, "standard-high@1"), Some(router));
        assert_eq!(router_of_launch(&c, agent, router), None, "unrouted");
        assert_eq!(router_of_launch(&c, agent, "deep-max@1"), None);
        assert_eq!(router_of_launch(&c, "absent@1", "standard-high@1"), None);
        // A binding pinned to a named profile takes no class profile in its place.
        c.intelligence[0].automatic_routing = None;
        assert!(routed_intelligence(&c, router, Some("standard-high@1")).is_none());
        assert_eq!(router_of_launch(&c, agent, "standard-high@1"), None);
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
