use super::*;
use crate::validation::{hex, label};
pub fn local_reasons(
    record: &ProjectRecord,
    request: &EligibilityRequest,
    config: &ProjectConfig,
    node_live: bool,
) -> Vec<String> {
    let b = &record.binding;
    let mut reasons = Vec::new();
    if b.validate(config).is_err() {
        reasons.push("project_policy_invalid".into());
    }
    if !b.enabled {
        reasons.push("project_disabled".into());
    }
    if record.revision != request.expected_project_revision {
        reasons.push("project_revision_changed".into());
    }
    if !request.admits_node(b, config, node_live) {
        reasons.push("node_not_eligible".into());
    }
    if !b.allowed_environments.contains(&request.environment) {
        reasons.push("environment_denied".into());
    }
    request.check_launch_references(&mut reasons);
    reasons
}
impl EligibilityRequest {
    fn admits_node(&self, binding: &ProjectBinding, config: &ProjectConfig, live: bool) -> bool {
        !self.node_id.is_nil() && live && binding.eligible_node(config, self.node_id)
    }
    fn check_launch_references(&self, reasons: &mut Vec<String>) {
        for value in [
            &self.role,
            &self.intelligence_profile,
            &self.execution_profile,
        ] {
            if label(value).is_err() {
                reasons.push("launch_reference_invalid".into());
            }
        }
        if !hex(&self.snapshot_digest) {
            reasons.push("snapshot_digest_invalid".into());
        }
    }
}

pub struct EligibilityEvidence<'a> {
    observation: &'a BindingObservation,
    node_live: bool,
    now_ms: u64,
}
impl<'a> EligibilityEvidence<'a> {
    pub fn new(observation: &'a BindingObservation, node_live: bool, now_ms: u64) -> Self {
        Self {
            observation,
            node_live,
            now_ms,
        }
    }
}
pub fn decide(
    record: &ProjectRecord,
    request: &EligibilityRequest,
    config: &ProjectConfig,
    evidence: EligibilityEvidence,
) -> Eligibility {
    let mut reasons = local_reasons(record, request, config, evidence.node_live);
    let observation = evidence.observation;
    observation.check(&record.binding, evidence.now_ms, &mut reasons);
    let eligible = reasons.is_empty();
    Eligibility {
        eligible,
        reasons,
        frozen_binding: eligible.then(|| FrozenProjectBinding {
            project: record.clone(),
            snapshot_digest: request.snapshot_digest.clone(),
            worker_membership_digest: aq_profiles::digest(config)
                .expect("bounded membership serializes"),
            binding_observation: observation.clone(),
            node_id: request.node_id,
            environment: request.environment.clone(),
            role: request.role.clone(),
            intelligence_profile: request.intelligence_profile.clone(),
            execution_profile: request.execution_profile.clone(),
        }),
    }
}

impl BindingObservation {
    fn matches(&self, binding: &ProjectBinding) -> bool {
        self.repository_id == binding.repository_id
            && self.graph_workspace_id == binding.graph_workspace_id
            && self.repository_adapter_reference == binding.repository_adapter_reference
            && self.graph_adapter_reference == binding.graph_adapter_reference
    }
    fn current_at(&self, now_ms: u64) -> bool {
        self.observed_at_ms != 0
            && (self.observed_at_ms..self.expires_at_ms).contains(&now_ms)
            && self.expires_at_ms.saturating_sub(self.observed_at_ms) <= 30_000
    }
    fn check(&self, binding: &ProjectBinding, now_ms: u64, reasons: &mut Vec<String>) {
        if !self.matches(binding) {
            reasons.push("binding_observation_identity_mismatch".into());
        }
        if !self.current_at(now_ms) {
            reasons.push("binding_observation_stale".into());
        }
        if self.repository != ExternalStatus::Verified {
            reasons.push(format!("repository_{:?}", self.repository).to_lowercase());
        } else if self.repository_generation != Some(binding.repository_binding_generation) {
            reasons.push("repository_stale".into());
        }
        if self.graph != ExternalStatus::Verified {
            reasons.push(format!("graph_{:?}", self.graph).to_lowercase());
        }
    }
}
