//! Finite first-pilot planning. Plans are data; no eligibility record, release,
//! Graph mutation, or execution authority can be constructed by this module.
use super::effects::{AqEffectsSnapshot, LedgerDisposition};
use crate::{
    error::AppError, profiled_execution::results::RecordedResult,
    source_retention::RecordedSourceConfirmation,
};
use aq_admission::{
    ArtifactRef, Digest,
    outcome::{OutcomeKind, ReviewRequirement},
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Eligible,
    Rejected,
    Stale,
    NeedsReconciliation,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    UnsupportedPolicy,
    UnsupportedOutcome,
    RequiredChecksUnsupported,
    SupersededReport,
    SourceMismatch,
    ReviewUnavailable,
    ReviewRejected,
    ReviewMismatch,
    AqEffectsUnavailable,
    AqEffectsUnsettled,
    DeclaredEffectsPending,
    ExternalDispositionUnavailable,
    ExternalDispositionMismatch,
    ApplicabilityUnavailable,
    Cancelled,
    ChangedInputs,
}
#[derive(Debug, PartialEq, Eq)]
pub struct EligibilityPlan {
    pub report_id: Uuid,
    pub disposition: Disposition,
    pub reason: Option<Reason>,
}

/// Finite body from an actually loaded, digest-checked definition. A project may
/// select another policy; that policy remains unsupported by this first pilot.
pub struct PilotPolicy {
    artifact: ArtifactRef,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyBody {
    schema_version: u32,
    kind: PolicyKind,
}
#[derive(Deserialize)]
enum PolicyKind {
    #[serde(rename = "source_change_reviewed_human_owner_no_checks_v1")]
    HumanOwnerNoChecks,
}
impl PilotPolicy {
    pub fn from_definition(
        definition: &crate::profiled_execution::definitions::Definition,
    ) -> Result<Self, AppError> {
        if definition.body().len() > 4096 {
            return Err(AppError::BadRequest("pilot validation policy bound".into()));
        }
        let body: PolicyBody = serde_json::from_str(definition.body())
            .map_err(|_| AppError::BadRequest("unsupported pilot validation policy".into()))?;
        if body.schema_version != 1 {
            return Err(AppError::BadRequest(
                "unsupported pilot validation policy".into(),
            ));
        }
        let PolicyKind::HumanOwnerNoChecks = body.kind;
        Ok(Self {
            artifact: definition.artifact().clone(),
        })
    }
}

/// Produced only by the existing protected decision owner's verified read.
/// It remains code review, never evidence that external effects do not exist.
pub struct VerifiedReviewEvidence {
    subject: aq_review_protocol::ReviewSubject,
    receipt: ArtifactRef,
    // Only a future actual combined-subject owner can populate this. Existing
    // code_review_v1 reads ALWAYS leave it absent.
    combined_subject: Option<ArtifactRef>,
    approved: bool,
}
impl VerifiedReviewEvidence {
    pub(super) fn from_combined(
        fact: &crate::reviews::combined::VerifiedCombinedDecision,
    ) -> Result<Self, AppError> {
        let subject = fact.subject();
        Ok(Self {
            subject: subject.code.clone(),
            receipt: fact.receipt().artifact().map_err(|_| AppError::Internal)?,
            combined_subject: Some(subject.artifact()?),
            approved: true,
        })
    }
}

/// The protected combined-decision owner supplies this product. A code-only Approve,
/// an empty worker pending list, or shell success cannot construct this evidence.
pub struct VerifiedEffectDisposition {
    report: ArtifactRef,
    stopped: ArtifactRef,
    source: Digest,
    ledger: Digest,
    policy: ArtifactRef,
    receipt: ArtifactRef,
    combined_subject: Option<ArtifactRef>,
}
impl VerifiedEffectDisposition {
    pub(super) fn from_combined(
        fact: &crate::reviews::combined::VerifiedCombinedDecision,
        source: &RecordedSourceConfirmation,
    ) -> Result<Self, AppError> {
        let s = fact.subject();
        Ok(Self {
            report: s.code.result.record_receipt.clone(),
            stopped: s.code.result.stopped_receipt.clone(),
            source: source_digest(source)?,
            ledger: s.aq_ledger.digest.clone(),
            policy: s.code.validation_policy.clone(),
            receipt: fact.receipt().artifact().map_err(|_| AppError::Internal)?,
            combined_subject: Some(s.artifact()?),
        })
    }
}
/// The actual current Graph/project/input owner supplies this product.
/// A caller bool or historical DTO is not an
/// applicability observation. The future owner must serialize cancellation.
pub struct CurrentApplicability {
    report: ArtifactRef,
    source: Digest,
    cancelled: bool,
    changed: bool,
}
impl CurrentApplicability {
    pub(super) fn from_owner(fact: &super::applicability::VerifiedApplicability) -> Self {
        Self {
            report: fact.report().clone(),
            source: fact.source().clone(),
            cancelled: fact.cancelled(),
            changed: false,
        }
    }
}

/// Missing owners are represented by None, never success-shaped placeholders.
/// Fields reference opaque owner products and confer no execution authority.
pub struct Evidence<'a> {
    pub policy: Option<&'a PilotPolicy>,
    pub review: Option<&'a VerifiedReviewEvidence>,
    pub aq_effects: Option<&'a AqEffectsSnapshot>,
    pub external_effects: Option<&'a VerifiedEffectDisposition>,
    pub applicability: Option<&'a CurrentApplicability>,
}
impl Evidence<'_> {
    pub fn unavailable() -> Self {
        Self {
            policy: None,
            review: None,
            aq_effects: None,
            external_effects: None,
            applicability: None,
        }
    }
}
fn source_digest(source: &RecordedSourceConfirmation) -> Result<Digest, AppError> {
    let value = serde_json::json!({"manifest":source.manifest,"receipt":source.receipt,"acknowledgement":source.acknowledgement});
    let bytes = aq_agent_api::canonical(&value).map_err(|_| AppError::Internal)?;
    Ok(Digest::of(&bytes))
}
fn source_matches(result: &RecordedResult, source: &RecordedSourceConfirmation) -> bool {
    let aq_admission::SessionIdentity::Remote { session_id, .. } = result.report.binding.session
    else {
        return false;
    };
    source.manifest.role == aq_worker_protocol::SourceRole::Checkpoint
        && source.manifest.attempt.attempt_id == result.report.binding.attempt_id
        && source.manifest.attempt.session_id == session_id
        && source.manifest.attempt.generation == result.report.binding.assignment_generation
        && source.manifest.resolved_spec_digest
            == result.report.binding.execution_spec_digest.as_str()
        && source.manifest.source.repository_id == result.spec.source.repository_id
        && source.manifest.source.binding_generation
            == result.spec.source.repository_binding_generation
        && source.manifest.source.base.hex == result.spec.source.resolved_object.hex
        && source.receipt.source == source.manifest.checkpoint
        && source.receipt.retention_handle == source.manifest.artifact_id
        && source.receipt.transfer_id == source.manifest.transfer_id
        && source.receipt.repository_id == source.manifest.source.repository_id
        && source.receipt.binding_generation == source.manifest.source.binding_generation
        && result
            .report
            .source_receipt_ids
            .contains(&source.receipt.receipt_id)
}

