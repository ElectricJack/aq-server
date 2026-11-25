use super::*;
use std::collections::{BTreeMap, BTreeSet};
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
    let mut components = Components::default();
    for (basis, fact) in facts {
        fact.validate()?;
        components.include(basis.as_ref(), fact, book)?;
    }
    components.summary(!facts.is_empty())
}

type Reported = BTreeMap<String, (u64, Vec<wire::ArtifactRef>)>;
type Estimated = BTreeMap<(String, String), (u64, Cost)>;
#[derive(Default)]
struct Components {
    reported: Reported,
    estimated: Estimated,
    unknown: Vec<UnknownReason>,
    reasons: BTreeSet<&'static str>,
}

fn add(total: &mut u64, amount: u64) -> Result<()> {
    *total = total.checked_add(amount).ok_or(Error::Overflow)?;
    Ok(())
}
impl Components {
    fn note(&mut self, reason: UnknownReason) {
        if !self.unknown.contains(&reason) {
            self.unknown.push(reason);
        }
    }
    fn include(
        &mut self,
        basis: Option<&EstimateBasis>,
        fact: &crate::reconcile::Fact,
        book: &RateCardBook,
    ) -> Result<()> {
        match basis {
            Some(EstimateBasis::SubscriptionAllocation) => {
                self.note(UnknownReason::NotApplicable);
                self.reasons.insert("flat subscription usage is labelled with the subscription basis, not costed at token rates");
                return Ok(());
            }
            Some(EstimateBasis::LocalModelAllocation) => {
                self.note(UnknownReason::NotApplicable);
                self.reasons.insert("local model usage is labelled with the local allocation basis, not costed at token rates");
                return Ok(());
            }
            _ => {}
        }
        if let Cost::ProviderReported {
            amount_microunits,
            currency,
            receipt,
        } = &fact.usage.cost
        {
            let entry = self
                .reported
                .entry(currency.as_str().to_owned())
                .or_default();
            add(&mut entry.0, *amount_microunits)?;
            entry.1.push(receipt.clone());
            return Ok(());
        }
        let card = match (basis, &fact.pricing_reference) {
            (Some(EstimateBasis::TokenRates), Some(pinned)) => book.effective(pinned),
            _ => None,
        };
        let Some(card) = card else {
            self.note(UnknownReason::NotReported);
            self.reasons
                .insert("no provider-reported cost or pinned rate card");
            return Ok(());
        };
        let cost = card.price(fact)?.cost;
        if let Cost::NonInvoiceEstimate {
            amount_microunits,
            currency,
            ..
        } = &cost
        {
            let key = serde_json::to_string(&card.reference).map_err(|_| Error::Invalid)?;
            let entry = self
                .estimated
                .entry((key, currency.as_str().to_owned()))
                .or_insert((0, cost.clone()));
            add(&mut entry.0, *amount_microunits)?;
        } else {
            self.note(UnknownReason::Incomplete);
            self.reasons
                .insert("token counters or accounting evidence are incomplete");
        }
        Ok(())
    }
    fn summary(self, has_facts: bool) -> Result<Summary> {
        let Self {
            reported,
            estimated,
            unknown,
            mut reasons,
        } = self;
        let mut costs = Vec::new();
        let mut totals: BTreeMap<String, u64> = BTreeMap::new();
        reported_components(reported, &mut costs, &mut totals)?;
        estimated_components(estimated, &mut costs, &mut totals)?;
        unknown_components(&unknown, &mut costs);
        let complete = has_facts && unknown.is_empty() && totals.len() <= 1;
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
        let reason =
            (!reasons.is_empty()).then(|| reasons.into_iter().collect::<Vec<_>>().join("; "));
        Ok(Summary {
            costs,
            spent_microunits,
            currency,
            complete: complete && reason.is_none(),
            reason,
        })
    }
}

fn reported_components(
    reported: Reported,
    costs: &mut Vec<Cost>,
    totals: &mut BTreeMap<String, u64>,
) -> Result<()> {
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
    Ok(())
}
fn estimated_components(
    estimated: Estimated,
    costs: &mut Vec<Cost>,
    totals: &mut BTreeMap<String, u64>,
) -> Result<()> {
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
    Ok(())
}
fn unknown_components(unknown: &[UnknownReason], costs: &mut Vec<Cost>) {
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
}
