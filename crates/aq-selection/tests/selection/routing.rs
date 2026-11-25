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
fn legacy_provider_and_harness_filters_are_queued_preferences() {
    let (catalog, mut input) = fixture();
    input.route = route(|r| {
        r.pinned = true;
        r.providers = vec!["fixture-b".into()];
        r.harnesses = vec!["opencode".into()];
    });
    let choice = select(&catalog, &input).unwrap().resolved.unwrap().choice;
    assert_eq!(choice.provider, "fixture-b");

    input.observations[1].state = Availability::QuotaUnavailable;
    input.observations[1].cooldown_until_ms = 5000;
    let choice = select(&catalog, &input).unwrap().resolved.unwrap().choice;
    assert_eq!(choice.provider, "fixture-a");
}

/// An advertised and qualified sandbox harness can honor the queued route's preference.
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
    let choice = select(&catalog, &input).unwrap().resolved.unwrap().choice;
    assert_ne!(choice.harness, "codex");
    input.node.harnesses.retain(|h| h.harness == "codex");
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
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
fn unavailable_composition_does_not_turn_a_legacy_preference_into_a_pin() {
    let (catalog, mut input) = primary_not_installed(Vec::new());
    input.route = route(|r| {
        r.pinned = true;
        r.providers = vec!["fixture-a".into()];
        r.harnesses = vec!["claude".into()];
    });
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.provider, "fixture-b");
    assert!(out.traces.iter().any(|trace| {
        trace.provider == "fixture-a" && trace.state == CandidateState::Unsupported
    }));
}

#[test]
fn capacity_can_use_another_provider_under_a_strict_queued_profile() {
    let (catalog, mut input) = fixture();
    input.route = route(|r| r.providers = vec!["fixture-a".into(), "fixture-b".into()]);
    let capacity =
        std::collections::BTreeMap::from([("fixture-a".into(), "swarm_concurrency_limit_reached")]);
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
    let queued = select_for_offer_with_capacity(&catalog.launch(), &input, &capacity).unwrap();
    assert_eq!(queued.resolved.unwrap().choice.provider, "fixture-b");
    assert!(queued.traces.iter().any(|trace| {
        trace.provider == "fixture-a"
            && trace.state == CandidateState::Unavailable
            && trace
                .reason_codes
                .iter()
                .any(|r| r == "swarm_concurrency_limit_reached")
    }));

    input.credentials[1].enabled = false;
    let blocked = select_for_offer_with_capacity(&catalog.launch(), &input, &capacity).unwrap();
    assert!(blocked.resolved.is_none(), "{blocked:?}");
}

#[test]
fn an_unavailable_preferred_model_does_not_hide_an_eligible_model_of_that_provider() {
    let (mut catalog, mut input) = fixture();
    let mut alternate = catalog.intelligence.candidates[0].clone();
    alternate.model = "model-a-alternate".into();
    catalog.intelligence.candidates.insert(1, alternate);
    catalog.policy.fallback_on.clear();
    catalog.policy.allow_unknown_availability = true;
    input.observations[0].state = Availability::QuotaUnavailable;
    input.observations[0].cooldown_until_ms = 5000;
    input.route = route(|r| r.providers = vec!["fixture-a".into(), "fixture-b".into()]);
    let out = select(&catalog, &input).unwrap();
    let choice = out.resolved.unwrap().choice;
    assert_eq!(choice.provider, "fixture-a");
    assert_eq!(choice.model, "model-a-alternate");
}
