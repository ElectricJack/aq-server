//! Selection for a live offer (spec 0014 §5.2): among the candidates of the
//! launch a role snapshot resolved, on the harnesses the node has installed,
//! whatever models those harnesses list.
mod common;
use aq_profiles::roles::{
    DocumentLimits, Kind, SelectionDefaults, Snapshot, SourceDocument, compile,
};
use aq_selection::*;
use common::*;

const HARNESS: &str = "opencode";
fn document(kind: Kind, id: &str, text: String) -> SourceDocument {
    SourceDocument {
        kind,
        label: id.into(),
        text,
    }
}
/// One role on one execution profile, and a level with `candidates`
/// (`provider, model, reasoning`) on the node's harness.
fn snapshot(candidates: &[(&str, &str, &str)], defaults: &SelectionDefaults) -> Snapshot {
    let rows: String = (candidates.iter())
        .map(|(p, m, r)| format!("| {p} | {m} | {r} | {HARNESS} |\n"))
        .collect();
    let documents = [
        document(Kind::Role, "developer", "---\nid: developer\nname: Developer\ndescription: Makes changes.\nexecution: sandbox\noperations: [task.read]\nenabled: true\n---\n## Role\nYou make the change.\n".into()),
        document(Kind::Execution, "sandbox", format!("---\nid: sandbox\nname: Sandbox\ndescription: The fixture node.\nharnesses: [{HARNESS}]\nenvironment: confined-worktree\nconfinement: native\n---\n## Credentials\n- {HARNESS}: account-a\n\n## Limits\n- wall_time_ms: 60000\n- memory_bytes: 268435456\n- cpu_millis: 60000\n- process_limit: 8\n- startup_context_bytes: 8192\n- max_output_tokens: 128\n")),
        document(Kind::Intelligence, "standard-high", format!("---\nid: standard-high\nname: Standard, high\ndescription: Ordinary work.\n---\n## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n{rows}")),
    ];
    compile(&documents, defaults, &DocumentLimits::default())
        .unwrap_or_else(|problems| panic!("{problems:?}"))
}
fn defaults() -> SelectionDefaults {
    SelectionDefaults {
        observation_max_age_ms: 60_000,
        ..SelectionDefaults::default()
    }
}
/// The shared fixture's inputs as a launch of `developer` on `standard-high`:
/// the node's harness is the execution profile's, with the capabilities that
/// profile requires and NO model list.
fn offer_inputs(snapshot: &Snapshot) -> SelectionInputs {
    let (_, mut input) = fixture();
    let launch = snapshot.resolve("developer", "standard-high").unwrap();
    input.project.role = launch.role.id.clone();
    input.project.intelligence_profile = launch.intelligence.id.clone();
    input.project.execution_profile = launch.execution.id.clone();
    input.project.role_snapshot_digest = serde_json::from_value(snapshot.digest().into()).unwrap();
    input.node.harnesses.truncate(1);
    for harness in &mut input.node.harnesses {
        harness.harness = HARNESS.into();
        harness.capabilities = launch.execution.required_capabilities();
    }
    input.launches = vec![PreparedLaunch {
        digest: digest(&"startup text").unwrap(),
        harness: HARNESS.into(),
        environment: "confined-worktree".into(),
        startup_bytes: 400,
        startup_limit_bytes: 8192,
    }];
    let expiry = input.now_ms + 60_000;
    input.node.expires_at_ms = expiry;
    input.project.expires_at_ms = expiry;
    input.budget.expires_at_ms = expiry;
    for binding in &mut input.node.credential_bindings {
        binding.expires_at_ms = expiry;
    }
    for observation in &mut input.observations {
        observation.expires_at_ms = expiry;
    }
    input
}
fn selected(snapshot: &Snapshot, input: &SelectionInputs) -> SelectionOutcome {
    let launch = snapshot.resolve("developer", "standard-high").unwrap();
    select_for_offer(&launch, input).unwrap()
}

#[test]
fn a_model_no_harness_lists_runs_on_an_installed_harness_with_its_stated_reasoning() {
    let snapshot = snapshot(
        &[("fixture-a", "a-model-no-harness-lists", "high")],
        &defaults(),
    );
    let mut input = offer_inputs(&snapshot);
    for observation in &mut input.observations {
        observation.scope.model = None;
    }
    let outcome = selected(&snapshot, &input);
    let resolved = outcome.resolved.expect("selected");
    assert_eq!(resolved.choice.model, "a-model-no-harness-lists");
    assert_eq!(resolved.choice.native_reasoning, "high");
    assert_eq!(resolved.choice.harness, HARNESS);
    // The decision records the candidate by provider, model and reasoning,
    // the prepared launch it chose and the snapshot it was resolved from.
    assert_eq!(
        resolved.choice.profile_candidate_id,
        "fixture-a:a-model-no-harness-lists:high"
    );
    assert_eq!(resolved.choice.launch_digest, input.launches[0].digest);
    assert_eq!(resolved.role_snapshot_digest.as_str(), snapshot.digest());
    assert_eq!(resolved.choice.max_output_tokens, 128);
    // Deterministic: the same inputs give the same sealed selection.
    let again = selected(&snapshot, &input).resolved.unwrap();
    assert_eq!(digest(&resolved).unwrap(), digest(&again).unwrap());
}

