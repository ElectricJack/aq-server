use super::*;

/// Spec 0011 §4: the fixture with a second installed harness for each candidate
/// (`claude` beside `codex` for the primary, `gemini` beside `opencode` for the
/// fallback), so a route can choose between harnesses.
fn two_harnesses() -> (Fx, SelectionInputs) {
    let (mut fx, mut input) = fixture();
    for (index, (harness, credential)) in [("claude", "account-a"), ("gemini", "account-b")]
        .into_iter()
        .enumerate()
    {
        fx.intelligence.candidates[index]
            .harnesses
            .push(harness.into());
        fx.execution.harnesses.push(harness.into());
        (fx.execution.credentials).insert(harness.into(), credential.into());
        input.node.harnesses.push(installed(harness));
        input.launches.push(prepared(harness));
    }
    (fx, input)
}
fn route(edit: impl FnOnce(&mut RouteLimits)) -> Option<RouteLimits> {
    let mut route = RouteLimits::default();
    edit(&mut route);
    Some(route)
}
/// (profile candidate, harness, state, first reason) of each trace, in order.
fn rows(out: &SelectionOutcome) -> Vec<(String, String, CandidateState, String)> {
    (out.traces.iter())
        .map(|t| {
            let reason = t.reason_codes.first().cloned().unwrap_or_default();
            (
                t.profile_candidate_id.clone(),
                t.harness.clone(),
                t.state,
                reason,
            )
        })
        .collect()
}
#[test]
fn route_harness_and_provider_allowlists_deny_other_candidates_with_typed_codes() {
    let (catalog, mut input) = fixture();
    input.route = route(|r| r.harnesses = vec!["claude".into()]);
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none(), "{out:?}");
    assert_eq!(out.reason_codes, ["no_eligible_candidate"]);
    assert_eq!(route_refusal(&out), Some(RouteRefusal::AllExcluded));
    for (_, _, state, reason) in rows(&out) {
        assert_eq!(
            (state, reason.as_str()),
            (CandidateState::PolicyDenied, "route_excludes_harness")
        );
    }
    // A provider outside the allowlist is excluded, and its exclusion is not a fallback:
    // the next candidate is chosen even under a policy that permits no fallback at all.
    let mut strict = policy();
    strict.fallback_on.clear();
    let catalog = change_policy(&catalog, strict, &mut input);
    input.route = route(|r| r.providers = vec!["fixture-b".into()]);
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.as_ref().unwrap().choice.provider, "fixture-b");
    assert_eq!(
        rows(&out)[0],
        (
            PRIMARY.into(),
            "codex".into(),
            CandidateState::PolicyDenied,
            "route_excludes_provider".into()
        )
    );
    // Both allowlists naming what the candidate has leave the profile's choice unchanged.
    input.route = route(|r| {
        r.harnesses = vec!["codex".into(), "opencode".into()];
        r.providers = vec!["fixture-a".into(), "fixture-b".into()];
    });
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-a");
}
/// A node advertising both sandbox harnesses is selected for Codex when the
/// route allows only codex, and for Claude when it allows only claude.
#[test]
fn node_advertising_both_sandbox_harnesses_is_selected_for_either() {
    let (catalog, mut input) = two_harnesses();
    let catalog = change_confinement(&catalog, Confinement::Container, &mut input);
    for harness in &mut input.node.harnesses {
        harness.adapter.id = aq_session_protocol::Text::new(match harness.harness.as_str() {
            "codex" => NODE_SANDBOX_CODEX_ADAPTER,
            _ => NODE_SANDBOX_ADAPTER,
        })
        .unwrap();
        harness.capabilities = vec![aq_session_protocol::Capability {
            id: aq_session_protocol::Text::new("aq.harness.container-confinement").unwrap(),
            version: 1,
        }];
    }
    for harness in ["codex", "claude"] {
        input.route = route(|r| r.harnesses = vec![harness.into()]);
        let chosen = select(&catalog, &input).unwrap().resolved.unwrap().choice;
        assert_eq!(chosen.harness, harness);
    }
    // Without the sandbox adapter identity the Codex harness is refused.
    input.node.harnesses[0].adapter.id =
        aq_session_protocol::Text::new("aq.codex-result-local-adapter").unwrap();
    input.route = route(|r| r.harnesses = vec!["codex".into()]);
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none(), "{out:?}");
}
#[test]
fn preferred_harness_is_tried_first_and_profile_order_is_kept_otherwise() {
    let (catalog, mut input) = two_harnesses();
    let out = select(&catalog, &input).unwrap();
    let chosen = &out.resolved.as_ref().unwrap().choice;
    assert_eq!(
        (
            chosen.profile_candidate_id.as_str(),
            chosen.harness.as_str()
        ),
        (PRIMARY, "codex")
    );
    let order = |out: &SelectionOutcome| {
        rows(out)
            .into_iter()
            .map(|(c, h, _, _)| {
                format!("{}/{h}", if c == PRIMARY { "primary" } else { "fallback" })
            })
            .collect::<Vec<_>>()
    };
    let profile_order = [
        "primary/codex",
        "primary/claude",
        "fallback/opencode",
        "fallback/gemini",
    ];
    assert_eq!(order(&out), profile_order);
    input.route = route(|r| r.prefer = vec!["claude".into(), "gemini".into()]);
    let out = select(&catalog, &input).unwrap();
    let chosen = &out.resolved.as_ref().unwrap().choice;
    assert_eq!(
        (
            chosen.profile_candidate_id.as_str(),
            chosen.harness.as_str()
        ),
        (PRIMARY, "claude")
    );
    assert_eq!(
        order(&out),
        [
            "primary/claude",
            "fallback/gemini",
            "primary/codex",
            "fallback/opencode"
        ]
    );
    // A preferred harness that no candidate row has keeps the profile order exactly.
    input.route = route(|r| r.prefer = vec!["pi".into()]);
    let out = select(&catalog, &input).unwrap();
    assert_eq!(order(&out), profile_order);
}
#[test]
fn pinned_route_refuses_fallback_with_route_provider_pinned() {
    let (catalog, mut input) = fixture();
    // The first provider is in cooldown; the policy would fall back to fixture-b.
    input.observations[0].state = Availability::QuotaUnavailable;
    input.observations[0].cooldown_until_ms = 5000;
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
    input.route = route(|r| r.pinned = true);
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none(), "{out:?}");
    assert_eq!(out.reason_codes, ["route_provider_pinned"]);
    assert_eq!(route_refusal(&out), Some(RouteRefusal::ProviderPinned));
    let fallback = &out.traces[1];
    assert_eq!(fallback.provider, "fixture-b");
    assert_eq!(fallback.state, CandidateState::PolicyDenied);
    assert!(
        fallback
            .reason_codes
            .contains(&"route_provider_pinned".into())
    );
    // Pinned to the first provider the route allows: an excluded provider pins nothing,
    // so with fixture-a excluded, fixture-b is that provider and is chosen.
    input.route = route(|r| {
        r.pinned = true;
        r.providers = vec!["fixture-b".into()];
    });
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
}
#[test]
fn the_server_fixture_inputs_roundtrip_and_a_route_changes_their_digest() {
    // Golden: the server's frozen fixture inputs.
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../../aq-server/tests/fixtures/selection-v2.json"
    ))
    .unwrap();
    let input: SelectionInputs = serde_json::from_value(golden["inputs"].clone()).unwrap();
    assert!(input.route.is_none());
    let encoded = serde_json::to_value(&input).unwrap();
    assert_eq!(encoded, golden["inputs"]);
    assert_eq!(
        aq_selection::digest(&input).unwrap(),
        aq_selection::digest(&golden["inputs"]).unwrap()
    );
    assert!(encoded.get("route").is_none());
    // A route that restricts nothing still changes the snapshot it is part of.
    let mut routed = input.clone();
    routed.route = Some(RouteLimits::default());
    assert_ne!(
        aq_selection::digest(&routed).unwrap(),
        aq_selection::digest(&input).unwrap()
    );
}
/// The fixture whose first candidate (`primary`, fixture-a) lists only a harness the
/// execution profile may use but the node has not installed, so it has no installed
/// launch (a placeholder row); `fallback` (fixture-b) stays eligible. `fallback_on` is
/// the policy's.
fn primary_not_installed(fallback_on: Vec<FallbackReason>) -> (Fx, SelectionInputs) {
    let (mut fx, input) = fixture();
    fx.intelligence.candidates[0].harnesses = vec!["claude".into()];
    fx.execution.harnesses.push("claude".into());
    (fx.execution.credentials).insert("claude".into(), "account-a".into());
    fx.policy.fallback_on = fallback_on;
    (fx, input)
}
#[test]
fn pinned_route_is_pinned_by_a_first_provider_without_an_installed_composition() {
    let (catalog, mut input) = primary_not_installed(policy().fallback_on);
    let placeholder = (
        PRIMARY.to_string(),
        "unavailable".to_string(),
        CandidateState::Unsupported,
        "compatible_installed_composition_unavailable".to_string(),
    );
    // Without a route, the policy falls back to fixture-b.
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
    // Its harness is unknown, so the harness allowlist does not exclude it: it pins fixture-a.
    input.route = route(|r| {
        r.pinned = true;
        r.harnesses = vec!["codex".into(), "opencode".into()];
    });
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none(), "{out:?}");
    assert_eq!(out.reason_codes, ["route_provider_pinned"]);
    let rows = rows(&out);
    assert_eq!(rows[0], placeholder);
    assert_eq!(rows[1].0, FALLBACK);
    assert_eq!(rows[1].2, CandidateState::PolicyDenied);
    assert!(
        out.traces[1]
            .reason_codes
            .contains(&"route_provider_pinned".into())
    );
    // A provider allowlist does exclude it, as its trace says, and fixture-b is then pinned.
    input.route = route(|r| {
        r.pinned = true;
        r.providers = vec!["fixture-b".into()];
    });
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
    assert_eq!(
        (out.traces[0].state, out.traces[0].reason_codes.clone()),
        (
            CandidateState::PolicyDenied,
            vec!["route_excludes_provider".to_string()]
        )
    );
}
#[test]
fn a_first_provider_without_an_installed_composition_still_bounds_fallback_under_a_route() {
    let strict = vec![FallbackReason::Unavailable, FallbackReason::PolicyDenied];
    let (catalog, mut input) = primary_not_installed(strict);
    let plain = select(&catalog, &input).unwrap();
    assert!(plain.resolved.is_none(), "{plain:?}");
    assert_eq!(plain.reason_codes, ["no_permitted_fallback"]);
    assert_eq!(
        route_refusal(&plain),
        None,
        "a policy refusal is not a route one"
    );
    input.route = route(|r| r.harnesses = vec!["opencode".into()]);
    let routed = select(&catalog, &input).unwrap();
    assert!(routed.resolved.is_none(), "{routed:?}");
    assert_eq!(routed.reason_codes, plain.reason_codes);
    assert_eq!(
        rows(&routed),
        rows(&plain),
        "the trace matches the decision"
    );
    assert_eq!(rows(&routed)[0].2, CandidateState::Unsupported);
    assert!(
        routed.traces[1]
            .reason_codes
            .contains(&"fallback_not_permitted".into())
    );
}