pub fn evaluate_pilot(
    result: &RecordedResult,
    source: &RecordedSourceConfirmation,
    evidence: &Evidence<'_>,
) -> Result<EligibilityPlan, AppError> {
    let plan = |disposition, reason| {
        Ok(EligibilityPlan {
            report_id: result.report.report_id,
            disposition,
            reason: Some(reason),
        })
    };
    if !result.is_current_report_head || result.head_report_id != result.report.report_id {
        return plan(Disposition::Stale, Reason::SupersededReport);
    }
    if result.outcome.kind != OutcomeKind::SourceChangeReviewed
        || result.outcome.review != ReviewRequirement::HumanOwner
    {
        return plan(Disposition::Rejected, Reason::UnsupportedOutcome);
    }
    if !result.outcome.required_checks.is_empty() {
        return plan(Disposition::Rejected, Reason::RequiredChecksUnsupported);
    }
    let Some(policy) = evidence
        .policy
        .filter(|p| p.artifact == result.outcome.validation_policy)
    else {
        return plan(Disposition::NeedsReconciliation, Reason::UnsupportedPolicy);
    };
    if !source_matches(result, source) {
        return plan(Disposition::Stale, Reason::SourceMismatch);
    }
    evaluate_owner_evidence(result, source, evidence, policy)
}

