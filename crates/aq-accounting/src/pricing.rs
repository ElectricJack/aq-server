use crate::{Category, Error, Layout, Result, wire};
use serde::{Deserialize, Serialize};
use wire::{Cost, EstimateBasis, UnknownReason, Validate};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub category: Category,
    pub subtract: Vec<Category>,
    pub microunits_per_million_tokens: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCard {
    pub reference: wire::ArtifactRef,
    pub allocation_policy: wire::ArtifactRef,
    pub currency: wire::Text,
    pub provider: wire::Text,
    pub model: wire::Text,
    pub layout: Layout,
    pub rules: Vec<Rule>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    pub cost: Cost,
    /// Explicit non-invoice lower bound, never a complete budget assertion.
    pub known_lower_bound: Cost,
    pub missing_categories: Vec<Category>,
    pub source_incomplete: bool,
}
impl Category {
    fn known_count(
        self,
        counters: &wire::Counters,
        missing: &mut std::collections::BTreeSet<Category>,
    ) -> Option<u64> {
        match self.get(counters) {
            wire::Counter::Known { value } => Some(*value),
            wire::Counter::Unknown { .. } => {
                missing.insert(self);
                None
            }
        }
    }
}
impl Rule {
    fn priced_atoms(&self, layout: &Layout) -> Result<u8> {
        if self.subtract.len() > 3 {
            return Err(Error::Invalid);
        }
        let base = layout.atoms(self.category);
        let mut subtract = 0;
        for category in &self.subtract {
            let atoms = layout.atoms(*category);
            if atoms & base != atoms || atoms & subtract != 0 {
                return Err(Error::Invalid);
            }
            subtract |= atoms;
        }
        let priced = base & !subtract;
        if priced == 0 {
            return Err(Error::Invalid);
        }
        Ok(priced)
    }
    fn count(
        &self,
        counters: &wire::Counters,
        missing: &mut std::collections::BTreeSet<Category>,
    ) -> Result<Option<u64>> {
        let mut count = self.category.known_count(counters, missing);
        for c in &self.subtract {
            count = match (count, c.known_count(counters, missing)) {
                (Some(total), Some(value)) => {
                    Some(total.checked_sub(value).ok_or(Error::Conflict)?)
                }
                _ => None,
            };
        }
        Ok(count)
    }
}

impl RateCard {
    pub fn validate(&self) -> Result<()> {
        wire::artifact(&self.reference)?;
        wire::artifact(&self.allocation_policy)?;
        self.layout.validate()?;
        self.currency.validate()?;
        self.provider.validate()?;
        self.model.validate()?;
        if !self.valid_currency() || !(1..=5).contains(&self.rules.len()) {
            return Err(Error::Invalid);
        }
        let mut covered = 0u8;
        for rule in &self.rules {
            let atoms = rule.priced_atoms(&self.layout)?;
            if covered & atoms != 0 {
                return Err(Error::Invalid);
            }
            covered |= atoms;
        }
        // All five semantic atoms must be priced explicitly, including a zero
        // rate where appropriate. This prevents an omitted category looking free.
        if covered != 31 {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn valid_currency(&self) -> bool {
        self.currency.as_str().len() == 3
            && self
                .currency
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_uppercase())
    }
    pub fn estimate(
        &self,
        provider: &wire::Text,
        model: &wire::Text,
        counters: &wire::Counters,
    ) -> Result<Pricing> {
        self.validate()?;
        self.layout.check_counters(counters)?;
        if provider != &self.provider || model != &self.model {
            return Err(Error::Unsupported);
        }
        let mut weighted = 0u128;
        let mut missing = std::collections::BTreeSet::new();
        for rule in &self.rules {
            if rule.microunits_per_million_tokens == 0 {
                continue;
            }
            let count = rule.count(counters, &mut missing)?;
            if let Some(count) = count {
                weighted = weighted
                    .checked_add(u128::from(count) * u128::from(rule.microunits_per_million_tokens))
                    .ok_or(Error::Overflow)?;
            }
        }
        self.finish_estimate(weighted, missing)
    }
    fn finish_estimate(
        &self,
        weighted: u128,
        missing: std::collections::BTreeSet<Category>,
    ) -> Result<Pricing> {
        // Round once per canonical contribution, upwards to a microunit. All
        // calculations use integers; neither binary float nor currency guessing.
        let amount = weighted.checked_add(999_999).ok_or(Error::Overflow)? / 1_000_000;
        let amount_microunits = u64::try_from(amount).map_err(|_| Error::Overflow)?;
        let estimate = Cost::NonInvoiceEstimate {
            amount_microunits,
            currency: self.currency.clone(),
            rate_card: self.reference.clone(),
            basis: EstimateBasis::TokenRates,
            allocation_policy: self.allocation_policy.clone(),
        };
        Ok(Pricing {
            cost: if missing.is_empty() {
                estimate.clone()
            } else {
                Cost::Unknown {
                    reason: UnknownReason::Incomplete,
                }
            },
            known_lower_bound: estimate,
            missing_categories: missing.into_iter().collect(),
            source_incomplete: false,
        })
    }
}

impl RateCard {
    /// Production entry: use the canonical fact's pinned card and verified
    /// category policy. Never price raw transcript fields using a different
    /// layout merely because the model display string matches.
    pub fn estimate_fact(&self, fact: &crate::reconcile::Fact) -> Result<Pricing> {
        fact.validate()?;
        if fact.pricing_reference.as_ref() != Some(&self.reference) || fact.layout != self.layout {
            return Err(Error::Unsupported);
        }
        let mut pricing = self.estimate(
            &fact.usage.call.provider,
            &fact.usage.call.model,
            &fact.contribution,
        )?;
        if fact.source_incomplete() {
            pricing.source_incomplete = true;
            pricing.cost = Cost::Unknown {
                reason: UnknownReason::Incomplete,
            };
        }
        Ok(pricing)
    }
}

/// One owner-published revision of a rate card. A correction is a new
/// revision naming its predecessor; it never edits a pinned card in place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateCardRevision {
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corrects: Option<wire::ArtifactRef>,
    pub card: RateCard,
}

