use super::*;

#[test]
fn lineage_retry_cost_and_replacement_budgets_cannot_reset_on_fallback() {
    let (catalog, mut input) = fixture();
    input.budget.attempts_started = 3;
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    input.budget.attempts_started = 1;
    input.budget.last_attempt_id = Some(Uuid::from_u128(6));
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"replacement_stop_retention_reconciliation_required".into())
    );
    input.replacement = Some(ReplacementEvidence {
        lineage: input.project.lineage.clone(),
        predecessor_attempt_id: Uuid::from_u128(6),
        stop_receipt: artifact("stop-receipt"),
        retained_source_receipt: artifact("retained-source"),
        effect_reconciliation_receipt: artifact("effects-reconciled"),
        observed_at_ms: 1000,
        expires_at_ms: 5000,
    });
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    // Time already used limits nothing: each attempt has its own window.
    input.budget.elapsed_ms = Some(u64::MAX);
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    input.budget.elapsed_ms = None;
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
}
/// Spec 0007 Q1: owner and service handoffs are not charged to the policy's
/// attempt limit; every other started attempt still is.
#[test]
fn exempt_attempts_do_not_count_against_policy_max_attempts() {
    let (catalog, mut input) = fixture();
    input.budget.attempts_started = 3;
    input.budget.exempt_attempts = 2;
    input.budget.last_attempt_id = Some(Uuid::from_u128(6));
    input.replacement = Some(ReplacementEvidence {
        lineage: input.project.lineage.clone(),
        predecessor_attempt_id: Uuid::from_u128(6),
        stop_receipt: artifact("stop-receipt"),
        retained_source_receipt: artifact("retained-source"),
        effect_reconciliation_receipt: artifact("effects-reconciled"),
        observed_at_ms: 1000,
        expires_at_ms: 5000,
    });
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    input.budget.exempt_attempts = 0;
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"lineage_retry_budget_exhausted".into())
    );
}
#[test]
fn budget_snapshot_without_exempt_serializes_unchanged() {
    let (_, mut input) = fixture();
    let plain = serde_json::to_value(&input.budget).unwrap();
    assert!(plain.get("exempt_attempts").is_none());
    let back: BudgetSnapshot = serde_json::from_value(plain).unwrap();
    assert_eq!(back.exempt_attempts, 0);
    input.budget.exempt_attempts = 1;
    assert_eq!(
        serde_json::to_value(&input.budget).unwrap()["exempt_attempts"],
        1
    );
}
#[test]
fn ledger_unknown_is_not_zero_and_hard_cost_requires_qualified_provider_and_adapter() {
    let (catalog, mut input) = fixture();
    input.project.ceilings.cost_limit_microunits = Some(100);
    input.project.ceilings.currency = Some("USD".into());
    assert!(select(&catalog, &input).unwrap().resolved.is_none());
    let mut policy = policy();
    policy.budget.unknown_cost = UnknownCostPolicy::AllowBounded;
    let catalog = change_policy(&catalog, policy, &mut input);
    assert!(select(&catalog, &input).unwrap().resolved.is_some());
    input.project.ceilings.require_hard_cost_enforcement = true;
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"hard_cost_enforcement_unqualified".into())
    );
}
#[test]
fn unknown_cost_never_bypasses_known_exhaustion_or_incompatible_currency() {
    let (catalog, mut input) = fixture();
    input.project.ceilings.cost_limit_microunits = Some(100);
    input.project.ceilings.currency = Some("USD".into());
    let mut policy = policy();
    policy.budget.unknown_cost = UnknownCostPolicy::AllowBounded;
    let catalog = change_policy(&catalog, policy, &mut input);
    for spent in [100, 101] {
        input.budget.spent_microunits = Some(spent);
        input.budget.currency = Some("USD".into());
        input.budget.accounting = AccountingCompleteness::Incomplete;
        let out = select(&catalog, &input).unwrap();
        assert!(out.resolved.is_none());
        assert!(
            out.traces[0]
                .reason_codes
                .contains(&"lineage_cost_budget_exhausted".into())
        );
    }
    input.budget.spent_microunits = Some(1);
    input.budget.currency = Some("EUR".into());
    let out = select(&catalog, &input).unwrap();
    assert!(out.resolved.is_none());
    assert!(
        out.traces[0]
            .reason_codes
            .contains(&"budget_currency_mismatch".into())
    );
}