#[test]
fn a_reasoning_the_harness_does_not_accept_is_unsupported_and_never_substituted() {
    // The document validator refuses such a level; a stored snapshot is
    // re-checked at selection all the same.
    let good = snapshot(&[("fixture-a", "model-a", "high")], &defaults());
    let mut content: serde_json::Value = serde_json::from_slice(good.canonical_bytes()).unwrap();
    let candidate = &mut content["intelligence"]["standard-high"]["candidates"][0];
    assert_eq!(candidate["reasoning"], "high");
    candidate["reasoning"] = "xhigh".into();
    let stored = Snapshot::from_stored_bytes(&serde_json::to_vec(&content).unwrap()).unwrap();
    let input = offer_inputs(&stored);
    let outcome = selected(&stored, &input);
    assert!(outcome.resolved.is_none());
    assert_eq!(outcome.traces[0].state, CandidateState::Unsupported);
    assert_eq!(
        outcome.traces[0].reason_codes,
        ["model_or_reasoning_unsupported"]
    );
}

#[test]
fn a_harness_the_node_lacks_or_without_the_required_capabilities_is_not_selected() {
    let snapshot = snapshot(&[("fixture-a", "model-a", "default")], &defaults());
    let mut input = offer_inputs(&snapshot);
    input.node.harnesses[0].capabilities.clear();
    let outcome = selected(&snapshot, &input);
    assert!(outcome.resolved.is_none());
    assert_eq!(
        outcome.traces[0].reason_codes,
        ["required_capability_unsupported"]
    );
    let mut input = offer_inputs(&snapshot);
    input.node.harnesses[0].harness = "claude".into();
    let outcome = selected(&snapshot, &input);
    assert!(outcome.resolved.is_none());
    assert_eq!(
        outcome.traces[0].reason_codes,
        ["compatible_installed_composition_unavailable"]
    );
}

#[test]
fn live_offer_selection_leaves_at_least_29_seconds_after_assembly_latency() {
    let snapshot = snapshot(&[("fixture-a", "model-a", "high")], &defaults());
    let input = offer_inputs(&snapshot);
    let selected = selected(&snapshot, &input).resolved.unwrap();
    assert_eq!(selected.expires_at_ms - input.now_ms, 30_000);
    let assembled_at_ms = input.now_ms + 900;
    assert!(selected.expires_at_ms - assembled_at_ms >= 29_000);
}

#[test]
fn live_offer_selection_still_expires_with_each_underlying_fact() {
    for fact in 0..6 {
        let mut defaults = defaults();
        if fact == 5 {
            // Observed at 1000, selected at 2000; age becomes stale at 3000.
            defaults.observation_max_age_ms = 1_999;
        }
        let snapshot = snapshot(&[("fixture-a", "model-a", "high")], &defaults);
        let mut input = offer_inputs(&snapshot);
        let expiry = input.now_ms + 1_000;
        match fact {
            0 => input.node.expires_at_ms = expiry,
            1 => input.project.expires_at_ms = expiry,
            2 => input.budget.expires_at_ms = expiry,
            3 => input.node.credential_bindings[0].expires_at_ms = expiry,
            4 => input.observations[0].expires_at_ms = expiry,
            _ => {}
        }
        let selected = selected(&snapshot, &input).resolved.unwrap();
        assert_eq!(selected.expires_at_ms, expiry, "fact {fact}");
    }
}

#[test]
fn a_launch_that_differs_from_the_project_snapshot_or_exceeds_its_startup_limit_is_refused() {
    let snapshot = snapshot(&[("fixture-a", "model-a", "high")], &defaults());
    let launch = snapshot.resolve("developer", "standard-high").unwrap();
    let mut other_role = offer_inputs(&snapshot);
    other_role.project.role = "reviewer".into();
    assert!(select_for_offer(&launch, &other_role).is_err());
    let mut over = offer_inputs(&snapshot);
    over.launches[0].startup_bytes = 9000;
    assert!(select_for_offer(&launch, &over).is_err());
}
