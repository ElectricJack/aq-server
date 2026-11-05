mod common;
use aq_selection::*;
use common::*;

fn fresh_offer() -> (aq_profiles::ValidatedCatalog, SelectionInputs) {
    let (catalog, mut input) = fixture();
    let mut policy = policy();
    policy.observation_max_age_ms = 60_000;
    let catalog = change_policy(&catalog, policy, &mut input);
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
    (catalog, input)
}

#[test]
fn live_offer_selection_leaves_at_least_29_seconds_after_assembly_latency() {
    let (catalog, input) = fresh_offer();
    let selected = select_for_offer(&catalog, &input)
        .unwrap()
        .resolved
        .unwrap();
    assert_eq!(selected.expires_at_ms - input.now_ms, 30_000);
    let assembled_at_ms = input.now_ms + 900;
    assert!(selected.expires_at_ms - assembled_at_ms >= 29_000);
    // Existing deterministic replay consumers keep their original output.
    assert_eq!(
        select(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap()
            .expires_at_ms,
        input.now_ms + 2_000
    );
}

#[test]
fn live_offer_selection_still_expires_with_each_underlying_fact() {
    for fact in 0..6 {
        let (catalog, mut input) = fresh_offer();
        let expiry = input.now_ms + 1_000;
        let catalog = match fact {
            0 => {
                input.node.expires_at_ms = expiry;
                catalog
            }
            1 => {
                input.project.expires_at_ms = expiry;
                catalog
            }
            2 => {
                input.budget.expires_at_ms = expiry;
                catalog
            }
            3 => {
                input.node.credential_bindings[0].expires_at_ms = expiry;
                catalog
            }
            4 => {
                input.observations[0].expires_at_ms = expiry;
                catalog
            }
            _ => {
                let mut policy = policy();
                // Observed at 1000, selected at 2000; age becomes stale at 3000.
                policy.observation_max_age_ms = 1_999;
                change_policy(&catalog, policy, &mut input)
            }
        };
        let selected = select_for_offer(&catalog, &input)
            .unwrap()
            .resolved
            .unwrap();
        assert_eq!(selected.expires_at_ms, expiry, "fact {fact}");
    }
}
