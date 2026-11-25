mod common;
use aq_profiles::roles::Confinement;
use aq_profiles::{ExecutionConfinement, FallbackReason, UnknownCostPolicy};
use aq_selection::*;
use common::*;
use uuid::Uuid;

#[path = "selection/budget.rs"]
mod budget;
#[path = "selection/credentials.rs"]
mod credentials;
#[path = "selection/evidence.rs"]
mod evidence;
#[path = "selection/placement.rs"]
mod placement;
#[path = "selection/routing.rs"]
mod routing;

#[test]
fn stable_order_chooses_exact_native_configuration_without_equating_reasoning_controls() {
    let (catalog, input) = fixture();
    let first = select(&catalog, &input).unwrap();
    let selected = first.resolved.unwrap();
    assert_eq!(selected.choice.model, "model-a");
    assert_eq!(selected.choice.native_reasoning, "high");
    let mut shuffled = input.clone();
    shuffled.credentials.reverse();
    shuffled.observations.reverse();
    assert_eq!(
        select(&catalog, &shuffled)
            .unwrap()
            .resolved
            .unwrap()
            .choice
            .candidate_id,
        selected.choice.candidate_id
    );
}
#[test]
fn explicit_fallback_and_cooldown_do_not_implicitly_downgrade() {
    let (catalog, mut input) = fixture();
    input.observations[0].state = Availability::QuotaUnavailable;
    input.observations[0].cooldown_until_ms = 5000;
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.model, "model-b");
    assert_eq!(out.traces[0].state, CandidateState::Unavailable);
    let mut policy = policy();
    policy.fallback_on.clear();
    let changed = change_policy(&catalog, policy, &mut input);
    let out = select(&changed, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(out.reason_codes.contains(&"no_permitted_fallback".into()));
}
/// What the server does to a Codex credential whose stored login is past the owner's
/// age limit (`aq_login_store::mask_stale`): the candidate is missing its credential and
/// the next one is chosen, when the level permits that fallback.
#[test]
fn a_credential_that_cannot_be_provisioned_falls_back_to_the_next_candidate() {
    let (catalog, mut input) = fixture();
    let first = select(&catalog, &input).unwrap().resolved.unwrap().choice;
    assert_eq!(first.model, "model-a");
    for credential in &mut input.credentials {
        credential.provisionable &= credential.profile != first.credential_profile;
    }
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.resolved.unwrap().choice.model, "model-b");
    assert_eq!(out.traces[0].state, CandidateState::MissingCredentials);
    let mut policy = policy();
    policy
        .fallback_on
        .retain(|r| *r != aq_profiles::FallbackReason::MissingCredentials);
    let narrowed = change_policy(&catalog, policy, &mut input);
    assert!(select(&narrowed, &input).unwrap().resolved.is_none());
}
#[test]
fn unknown_availability_can_only_allow_bounded_attempt_with_present_credentials() {
    let (catalog, mut input) = fixture();
    input.observations.clear();
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    let mut policy = policy();
    policy.allow_unknown_availability = true;
    let catalog = change_policy(&catalog, policy, &mut input);
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    input
        .credentials
        .iter_mut()
        .for_each(|c| c.provisionable = false);
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces
            .iter()
            .all(|t| t.state == CandidateState::MissingCredentials)
    );
}
#[test]
fn installed_credential_and_central_authorization_are_independent() {
    let (catalog, mut input) = fixture();
    input.node.credential_bindings.clear();
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    let (catalog, mut input) = fixture();
    input.credentials[0].enabled = false;
    let out = select(&catalog, &input).unwrap();
    assert_eq!(out.traces[0].state, CandidateState::PolicyDenied);
    assert_eq!(out.resolved.unwrap().choice.model, "model-b");
}
#[test]
fn stale_node_and_unsupported_native_settings_never_become_unknown_availability_attempts() {
    let (catalog, mut input) = fixture();
    let mut policy = policy();
    policy.allow_unknown_availability = true;
    let catalog = change_policy(&catalog, policy, &mut input);
    input.node.expires_at_ms = 1500;
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    // A reasoning effort the harness does not accept is never substituted.
    let (mut catalog, input) = fixture();
    for candidate in &mut catalog.intelligence.candidates {
        candidate.reasoning = "ultra".into();
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    let (catalog, mut input) = fixture();
    for harness in &mut input.node.harnesses {
        harness.limits.max_output_tokens = 10;
    }
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
}
#[test]
fn fallback_unsupported_is_explicit_and_provider_specific_reasoning_remains_native() {
    let (mut catalog, mut input) = fixture();
    // `codex` does not accept `xhigh`; the fallback keeps its own stated effort.
    catalog.intelligence.candidates[0].reasoning = "xhigh".into();
    let mut policy = policy();
    policy.fallback_on = vec![FallbackReason::Unsupported];
    let catalog = change_policy(&catalog, policy, &mut input);
    let out = select(&catalog, &input).unwrap();
    assert_eq!(
        out.traces[0].reason_codes,
        ["model_or_reasoning_unsupported"]
    );
    assert_eq!(out.resolved.unwrap().choice.native_reasoning, "default");
}
#[test]
fn project_environment_allowlist_cannot_be_widened_by_installed_qualified_adapter() {
    let (catalog, mut input) = fixture();
    input.project.allowed_environments = vec!["another-qualified-environment".into()];
    let result = select(&catalog, &input).unwrap();
    assert!(result.resolved.is_none());
    assert!(result.traces.iter().any(|t| {
        t.reason_codes
            .iter()
            .any(|r| r == "project_environment_denied")
    }));
}
