use crate::{Error, Layout, Result, subtract, wire};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wire::{CategorySemantics, CounterBasis, ObservationKind, Validate};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserAuthority {
    pub parser: wire::ArtifactRef,
    pub category_claim: wire::ArtifactRef,
    pub priority: u8,
    /// Owner-verified selected-version qualification, never a worker capability claim.
    pub complete_accounting_evidence: Option<wire::ArtifactRef>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub reference: wire::ArtifactRef,
    /// Stable owner-established account identity, independent of key rotation.
    pub account_scope: wire::Digest,
    /// Stable native-call identity namespace, independent of parser versions.
    pub call_namespace: wire::Digest,
    pub provider: wire::Text,
    pub layout: Layout,
    pub parsers: Vec<ParserAuthority>,
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        wire::artifact(&self.reference)?;
        self.provider.validate()?;
        self.layout.validate()?;
        if self.parsers.is_empty() || self.parsers.len() > 16 {
            return Err(Error::Invalid);
        }
        for (i, p) in self.parsers.iter().enumerate() {
            wire::artifact(&p.parser)?;
            wire::artifact(&p.category_claim)?;
            if let Some(evidence) = &p.complete_accounting_evidence {
                wire::artifact(evidence)?;
            }
            if p.priority == 0 || self.parsers[..i].iter().any(|old| old.parser == p.parser) {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub counter_key: wire::Digest,
    /// Actual owner-verified fresh-zero or retained-history baseline evidence.
    pub reference: wire::ArtifactRef,
    pub counters: wire::Counters,
}
/// Inputs are looked up by the central owner, not deserialized from an agent's
/// request. `lineage` and account/namespace policy must match actual admission.
pub struct Context<'a> {
    pub lineage: Uuid,
    pub policy: &'a Policy,
    pub current: Option<&'a Fact>,
    pub correction_target: Option<&'a Fact>,
    pub baseline: Option<&'a Baseline>,
    pub pricing_reference: Option<&'a wire::ArtifactRef>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub key: wire::Digest,
    pub lineage: Uuid,
    pub event: wire::EventRef,
    pub source: wire::Digest,
    pub priority: u8,
    pub complete_accounting_evidence: Option<wire::ArtifactRef>,
    pub policy: wire::ArtifactRef,
    pub layout: Layout,
    pub pricing_reference: Option<wire::ArtifactRef>,
    pub usage: wire::Usage,
    pub baseline: Option<Baseline>,
    pub contribution: wire::Counters,
    pub semantic_digest: wire::Digest,
    pub fact_digest: wire::Digest,
}
impl Fact {
    fn digest(&self) -> Result<wire::Digest> {
        crate::hash(&(
            &self.key,
            self.lineage,
            &self.event,
            &self.source,
            self.priority,
            &self.complete_accounting_evidence,
            &self.policy,
            &self.layout,
            &self.pricing_reference,
            &self.usage,
            &self.baseline,
            &self.contribution,
            &self.semantic_digest,
        ))
    }
    fn has_digest_mismatch(&self) -> Result<bool> {
        Ok(self.semantic_digest
            != semantic(
                &self.usage,
                &self.layout,
                &self.complete_accounting_evidence,
            )?
            || self.fact_digest != self.digest()?)
    }
    pub(crate) fn source_incomplete(&self) -> bool {
        self.complete_accounting_evidence.is_none()
            || !matches!(
                self.usage.completeness,
                wire::Completeness::ReportedComplete
            )
    }
    pub fn validate(&self) -> Result<()> {
        self.event.validate()?;
        self.usage.validate()?;
        wire::artifact(&self.policy)?;
        self.layout.check_counters(&self.usage.counters)?;
        self.layout.check_counters(&self.contribution)?;
        if self.lineage.is_nil() || self.priority == 0 {
            return Err(Error::Invalid);
        }
        if self.has_digest_mismatch()? {
            return Err(Error::Invalid);
        }
        self.pricing_reference
            .iter()
            .chain(&self.complete_accounting_evidence)
            .try_for_each(wire::artifact)?;
        self.validate_contribution()
    }
    fn validate_contribution(&self) -> Result<()> {
        match (&self.usage.basis, &self.baseline) {
            (CounterBasis::PerCall, None) => {
                if self.contribution != self.usage.counters {
                    return Err(Error::Invalid);
                }
            }
            (CounterBasis::Cumulative { .. }, Some(baseline)) => {
                wire::artifact(&baseline.reference)?;
                if baseline.counter_key != self.key {
                    return Err(Error::Invalid);
                }
                if self.contribution != subtract(&self.usage.counters, &baseline.counters)? {
                    return Err(Error::Invalid);
                }
            }
            _ => return Err(Error::Invalid),
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoChargeReason {
    Identical,
    LowerPrecedence,
    ImportedHistory,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    MissingBaseline,
    CounterReset,
    AttributionChanged,
    ConflictingCall,
    UnknownCorrection,
    StaleCorrection,
    ForeignLineageCorrection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Decision {
    Append {
        fact: Box<Fact>,
        supersedes: Option<wire::EventRef>,
        previous_contribution: Option<wire::Counters>,
    },
    NoCharge {
        reason: NoChargeReason,
    },
    /// Accept/audit the observation with an incomplete accounting condition;
    /// do not claim these unrepresented tokens are zero or consume a full-policy
    /// accounting capability. The adapter must persist that gap atomically.
    Unresolved {
        reason: UnresolvedReason,
    },
}
fn semantic(
    usage: &wire::Usage,
    layout: &Layout,
    qualification: &Option<wire::ArtifactRef>,
) -> Result<wire::Digest> {
    // Cost is a separately verified owner claim. An untrusted reported invoice
    // cannot change accounting by varying the transcript's cost field.
    crate::hash(&(
        &usage.call.provider,
        &usage.call.model,
        &usage.basis,
        &usage.counters,
        layout,
        &usage.completeness,
        qualification,
    ))
}
pub fn key(record: &wire::ObservationRecord, policy: &Policy) -> Result<wire::Digest> {
    record.validate()?;
    policy.validate()?;
    let ObservationKind::Usage { usage } = &record.body.kind else {
        return Err(Error::Invalid);
    };
    Ok(match &usage.basis {
        CounterBasis::PerCall => crate::hash(&(
            "aq.call/1",
            &policy.account_scope,
            &policy.call_namespace,
            &usage.call.native_call_id,
        ))?,
        CounterBasis::Cumulative {
            counter_id,
            counter_generation,
        } => crate::hash(&(
            "aq.cumulative/1",
            &policy.account_scope,
            &policy.call_namespace,
            record.body.provenance.source_id,
            record.body.provenance.source_generation,
            counter_id,
            counter_generation,
        ))?,
    })
}
pub fn decide(record: &wire::ObservationRecord, context: Context<'_>) -> Result<Decision> {
    record.validate()?;
    context.policy.validate()?;
    if context.lineage.is_nil() || record.body.binding.lineage_id != context.lineage {
        return Err(Error::Invalid);
    }
    let ObservationKind::Usage { usage } = &record.body.kind else {
        return Err(Error::Invalid);
    };
    let parser = context.policy.verify_usage(record, usage)?;
    let key = key(record, context.policy)?;
    let source = crate::hash(&(
        record.body.provenance.source_id,
        record.body.provenance.source_generation,
        &record.body.provenance.native_session_key,
    ))?;
    let semantic_digest = semantic(
        usage,
        &context.policy.layout,
        &parser.complete_accounting_evidence,
    )?;
    let incoming = Incoming {
        usage,
        key,
        parser,
        source,
        semantic_digest,
    };
    context.validate_heads(&incoming.key)?;
    if let Some(reason) = incoming.correction_refusal(&context) {
        return Ok(Decision::Unresolved { reason });
    }
    if let Some(decision) = context
        .current
        .and_then(|current| incoming.compare(current, &context))
    {
        return Ok(decision);
    }
    incoming.append(record, context)
}

impl Context<'_> {
    fn validate_heads(&self, key: &wire::Digest) -> Result<()> {
        for head in self.current.into_iter().chain(self.correction_target) {
            head.validate()?;
            if &head.key != key {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
    fn baseline_for(&self, usage: &wire::Usage) -> Option<Baseline> {
        if matches!(usage.basis, CounterBasis::PerCall) {
            return None;
        }
        match self.current {
            Some(current) => current.baseline.clone(),
            None => self.baseline.cloned(),
        }
    }
}
impl Policy {
    fn verify_usage(
        &self,
        record: &wire::ObservationRecord,
        usage: &wire::Usage,
    ) -> Result<&ParserAuthority> {
        if usage.call.provider != self.provider {
            return Err(Error::Unsupported);
        }
        let category_policy = match &usage.category_semantics {
            CategorySemantics::DeclaredDisjoint { policy }
            | CategorySemantics::MayOverlap { policy } => policy,
        };
        let parser = self
            .parsers
            .iter()
            .find(|p| p.parser == record.body.provenance.parser)
            .ok_or(Error::Unsupported)?;
        if category_policy != &parser.category_claim {
            return Err(Error::Unsupported);
        }
        if matches!(
            usage.category_semantics,
            CategorySemantics::DeclaredDisjoint { .. }
        ) && self.layout.has_overlap()
        {
            return Err(Error::Conflict);
        }
        self.layout.check_counters(&usage.counters)?;
        Ok(parser)
    }
}
impl Layout {
    fn has_overlap(&self) -> bool {
        self.input_includes_cache_read
            || self.input_includes_cache_write
            || self.output_includes_reasoning
    }
}
struct Incoming<'a> {
    key: wire::Digest,
    parser: &'a ParserAuthority,
    usage: &'a wire::Usage,
    source: wire::Digest,
    semantic_digest: wire::Digest,
}
impl Incoming<'_> {
    fn append(self, record: &wire::ObservationRecord, context: Context<'_>) -> Result<Decision> {
        let usage = self.usage;
        let unresolved = |reason| Ok(Decision::Unresolved { reason });
        let baseline = context.baseline_for(usage);
        let contribution = match &usage.basis {
            CounterBasis::PerCall => usage.counters.clone(),
            CounterBasis::Cumulative { .. } => {
                let Some(baseline) = &baseline else {
                    return unresolved(UnresolvedReason::MissingBaseline);
                };
                wire::artifact(&baseline.reference)?;
                if baseline.counter_key != self.key {
                    return Err(Error::Invalid);
                }
                match subtract(&usage.counters, &baseline.counters) {
                    Ok(c) => c,
                    Err(Error::Conflict) => return unresolved(UnresolvedReason::CounterReset),
                    Err(e) => return Err(e),
                }
            }
        };
        context.policy.layout.check_counters(&contribution)?;
        let pricing_reference = match context.current {
            Some(current) => current.pricing_reference.clone(),
            None => context.pricing_reference.cloned(),
        };
        let mut fact = Fact {
            key: self.key,
            lineage: context.lineage,
            event: wire::EventRef {
                event_id: record.event_id,
                digest: record.body_digest.clone(),
            },
            source: self.source,
            priority: self.parser.priority,
            complete_accounting_evidence: self.parser.complete_accounting_evidence.clone(),
            policy: context.policy.reference.clone(),
            layout: context.policy.layout.clone(),
            pricing_reference,
            usage: usage.clone(),
            baseline,
            contribution,
            semantic_digest: self.semantic_digest,
            fact_digest: wire::Digest::of(b"unsealed"),
        };
        fact.fact_digest = fact.digest()?;
        fact.validate()?;
        Ok(Decision::Append {
            fact: Box::new(fact),
            supersedes: context.current.map(|v| v.event.clone()),
            previous_contribution: context.current.map(|v| v.contribution.clone()),
        })
    }

    fn correction_refusal(&self, context: &Context<'_>) -> Option<UnresolvedReason> {
        let old = self.usage.correction_of.as_ref()?;
        let Some(target) = context
            .correction_target
            .filter(|target| &target.event == old)
        else {
            return Some(UnresolvedReason::UnknownCorrection);
        };
        if target.lineage != context.lineage {
            return Some(UnresolvedReason::ForeignLineageCorrection);
        }
        match context.current {
            None => Some(UnresolvedReason::UnknownCorrection),
            Some(current) if current.event != target.event && current.source == self.source => {
                Some(UnresolvedReason::StaleCorrection)
            }
            _ => None,
        }
    }
    fn compare(&self, current: &Fact, context: &Context<'_>) -> Option<Decision> {
        let unresolved = |reason| Some(Decision::Unresolved { reason });
        let no_charge = |reason| Some(Decision::NoCharge { reason });
        let identical = current.semantic_digest == self.semantic_digest;
        if current.lineage != context.lineage {
            return if identical {
                no_charge(NoChargeReason::ImportedHistory)
            } else {
                unresolved(UnresolvedReason::ForeignLineageCorrection)
            };
        }
        if current.layout != context.policy.layout {
            return unresolved(UnresolvedReason::AttributionChanged);
        }
        if self.parser.priority < current.priority {
            return no_charge(NoChargeReason::LowerPrecedence);
        }
        if identical && self.parser.priority == current.priority {
            return no_charge(NoChargeReason::Identical);
        }
        self.usage_conflict(current, identical)
            .map(|reason| Decision::Unresolved { reason })
    }
    fn usage_conflict(&self, current: &Fact, identical: bool) -> Option<UnresolvedReason> {
        let usage = self.usage;
        if matches!(usage.basis, CounterBasis::Cumulative { .. }) {
            return self.cumulative_conflict(current);
        }
        let same_priority = self.parser.priority == current.priority;
        (same_priority && usage.correction_of.is_none() && !identical)
            .then_some(UnresolvedReason::ConflictingCall)
    }
    fn cumulative_conflict(&self, current: &Fact) -> Option<UnresolvedReason> {
        let usage = self.usage;
        if current.source != self.source || current.usage.call.model != usage.call.model {
            return Some(UnresolvedReason::AttributionChanged);
        }
        let reset = usage.correction_of.is_none()
            && subtract(&usage.counters, &current.usage.counters).is_err();
        reset.then_some(UnresolvedReason::CounterReset)
    }
}
