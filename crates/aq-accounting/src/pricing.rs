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
impl RateCard {
    pub fn validate(&self) -> Result<()> {
        wire::artifact(&self.reference)?;
        wire::artifact(&self.allocation_policy)?;
        self.layout.validate()?;
        self.currency.validate()?;
        self.provider.validate()?;
        self.model.validate()?;
        if self.currency.as_str().len() != 3
            || !self
                .currency
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_uppercase())
            || self.rules.is_empty()
            || self.rules.len() > 5
        {
            return Err(Error::Invalid);
        }
        let mut covered = 0u8;
        for rule in &self.rules {
            if rule.subtract.len() > 3 {
                return Err(Error::Invalid);
            }
            let base = self.layout.atoms(rule.category);
            let mut subtract = 0u8;
            for c in &rule.subtract {
                let atoms = self.layout.atoms(*c);
                if atoms & base != atoms || atoms & subtract != 0 {
                    return Err(Error::Invalid);
                }
                subtract |= atoms;
            }
            let atoms = base & !subtract;
            if atoms == 0 || covered & atoms != 0 {
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
            let mut count = match rule.category.get(counters) {
                wire::Counter::Known { value } => Some(*value),
                wire::Counter::Unknown { .. } => {
                    missing.insert(rule.category);
                    None
                }
            };
            for c in &rule.subtract {
                match c.get(counters) {
                    wire::Counter::Known { value } => {
                        if let Some(v) = count {
                            count = Some(v.checked_sub(*value).ok_or(Error::Conflict)?);
                        }
                    }
                    wire::Counter::Unknown { .. } => {
                        missing.insert(*c);
                        count = None;
                    }
                }
            }
            if let Some(count) = count {
                weighted = weighted
                    .checked_add(u128::from(count) * u128::from(rule.microunits_per_million_tokens))
                    .ok_or(Error::Overflow)?;
            }
        }
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
        if fact.complete_accounting_evidence.is_none()
            || !matches!(
                fact.usage.completeness,
                wire::Completeness::ReportedComplete
            )
        {
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

impl RateCardBook {
    pub fn new(mut revisions: Vec<RateCardRevision>) -> Result<Self> {
        if revisions.len() > 256 {
            return Err(Error::Invalid);
        }
        let mut references = std::collections::BTreeSet::new();
        for r in &revisions {
            r.card.validate()?;
            if !references
                .insert(serde_json::to_string(&r.card.reference).map_err(|_| Error::Invalid)?)
            {
                return Err(Error::Invalid);
            }
        }
        revisions.sort_by(|a, b| {
            (a.card.provider.as_str(), a.card.model.as_str(), a.revision).cmp(&(
                b.card.provider.as_str(),
                b.card.model.as_str(),
                b.revision,
            ))
        });
        let mut chains: Vec<Vec<RateCardRevision>> = Vec::new();
        for r in revisions {
            match chains.last_mut() {
                Some(chain)
                    if chain[0].card.provider == r.card.provider
                        && chain[0].card.model == r.card.model =>
                {
                    let previous = chain.last().expect("chains are never empty");
                    if r.revision != previous.revision + 1
                        || r.corrects.as_ref() != Some(&previous.card.reference)
                        || r.card.layout != previous.card.layout
                        || r.card.currency != previous.card.currency
                    {
                        return Err(Error::Invalid);
                    }
                    chain.push(r);
                }
                _ => {
                    if r.revision != 1 || r.corrects.is_some() {
                        return Err(Error::Invalid);
                    }
                    chains.push(vec![r]);
                }
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

/// Bounded aggregate of canonical facts for one attempt or lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// At most eight components, as the usage view allows.
    pub costs: Vec<Cost>,
    /// Known lower-bound spend in `currency`; `None` when currencies differ.
    pub spent_microunits: Option<u64>,
    pub currency: Option<wire::Text>,
    /// Every fact has a provider-reported or token-rate cost.
    pub complete: bool,
    pub reason: Option<String>,
}

pub const MAX_COST_COMPONENTS: usize = 8;

/// Summarize canonical facts with the billing basis of the source that owns
/// each one. A reported zero stays a `ProviderReported` component; missing
/// evidence is an `Unknown` component; flat subscription and local-model usage
/// is labelled `NotApplicable`, never charged at token rates.
pub fn summarize(
    facts: &[(Option<EstimateBasis>, &crate::reconcile::Fact)],
    book: &RateCardBook,
) -> Result<Summary> {
    use std::collections::BTreeMap;
    let key = |v: &wire::ArtifactRef| serde_json::to_string(v).map_err(|_| Error::Invalid);
    let mut reported: BTreeMap<String, (u64, Vec<wire::ArtifactRef>)> = BTreeMap::new();
    let mut estimated: BTreeMap<(String, String), (u64, Cost)> = BTreeMap::new();
    let mut unknown: Vec<UnknownReason> = Vec::new();
    let note = |unknown: &mut Vec<UnknownReason>, reason: UnknownReason| {
        if !unknown.contains(&reason) {
            unknown.push(reason);
        }
    };
    let mut reasons = std::collections::BTreeSet::new();
    let add = |total: &mut u64, amount: u64| -> Result<()> {
        *total = total.checked_add(amount).ok_or(Error::Overflow)?;
        Ok(())
    };
    for (basis, fact) in facts {
        fact.validate()?;
        match basis {
            Some(EstimateBasis::SubscriptionAllocation) => {
                note(&mut unknown, UnknownReason::NotApplicable);
                reasons.insert("flat subscription usage is labelled with the subscription basis, not costed at token rates");
                continue;
            }
            Some(EstimateBasis::LocalModelAllocation) => {
                note(&mut unknown, UnknownReason::NotApplicable);
                reasons.insert("local model usage is labelled with the local allocation basis, not costed at token rates");
                continue;
            }
            _ => {}
        }
        if let Cost::ProviderReported {
            amount_microunits,
            currency,
            receipt,
        } = &fact.usage.cost
        {
            let entry = reported.entry(currency.as_str().to_owned()).or_default();
            add(&mut entry.0, *amount_microunits)?;
            entry.1.push(receipt.clone());
            continue;
        }
        let card = match (basis, &fact.pricing_reference) {
            (Some(EstimateBasis::TokenRates), Some(pinned)) => book.effective(pinned),
            _ => None,
        };
        let Some(card) = card else {
            note(&mut unknown, UnknownReason::NotReported);
            reasons.insert("no provider-reported cost or pinned rate card");
            continue;
        };
        match card.price(fact)?.cost {
            cost @ Cost::NonInvoiceEstimate { .. } => {
                let Cost::NonInvoiceEstimate {
                    amount_microunits,
                    currency,
                    ..
                } = &cost
                else {
                    unreachable!()
                };
                let entry = estimated
                    .entry((key(&card.reference)?, currency.as_str().to_owned()))
                    .or_insert((0, cost.clone()));
                add(&mut entry.0, *amount_microunits)?;
            }
            _ => {
                note(&mut unknown, UnknownReason::Incomplete);
                reasons.insert("token counters or accounting evidence are incomplete");
            }
        }
    }
    let mut costs = Vec::new();
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    for (currency, (amount, receipts)) in reported {
        let receipt = if receipts.len() == 1 {
            receipts[0].clone()
        } else {
            wire::ArtifactRef {
                id: wire::Text::new("aq.accounting.reported-receipts")?,
                version: wire::Text::new("1")?,
                digest: crate::hash(&receipts)?,
            }
        };
        add(totals.entry(currency.clone()).or_default(), amount)?;
        costs.push(Cost::ProviderReported {
            amount_microunits: amount,
            currency: wire::Text::new(&currency)?,
            receipt,
        });
    }
    for ((_, currency), (amount, mut cost)) in estimated {
        add(totals.entry(currency).or_default(), amount)?;
        if let Cost::NonInvoiceEstimate {
            amount_microunits, ..
        } = &mut cost
        {
            *amount_microunits = amount;
        }
        costs.push(cost);
    }
    for reason in [
        UnknownReason::NotReported,
        UnknownReason::Unsupported,
        UnknownReason::SourceGap,
        UnknownReason::NotApplicable,
        UnknownReason::Incomplete,
    ] {
        if unknown.contains(&reason) {
            costs.push(Cost::Unknown { reason });
        }
    }
    let complete = !facts.is_empty() && unknown.is_empty() && totals.len() <= 1;
    if costs.len() > MAX_COST_COMPONENTS {
        costs = vec![Cost::Unknown {
            reason: UnknownReason::Incomplete,
        }];
        reasons.insert("more cost components than the usage view can carry");
    }
    let (spent_microunits, currency) = match totals.len() {
        0 => (Some(0), None),
        1 => {
            let (c, v) = totals.into_iter().next().expect("one total");
            (Some(v), Some(wire::Text::new(&c)?))
        }
        _ => {
            reasons.insert("costs are reported in more than one currency");
            (None, None)
        }
    };
    let reason = (!reasons.is_empty()).then(|| reasons.into_iter().collect::<Vec<_>>().join("; "));
    Ok(Summary {
        costs,
        spent_microunits,
        currency,
        complete: complete && reason.is_none(),
        reason,
    })
}