#[test]
fn capacity_refusal_respects_the_profiles_unavailable_fallback_policy() {
    let (catalog, mut input) = fixture();
    input.route = route(|r| r.providers = vec!["fixture-a".into(), "fixture-b".into()]);
    let capacity =
        std::collections::BTreeMap::from([("fixture-a".into(), "swarm_concurrency_limit_reached")]);

    let permitted = select_for_offer_with_capacity(&catalog.launch(), &input, &capacity).unwrap();
    assert_eq!(permitted.resolved.unwrap().choice.provider, "fixture-b");

    let mut strict = policy();
    strict
        .fallback_on
        .retain(|reason| *reason != FallbackReason::Unavailable);
    let catalog = change_policy(&catalog, strict, &mut input);
    assert_eq!(
        select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap()
            .choice
            .provider,
        "fixture-a"
    );
    let refused = select_for_offer_with_capacity(&catalog.launch(), &input, &capacity).unwrap();
    assert!(refused.resolved.is_none(), "{refused:?}");
    assert_eq!(refused.traces[0].state, CandidateState::Unavailable);
    assert!(
        refused.traces[0]
            .reason_codes
            .iter()
            .any(|reason| reason == "swarm_concurrency_limit_reached")
    );
    assert!(
        refused
            .reason_codes
            .iter()
            .any(|reason| reason == "no_permitted_fallback")
    );
}
