//! Explicit selector policy in versioned intelligence definitions. Absence means
//! selection is unsupported, never permission to infer fallback or weaken limits.
use crate::{Result, ensure};
use aq_session_protocol::{Capability, require_capabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    MissingCredentials,
    Unavailable,
    Unsupported,
    StaleUnknown,
    PolicyDenied,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnknownCostPolicy {
    Deny,
    AllowBounded,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntelligenceBudgetPolicy {
    pub max_cost_microunits: Option<u64>,
    pub currency: Option<String>,
    pub require_complete_accounting: bool,
    pub require_hard_cost_enforcement: bool,
    pub unknown_cost: UnknownCostPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectionPolicy {
    pub schema_version: u32,
    pub fallback_on: Vec<FallbackReason>,
    pub allow_unknown_availability: bool,
    pub observation_max_age_ms: u64,
    pub max_output_tokens: u64,
    pub max_attempts: u32,
    pub max_lineage_elapsed_ms: u64,
    pub required_capabilities: Vec<Capability>,
    pub budget: IntelligenceBudgetPolicy,
}
impl SelectionPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure(self.schema_version == 1, "selection policy version")?;
        ensure(
            self.fallback_on.len() <= 5
                && self.fallback_on.iter().collect::<BTreeSet<_>>().len() == self.fallback_on.len(),
            "duplicate or excessive fallback reasons",
        )?;
        ensure(
            (1000..=3_600_000).contains(&self.observation_max_age_ms)
                && self.max_output_tokens > 0
                && self.max_output_tokens <= 1_048_576
                && (1..=32).contains(&self.max_attempts)
                && self.max_lineage_elapsed_ms > 0
                && self.max_lineage_elapsed_ms <= 604_800_000,
            "selection policy bounds",
        )?;
        require_capabilities(&self.required_capabilities, &self.required_capabilities)
            .map_err(|e| crate::Error::Invalid(e.to_string()))?;
        let budget = &self.budget;
        ensure(
            budget.max_cost_microunits.is_some() == budget.currency.is_some()
                && (!budget.require_hard_cost_enforcement || budget.max_cost_microunits.is_some()),
            "selection cost basis",
        )?;
        if let Some(currency) = &budget.currency {
            ensure(
                currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
                "selection currency",
            )?;
        }
        ensure(
            !budget.require_hard_cost_enforcement
                || (budget.require_complete_accounting
                    && budget.unknown_cost == UnknownCostPolicy::Deny),
            "hard cost policy must require complete known accounting",
        )
    }
}
