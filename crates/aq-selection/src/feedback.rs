//! Narrow classified worker failures. Authentication and sealed-attempt lookup
//! belong to the server; generic lifecycle provider-error text is insufficient.
use crate::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailure {
    Quota,
    RateLimited,
    Authentication,
    ModelUnsupported,
    Temporary,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredFailure {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub selection_digest: Digest,
    pub node_id: Uuid,
    pub boot_id: Uuid,
    pub classifier: ArtifactRef,
    pub sequence: u64,
    pub failure: ProviderFailure,
    pub lifecycle_event_id: Option<Uuid>,
}
/// These arguments must come from authenticated current daemon records. This
/// function derives scope; no worker-chosen account/model/endpoint is accepted.
pub struct FailureContext<'a> {
    pub observation_id: Uuid,
    pub authenticated_node: Uuid,
    pub current_boot: Uuid,
    pub sealed_attempt: Uuid,
    pub sealed: &'a ResolvedSelection,
    pub revision: u64,
    pub now_ms: u64,
    pub ttl_ms: u64,
    pub cooldown_ms: u64,
}
pub fn derive_worker_observation(
    report: &StructuredFailure,
    context: &FailureContext<'_>,
) -> Result<AvailabilityObservation> {
    let FailureContext {
        observation_id,
        authenticated_node,
        current_boot,
        sealed_attempt,
        sealed,
        revision,
        now_ms,
        ttl_ms,
        cooldown_ms,
    } = *context;
    context.validate_timing(report)?;
    context.validate_binding(report)?;
    let state = match report.failure {
        ProviderFailure::Quota => Availability::QuotaUnavailable,
        ProviderFailure::RateLimited | ProviderFailure::Temporary => {
            Availability::TemporaryUnavailable
        }
        ProviderFailure::Authentication => Availability::AuthenticationFailed,
        ProviderFailure::ModelUnsupported => Availability::ModelUnsupported,
    };
    let result = AvailabilityObservation {
        observation_id,
        scope: ObservationScope {
            credential_revision: sealed.credential_revision,
            provider: sealed.choice.provider.clone(),
            credential_profile: sealed.choice.credential_profile.clone(),
            account_reference: sealed.choice.account_reference.clone(),
            endpoint_reference: sealed.choice.endpoint_reference.clone(),
            model: Some(sealed.choice.model.clone()),
            node_id: Some(authenticated_node),
        },
        source: ObservationSource::Worker {
            node_id: authenticated_node,
            boot_id: current_boot,
            attempt_id: sealed_attempt,
            selection_digest: report.selection_digest.clone(),
        },
        sequence: report.sequence,
        revision,
        state,
        received_at_ms: now_ms,
        expires_at_ms: now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| SelectionError::Invalid("expiry overflow".into()))?,
        cooldown_until_ms: now_ms
            .checked_add(cooldown_ms)
            .ok_or_else(|| SelectionError::Invalid("cooldown overflow".into()))?,
    };
    result.validate()?;
    Ok(result)
}

impl FailureContext<'_> {
    fn validate_timing(&self, report: &StructuredFailure) -> Result<()> {
        ensure(
            !self.observation_id.is_nil()
                && report.schema_version == 1
                && !report.operation_id.is_nil()
                && !self.sealed_attempt.is_nil()
                && report.sequence > 0
                && self.valid_observation_window(),
            "failure identity/timing",
        )?;
        ensure(
            report.lifecycle_event_id.is_none_or(|id| !id.is_nil()),
            "lifecycle provenance identity",
        )?;
        Ok(())
    }
    fn valid_observation_window(&self) -> bool {
        self.revision > 0
            && self.now_ms > 0
            && (1000..=3_600_000).contains(&self.ttl_ms)
            && self.cooldown_ms <= self.ttl_ms
    }
    fn validate_binding(&self, report: &StructuredFailure) -> Result<()> {
        ensure(
            report.node_id == self.authenticated_node
                && report.boot_id == self.current_boot
                && report.attempt_id == self.sealed_attempt
                && self.sealed.target
                    == ExecutionTarget::Worker {
                        node_id: self.authenticated_node,
                        boot_id: self.current_boot,
                    },
            "failure current sealed attempt/boot",
        )?;
        ensure(
            report.selection_digest == digest(self.sealed)?
                && self.sealed.choice.failure_classifier.as_ref() == Some(&report.classifier),
            "failure classifier/selection qualification",
        )?;
        Ok(())
    }
}
