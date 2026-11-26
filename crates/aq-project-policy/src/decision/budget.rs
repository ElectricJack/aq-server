//! Deterministic retry and accounting projection of a frozen lineage.
use super::ReplacementBudget;
use aq_selection::{
    AccountingCompleteness, ArtifactRef, BudgetSnapshot, Digest, OFFER_SELECTION_TTL_MS,
    TaskLineage, Text,
};
/// The accounting part of a selection `BudgetSnapshot`.
#[derive(Clone, Debug)]
pub struct AccountingSnapshot {
    pub spent_microunits: Option<u64>,
    pub currency: Option<String>,
    pub accounting: aq_selection::AccountingCompleteness,
    pub accounting_source: aq_selection::ArtifactRef,
    /// Always `None`: the ledger is not a hard budget provider (0004 §2.2).
    pub hard_budget_provider: Option<aq_selection::ArtifactRef>,
}

fn unavailable_source() -> ArtifactRef {
    const ID: &str = "aq.accounting.unavailable";
    ArtifactRef {
        id: Text::new(ID).expect("constant"),
        version: Text::new("1").expect("constant"),
        digest: Digest::of(ID.as_bytes()),
    }
}
pub struct Accounting {
    pub ledger: Option<AccountingSnapshot>,
    pub cost_baseline: u64,
}
pub struct RetryHistory<'a> {
    pub started: u32,
    pub revision: u64,
    pub replacement: Option<&'a ReplacementBudget>,
}
impl Accounting {
    pub fn snapshot(
        self,
        history: RetryHistory<'_>,
        lineage: TaskLineage,
        time: u64,
    ) -> Option<BudgetSnapshot> {
        let Self {
            ledger,
            cost_baseline,
        } = self;
        let RetryHistory {
            started,
            revision,
            replacement,
        } = history;
        Some(BudgetSnapshot {
            lineage,
            revision,
            attempts_started: started,
            exempt_attempts: replacement.map_or(0, |r| r.exempt_attempts),
            last_attempt_id: replacement.map(|r| r.evidence.predecessor_attempt_id),
            elapsed_ms: match replacement {
                Some(r) => Some(r.elapsed_ms),
                None if started == 0 => Some(0),
                None => None,
            },
            // An owner override starts the cost budget again: only later spend counts.
            spent_microunits: (ledger.as_ref().and_then(|l| l.spent_microunits))
                .map(|spent| spent.saturating_sub(cost_baseline)),
            currency: ledger.as_ref().and_then(|l| l.currency.clone()),
            accounting: ledger
                .as_ref()
                .map_or(AccountingCompleteness::Unavailable, |l| l.accounting),
            accounting_source: ledger
                .as_ref()
                .map_or_else(unavailable_source, |l| l.accounting_source.clone()),
            hard_budget_provider: None,
            observed_at_ms: time,
            expires_at_ms: time.checked_add(OFFER_SELECTION_TTL_MS)?,
        })
    }
}
