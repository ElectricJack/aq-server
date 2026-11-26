//! One explicit human decision over exact code AND a declared effect scope.
//! Serialized subjects are data. Only the protected owner can produce the
//! verified decision product consumed by settlement.
use super::*;
use crate::profiled_execution::settlement::effects::{self, AqEffectsSnapshot, LedgerDisposition};
use serde::Deserialize;
use subject_owner::{Profile, Subject};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CombinedProfile {
    #[serde(rename = "source_and_effects_review_v1")]
    V1,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ExternalScope {
    #[serde(rename = "trusted_node_source_and_aq_reads_only_v1")]
    TrustedNodeSourceAndAqReadsOnly,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExternalDisposition {
    Reconciled,
    Unresolved,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExternalEffect {
    pub resource: String,
    pub disposition: ExternalDisposition,
    pub reason: String,
    pub evidence: Vec<ArtifactRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExternalDeclaration {
    pub scope: ExternalScope,
    pub worktree_attempt_id: Uuid,
    pub known_effects: Vec<ExternalEffect>,
    pub reason: String,
    pub residual_uncertainty: String,
}
fn text(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max {
        return Err(invalid());
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
impl ExternalDeclaration {
    pub fn validate(&self, attempt: Uuid) -> Result<()> {
        if self.worktree_attempt_id != attempt || attempt.is_nil() {
            return Err(invalid());
        }
        if self.known_effects.len() > 16 {
            return Err(invalid());
        }
        text(&self.reason, 2048)?;
        text(&self.residual_uncertainty, 2048)?;
        let mut resources = std::collections::BTreeSet::new();
        for e in &self.known_effects {
            e.validate(&mut resources)?;
        }
        if encode(self)?.len() > 8192 {
            return Err(invalid());
        }
        Ok(())
    }
}
impl ExternalEffect {
    fn validate<'a>(
        &'a self,
        resources: &mut std::collections::BTreeSet<&'a String>,
    ) -> Result<()> {
        text(&self.resource, 512)?;
        text(&self.reason, 2048)?;
        if !resources.insert(&self.resource) || self.evidence.len() > 8 {
            return Err(invalid());
        }
        for r in &self.evidence {
            r.id.validate().map_err(|_| invalid())?;
            r.version.validate().map_err(|_| invalid())?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AqLedgerBasis {
    pub profile: ArtifactRef,
    pub digest: Digest,
    pub operation_count: u32,
    pub inspected_bytes: u64,
    pub messaging_disabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CombinedSubject {
    pub schema_version: u32,
    pub profile: CombinedProfile,
    pub code: ReviewSubject,
    pub aq_ledger: AqLedgerBasis,
    pub external: ExternalDeclaration,
}
impl CombinedSubject {
    pub fn validate(&self) -> Result<()> {
        self.code.validate().map_err(|_| invalid())?;
        self.external.validate(self.code.binding.attempt_id)?;
        if self.schema_version != 1 || !self.aq_ledger.messaging_disabled {
            return Err(invalid());
        }
        if self.aq_ledger.operation_count > 4096 || self.aq_ledger.inspected_bytes > 33554432 {
            return Err(invalid());
        }
        if self.aq_ledger.profile.digest != self.code.binding.graph_access_profile_digest {
            return Err(invalid());
        }
        encode(self)?;
        Ok(())
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        self.validate()?;
        reference(
            format!(
                "aq.source-effects-review-subject/{}",
                self.code.result.report_id
            ),
            self,
        )
    }
    pub(super) fn matches_ledger(
        &self,
        result: &crate::profiled_execution::results::RecordedResult,
        ledger: &AqEffectsSnapshot,
    ) -> bool {
        ledger.disposition() == LedgerDisposition::Settled
            && self.aq_ledger.profile == result.spec.graph_access_profile
            && self.aq_ledger.digest == *ledger.digest()
            && self.aq_ledger.operation_count as usize == ledger.operation_count()
            && self.aq_ledger.inspected_bytes == ledger.inspected_bytes()
    }
}
/// Prepared actual owner reads, never deserializable or a finality receipt.
pub struct PreparedCombined {
    code: PreparedReview,
    subject: CombinedSubject,
    principal: Option<crate::profiled_execution::settlement::actor::ActorPin>,
}
impl PreparedCombined {
    pub fn subject(&self) -> &CombinedSubject {
        &self.subject
    }
}
/// Produced only by protected exact decision reads and current eligible Owner.
pub struct VerifiedCombinedDecision {
    subject: CombinedSubject,
    receipt: DecisionReceipt,
}
impl VerifiedCombinedDecision {
    pub(crate) fn subject(&self) -> &CombinedSubject {
        &self.subject
    }
    pub(crate) fn receipt(&self) -> &DecisionReceipt {
        &self.receipt
    }
}
impl Service {
    async fn observe_principal(
        &self,
        actor: &crate::store::User,
    ) -> Result<crate::profiled_execution::settlement::actor::ActorPin> {
        crate::profiled_execution::settlement::actor::ActorPin::observe(&self.state, actor).await
    }

    async fn prepare_combined(
        &self,
        report: Uuid,
        source: Uuid,
        external: ExternalDeclaration,
    ) -> Result<PreparedCombined> {
        let code = self.prepare(report, source).await?;
        external.validate(code.subject.binding.attempt_id)?;
        let mut tx = self.tx().await?;
        let result =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report).await?;
        if !result.is_current_report_head {
            return Err(conflict());
        }
        let ledger = effects::inspect_aq_effects_locked(&self.state, &mut tx, &result).await?;
        if ledger.disposition() != LedgerDisposition::Settled {
            return Err(AppError::UpdatesUnavailable(
                "AQ effects require reconciliation before combined review".into(),
            ));
        }
        let subject = CombinedSubject {
            schema_version: 1,
            profile: CombinedProfile::V1,
            code: code.subject.clone(),
            aq_ledger: AqLedgerBasis {
                profile: result.spec.graph_access_profile,
                digest: ledger.digest().clone(),
                operation_count: ledger.operation_count() as u32,
                inspected_bytes: ledger.inspected_bytes(),
                messaging_disabled: true,
            },
            external,
        };
        subject.validate()?;
        tx.commit().await.map_err(db)?;
        Ok(PreparedCombined {
            code,
            subject,
            principal: None,
        })
    }
    pub async fn admit_combined(
        &self,
        actor: Uuid,
        prepared: &PreparedCombined,
        request: &DecisionRequest,
    ) -> Result<()> {
        if prepared.principal.as_ref().is_none_or(|p| p.id() != actor) {
            return Err(forbidden());
        }
        let mut tx = self.tx().await?;
        let mut proposal = prepared.code.proposal(request);
        proposal.combined = Some(&prepared.subject);
        self.admit_subject_in_tx(
            &mut tx,
            ReviewActor::new(actor, prepared.principal.as_ref()),
            proposal,
        )
        .await?;
        tx.commit().await.map_err(db)
    }
    pub async fn prepare_combined_as(
        &self,
        actor: &crate::store::User,
        report: Uuid,
        source: Uuid,
        external: ExternalDeclaration,
    ) -> Result<PreparedCombined> {
        let principal = self.observe_principal(actor).await?;
        let mut prepared = self.prepare_combined(report, source, external).await?;
        // Preparation itself exposes private subject data, even without later
        // admission. Fence the original principal AFTER Git I/O, preserving the
        // common B1 -> user lock order used by every actual review operation.
        let mut tx = self.tx().await?;
        let current =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report).await?;
        principal.fence(&mut tx).await?;
        if !current.is_current_report_head
            || current.receipt_artifact != prepared.code.subject.result.record_receipt
        {
            return Err(conflict());
        }
        tx.commit().await.map_err(db)?;
        prepared.principal = Some(principal);
        Ok(prepared)
    }
    pub async fn resolve_combined_as(
        &self,
        actor: &crate::store::User,
        answer: &ResolveRequest,
        expected_subject: &ArtifactRef,
    ) -> Result<DecisionReceipt> {
        let principal = self.observe_principal(actor).await?;
        self.resolve_subject(
            ReviewActor::new(actor.id, Some(&principal)),
            answer,
            ReviewScope::new(Profile::SourceAndEffects, Some(expected_subject)),
        )
        .await
    }
    pub async fn inspect_combined_as(
        &self,
        actor: &crate::store::User,
        decision: Uuid,
    ) -> Result<serde_json::Value> {
        let principal = self.observe_principal(actor).await?;
        self.inspect_subject(
            ReviewActor::new(actor.id, Some(&principal)),
            decision,
            Profile::SourceAndEffects,
        )
        .await
    }
    pub(crate) async fn combined_completion_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        decision: Uuid,
    ) -> Result<VerifiedCombinedDecision> {
        // Immutable identity peek, then actual common authority BEFORE any
        // decision/outbox lock, including callers that did not already load B1.
        let identity=sqlx::query("SELECT s.report_id,s.subject_profile FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1").bind(decision).fetch_one(&mut **tx).await.map_err(db)?;
        crate::profiled_execution::results::load_locked(&self.state, tx, identity.get("report_id"))
            .await?;
        if identity.get::<String, _>("subject_profile") != "source_and_effects_review_v1" {
            return Err(conflict());
        }
        // A valid pending combined question is a normal unavailable requirement,
        // not corrupted finality. Keep the old code-only callback reader intact.
        let progress=sqlx::query("SELECT decision_revision,state FROM human_decision_requests WHERE decision_id=$1 FOR SHARE").bind(decision).fetch_one(&mut **tx).await.map_err(db)?;
        if records::pending(&progress) {
            return Err(AppError::UpdatesUnavailable(
                "combined review has no recorded answer".into(),
            ));
        }
        let (subject, _event, receipt) = self.completion_subject_in_tx(tx, decision).await?;
        let Subject::SourceAndEffects(subject) = subject else {
            return Err(conflict());
        };
        subject.validate()?;
        let DecisionFinality::HumanAnswer {
            owner_user_id,
            choice: Choice::Approve,
            ..
        } = receipt.finality
        else {
            return Err(conflict());
        };
        if !subject
            .external
            .known_effects
            .iter()
            .all(|e| e.disposition == ExternalDisposition::Reconciled)
        {
            return Err(conflict());
        }
        let actor = crate::profiled_execution::settlement::actor::ActorPin::current_locked(
            &self.state,
            tx,
            owner_user_id,
        )
        .await?;
        let row=sqlx::query("SELECT combined_actor_digest,combined_actor_revision FROM human_decision_finality WHERE decision_id=$1 FOR SHARE").bind(decision).fetch_one(&mut **tx).await.map_err(db)?;
        records::validate_actor_pin(&row, &actor)?;
        Ok(VerifiedCombinedDecision {
            subject: *subject,
            receipt,
        })
    }
}