/// Validated set of rate-card revision chains, one chain per (provider, model).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RateCardBook {
    chains: Vec<Vec<RateCardRevision>>,
}

fn validate_references(revisions: &[RateCardRevision]) -> Result<()> {
    let mut references = std::collections::BTreeSet::new();
    for r in revisions {
        r.card.validate()?;
        if !references.insert(serde_json::to_string(&r.card.reference).map_err(|_| Error::Invalid)?)
        {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}
impl RateCardRevision {
    fn validate_correction(&self, previous: &Self) -> Result<()> {
        let preserved_basis = self.card.layout == previous.card.layout
            && self.card.currency == previous.card.currency;
        let follows = self.revision == previous.revision + 1
            && self.corrects.as_ref() == Some(&previous.card.reference);
        if follows && preserved_basis {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}

impl RateCardBook {
    pub fn new(mut revisions: Vec<RateCardRevision>) -> Result<Self> {
        if revisions.len() > 256 {
            return Err(Error::Invalid);
        }
        validate_references(&revisions)?;
        revisions.sort_by(|a, b| {
            (a.card.provider.as_str(), a.card.model.as_str(), a.revision).cmp(&(
                b.card.provider.as_str(),
                b.card.model.as_str(),
                b.revision,
            ))
        });
        let mut chains: Vec<Vec<RateCardRevision>> = Vec::new();
        for r in revisions {
            let chain = chains.last_mut().filter(|c| {
                c[0].card.provider == r.card.provider && c[0].card.model == r.card.model
            });
            if let Some(chain) = chain {
                r.validate_correction(chain.last().expect("chains are never empty"))?;
                chain.push(r);
            } else {
                if r.revision != 1 || r.corrects.is_some() {
                    return Err(Error::Invalid);
                }
                chains.push(vec![r]);
            }
        }
        Ok(Self { chains })
    }
    /// Newest card for a call. Ingest pins this reference on the first fact.
    pub fn current(&self, provider: &wire::Text, model: &wire::Text) -> Option<&RateCard> {
        self.chains
            .iter()
            .find(|c| &c[0].card.provider == provider && &c[0].card.model == model)
            .and_then(|c| c.last())
            .map(|r| &r.card)
    }
    /// Newest correction of the pinned card's chain, so a corrected rate
    /// re-prices the one canonical contribution instead of adding a charge.
    pub fn effective(&self, pinned: &wire::ArtifactRef) -> Option<&RateCard> {
        self.chains
            .iter()
            .find(|c| c.iter().any(|r| &r.card.reference == pinned))
            .and_then(|c| c.last())
            .map(|r| &r.card)
    }
}

impl RateCard {
    /// Price a canonical fact whose pinned card belongs to this card's
    /// revision chain. The category layout must still match exactly.
    pub fn price(&self, fact: &crate::reconcile::Fact) -> Result<Pricing> {
        let mut pinned = self.clone();
        match &fact.pricing_reference {
            Some(reference) => pinned.reference = reference.clone(),
            None => return Err(Error::Unsupported),
        }
        let mut pricing = pinned.estimate_fact(fact)?;
        for cost in [&mut pricing.cost, &mut pricing.known_lower_bound] {
            if let Cost::NonInvoiceEstimate { rate_card, .. } = cost {
                *rate_card = self.reference.clone();
            }
        }
        Ok(pricing)
    }
}

mod summary;
pub use summary::{MAX_COST_COMPONENTS, Summary, summarize};