fn evaluate_owner_evidence(
    result: &RecordedResult,
    source: &RecordedSourceConfirmation,
    evidence: &Evidence<'_>,
    policy: &PilotPolicy,
) -> Result<EligibilityPlan, AppError> {
    let plan = |disposition, reason| {
        Ok(EligibilityPlan {
            report_id: result.report.report_id,
            disposition,
            reason: Some(reason),
        })
    };
    let Some(review) = evidence.review else {
        return plan(Disposition::NeedsReconciliation, Reason::ReviewUnavailable);
    };
    if !review_matches(&review.subject, result, source, policy)? {
        return plan(Disposition::Stale, Reason::ReviewMismatch);
    }
    if !review.approved {
        return plan(Disposition::Rejected, Reason::ReviewRejected);
    }
    let Some(aq) = evidence.aq_effects.filter(|a| a.matches(result)) else {
        return plan(
            Disposition::NeedsReconciliation,
            Reason::AqEffectsUnavailable,
        );
    };
    if aq.disposition() != LedgerDisposition::Settled {
        return plan(Disposition::NeedsReconciliation, Reason::AqEffectsUnsettled);
    }
    // The exhaustive ledger is authoritative, but a still-pending worker claim
    // is not silently dropped. This finite pilot requires a corrected report
    // after those claimed effects have actually settled; empty is not proof.
    if !result.report.pending_effect_ids.is_empty() {
        return plan(
            Disposition::NeedsReconciliation,
            Reason::DeclaredEffectsPending,
        );
    }
    let Some(external) = evidence.external_effects else {
        return plan(
            Disposition::NeedsReconciliation,
            Reason::ExternalDispositionUnavailable,
        );
    };
    if external.report != result.receipt_artifact
        || external.stopped != result.receipt.stopped_receipt
        || external.source != source_digest(source)?
        || external.ledger != *aq.digest()
        || external.policy != policy.artifact
        || !decision_views_match(review, external)
    {
        return plan(
            Disposition::NeedsReconciliation,
            Reason::ExternalDispositionMismatch,
        );
    }
    let Some(applicability) = evidence.applicability else {
        return plan(
            Disposition::NeedsReconciliation,
            Reason::ApplicabilityUnavailable,
        );
    };
    if applicability.report != result.receipt_artifact
        || applicability.source != source_digest(source)?
    {
        return plan(Disposition::Stale, Reason::ChangedInputs);
    }
    if applicability.cancelled {
        return plan(Disposition::Rejected, Reason::Cancelled);
    }
    if applicability.changed {
        return plan(Disposition::Stale, Reason::ChangedInputs);
    }
    Ok(EligibilityPlan {
        report_id: result.report.report_id,
        disposition: Disposition::Eligible,
        reason: None,
    })
}

fn review_matches(
    s: &aq_review_protocol::ReviewSubject,
    result: &RecordedResult,
    source: &RecordedSourceConfirmation,
    policy: &PilotPolicy,
) -> Result<bool, AppError> {
    if !review_result_matches(s, result, policy)? {
        return Ok(false);
    }
    if !review_source_matches(&s.source, result, source)? {
        return Ok(false);
    }
    Ok(s.project_id == result.spec.lifecycle.project_id
        && s.original_project_revision == result.spec.project_revision
        && s.criteria.iter().map(|c| c.as_str()).eq(result
            .outcome
            .acceptance_criteria
            .iter()
            .map(|c| c.id.as_str()))
        && s.task.task_id == result.spec.task.task_id
        && s.task.workspace_id == result.spec.task.workspace_id
        && s.task.version == result.spec.task.version
        && s.task.content_revision == result.spec.task.content_revision)
}

fn review_result_matches(
    s: &aq_review_protocol::ReviewSubject,
    result: &RecordedResult,
    policy: &PilotPolicy,
) -> Result<bool, AppError> {
    if s.result.report_id != result.report.report_id
        || s.result.report_digest != result.receipt.report_digest
        || s.result.record_receipt != result.receipt_artifact
        || s.result.stopped_receipt != result.receipt.stopped_receipt
        || s.binding != result.report.binding
        || s.validation_policy != policy.artifact
        || s.expected_outcome != result.spec.outcome_artifact
        || s.task_observation != result.spec.task_observation
        || s.task_context != result.spec.task_context
        || s.execution_spec != result.spec.artifact().map_err(|_| AppError::Internal)?
    {
        return Ok(false);
    }
    Ok(true)
}

fn review_source_matches(
    s: &aq_review_protocol::SourcePin,
    result: &RecordedResult,
    source: &RecordedSourceConfirmation,
) -> Result<bool, AppError> {
    if s.resulting_object.hex != source.receipt.source.hex
        || s.source_receipt_id != source.receipt.receipt_id
        || s.repository_id != source.receipt.repository_id
        || s.binding_generation != source.receipt.binding_generation
        || s.base.hex != source.manifest.source.base.hex
        || s.input_artifact != result.spec.source_artifact
        || s.resulting_artifact.id.as_str()
            != format!("aq.git-import-manifest/{}", source.manifest.artifact_id)
        || s.resulting_artifact.version.as_str() != "1"
        || s.resulting_artifact.digest.as_str() != source.receipt.manifest_digest
        || s.retention_receipt.id.as_str()
            != format!("aq.git-import-receipt/{}", source.receipt.receipt_id)
        || s.retention_receipt.version.as_str() != "1"
        || s.retention_receipt.digest
            != aq_review_protocol::digest(&source.acknowledgement)
                .map_err(|_| AppError::Internal)?
    {
        return Ok(false);
    }
    Ok(true)
}

fn decision_views_match(
    review: &VerifiedReviewEvidence,
    effects: &VerifiedEffectDisposition,
) -> bool {
    match (&review.combined_subject, &effects.combined_subject) {
        // Qualified machine-derived disposition can have a separate actual
        // owner receipt. Reusing an OLD code-only approval cannot stand in.
        (None, None) => review.receipt != effects.receipt,
        // One NEW explicit combined decision may supply two typed views only
        // when both retain the exact complete combined subject and receipt.
        (Some(a), Some(b)) => a == b && review.receipt == effects.receipt,
        _ => false,
    }
}

#[cfg(test)]
#[path = "eligibility_tests.rs"]
mod tests;
