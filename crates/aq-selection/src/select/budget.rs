use crate::validate::fresh;
use crate::*;
use aq_profiles::{SelectionPolicy, UnknownCostPolicy};
pub(super) fn global_budget(
    input: &SelectionInputs,
    policy: &SelectionPolicy,
) -> Option<&'static str> {
    let budget = &input.budget;
    if input.now_ms >= input.project.expires_at_ms {
        return Some("project_admission_snapshot_stale");
    }
    if !fresh(budget.observed_at_ms, budget.expires_at_ms, input.now_ms) {
        return Some("lineage_budget_stale");
    }
    if budget
        .attempts_started
        .saturating_sub(budget.exempt_attempts)
        >= policy.max_attempts
    {
        return Some("lineage_retry_budget_exhausted");
    }
    replacement_budget(input)
}

fn replacement_budget(input: &SelectionInputs) -> Option<&'static str> {
    let budget = &input.budget;
    if budget.attempts_started == 0 {
        return None;
    }
    let Some(predecessor) = budget.last_attempt_id else {
        return Some("lineage_predecessor_unknown");
    };
    let reconciled = input.replacement.as_ref().is_some_and(|e| {
        e.predecessor_attempt_id == predecessor
            && fresh(e.observed_at_ms, e.expires_at_ms, input.now_ms)
    });
    (!reconciled).then_some("replacement_stop_retention_reconciliation_required")
}
pub(super) fn cost(
    input: &SelectionInputs,
    policy: &SelectionPolicy,
    harness: &InstalledHarness,
    estimate: Option<&CandidateEstimate>,
) -> CostResult {
    let project = &input.project.ceilings;
    let p = &policy.budget;
    let budget = &input.budget;
    let hard = project.require_hard_cost_enforcement || p.require_hard_cost_enforcement;
    check_accounting(budget, harness, hard, p.require_complete_accounting)?;
    if project
        .currency
        .as_ref()
        .zip(p.currency.as_ref())
        .is_some_and(|(project, policy)| project != policy)
    {
        return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
    }
    let limits = [project.cost_limit_microunits, p.max_cost_microunits];
    let Some(limit) = limits.into_iter().flatten().min() else {
        return Ok(());
    };
    CostLimit {
        limit,
        currency: project.currency.as_ref().or(p.currency.as_ref()),
        allow_unknown: !hard
            && !p.require_complete_accounting
            && p.unknown_cost == UnknownCostPolicy::AllowBounded,
    }
    .check(budget, estimate)
}

fn check_accounting(
    budget: &BudgetSnapshot,
    harness: &InstalledHarness,
    hard: bool,
    require_complete: bool,
) -> CostResult {
    if hard && !budget.can_enforce_cost(harness) {
        return Err((
            CandidateState::Unsupported,
            "hard_cost_enforcement_unqualified",
        ));
    }
    if require_complete && budget.accounting != AccountingCompleteness::Complete {
        return Err((CandidateState::PolicyDenied, "complete_accounting_required"));
    }
    Ok(())
}
impl BudgetSnapshot {
    fn can_enforce_cost(&self, harness: &InstalledHarness) -> bool {
        harness.limits.hard_cost_enforcer.is_some()
            && self.hard_budget_provider.is_some()
            && self.accounting == AccountingCompleteness::Complete
    }
}

type CostResult = std::result::Result<(), (CandidateState, &'static str)>;

struct CostLimit<'a> {
    limit: u64,
    currency: Option<&'a String>,
    allow_unknown: bool,
}

impl CostLimit<'_> {
    fn check(&self, budget: &BudgetSnapshot, estimate: Option<&CandidateEstimate>) -> CostResult {
        let known_currencies = [budget.currency.as_ref(), estimate.map(|e| &e.currency)];
        if known_currencies
            .into_iter()
            .flatten()
            .any(|c| Some(c) != self.currency)
        {
            return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
        }
        if budget
            .spent_microunits
            .is_some_and(|spent| spent >= self.limit)
        {
            return Err((
                CandidateState::PolicyDenied,
                "lineage_cost_budget_exhausted",
            ));
        }
        match (budget.spent_microunits, estimate, budget.accounting) {
            (Some(spent), Some(estimate), AccountingCompleteness::Complete) => {
                self.check_total(budget, spent, estimate)
            }
            _ if self.allow_unknown => Ok(()),
            _ => Err((CandidateState::PolicyDenied, "cost_unknown")),
        }
    }

    fn check_total(
        &self,
        budget: &BudgetSnapshot,
        spent: u64,
        estimate: &CandidateEstimate,
    ) -> CostResult {
        if [budget.currency.as_ref(), Some(&estimate.currency)] != [self.currency; 2] {
            return Err((CandidateState::PolicyDenied, "budget_currency_mismatch"));
        }
        if spent
            .checked_add(estimate.microunits)
            .is_none_or(|sum| sum > self.limit)
        {
            return Err((
                CandidateState::PolicyDenied,
                "lineage_cost_budget_exhausted",
            ));
        }
        Ok(())
    }
}
