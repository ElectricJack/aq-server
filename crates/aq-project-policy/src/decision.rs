//! Selection artifacts derived from a frozen project binding and its role snapshot.
mod budget;
use crate::{FrozenProjectBinding, ProjectCeilings};
use aq_profiles::roles::{Launch, Snapshot};
use aq_selection::{
    AuthorizedCredential, AvailabilityObservation, ExecutionTarget, PreparedLaunch,
    ProjectSnapshot, ReplacementEvidence, ResourceCeilings, RouteLimits, Scope, SelectionInputs,
    SelectionOutcome, TaskLineage, Text, digest,
};
pub use budget::{Accounting, AccountingSnapshot, RetryHistory};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub decision_id: Uuid,
    pub project: FrozenProjectBinding,
    pub inputs: SelectionInputs,
    pub outcome: SelectionOutcome,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub operation_id: Uuid,
    pub lineage: TaskLineage,
    pub project: FrozenProjectBinding,
    /// The launch rendered for each installed harness (spec 0014).
    pub launches: Vec<PreparedLaunch>,
    /// Owner-assessed failed-predecessor replacement (B4). Absent for a first
    /// attempt; legacy history without it stays conservatively unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<ReplacementBudget>,
    /// False only for a scheduling poll: a refused selection is then not
    /// recorded, so refused polls cannot fill the decision table (spec 0007).
    /// Not part of the request payload or its digest.
    #[serde(skip, default = "yes")]
    pub record_refused: bool,
    /// A routed task's harness and provider limits (spec 0011 §4); absent unrouted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteLimits>,
}
fn yes() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementBudget {
    pub attempts_started: u32,
    /// Owner and service handoffs (spec 0007 Q1); not charged to the policy.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exempt_attempts: u32,
    pub elapsed_ms: u64,
    pub evidence: ReplacementEvidence,
}
fn is_zero(v: &u32) -> bool {
    *v == 0
}

pub enum SnapshotError {
    Digest,
    Operation,
    Scope,
}

impl ResolveRequest {
    pub fn launch<'a>(&self, snapshot: &'a Snapshot) -> Result<Launch<'a>, &'static str> {
        if snapshot.digest() != self.project.snapshot_digest {
            return Err("role snapshot differs from the frozen binding");
        }
        let launch = snapshot
            .resolve(&self.project.role, &self.project.intelligence_profile)
            .map_err(|_| "role snapshot does not resolve the frozen launch")?;
        let binding = &self.project.project.binding;
        let lineage_matches = (self.lineage.project_id, self.lineage.workspace_id)
            == (binding.project_id, binding.graph_workspace_id);
        if [self.operation_id, self.lineage.task_id].contains(&Uuid::nil()) || !lineage_matches {
            return Err("trusted resolver subject mismatch");
        }
        Ok(launch)
    }
    pub fn project_snapshot(
        &self,
        role_digest: &str,
        target: &ExecutionTarget,
    ) -> Result<ProjectSnapshot, SnapshotError> {
        let ceiling = &self.project.project.binding.ceilings;
        Ok(ProjectSnapshot {
            lineage: self.lineage.clone(),
            revision: self.project.project.revision,
            role_snapshot_digest: serde_json::from_value(json!(role_digest))
                .map_err(|_| SnapshotError::Digest)?,
            admission_snapshot_digest: digest(&self.project).map_err(|_| SnapshotError::Digest)?,
            expires_at_ms: self.project.binding_observation.expires_at_ms,
            target: target.clone(),
            ceilings: ceiling.resources(),
            allowed_environments: vec![self.project.environment.clone()],
            permission_ceiling: ceiling
                .permissions
                .iter()
                .map(|p| {
                    Ok(Scope {
                        operation: Text::new(&p.operation).map_err(|_| SnapshotError::Operation)?,
                        resource: Text::new(&p.scope_reference)
                            .map_err(|_| SnapshotError::Scope)?,
                    })
                })
                .collect::<Result<Vec<_>, SnapshotError>>()?,
            role: self.project.role.clone(),
            intelligence_profile: self.project.intelligence_profile.clone(),
            execution_profile: self.project.execution_profile.clone(),
        })
    }

    pub fn take_replacement(
        &mut self,
        started: u32,
    ) -> Result<Option<ReplacementBudget>, &'static str> {
        match self.replacement.take() {
            Some(r) if !r.matches(&self.lineage, started) => {
                Err("replacement budget does not match lineage history")
            }
            replacement => Ok(replacement),
        }
    }
}
impl ReplacementBudget {
    fn matches(&self, lineage: &TaskLineage, started: u32) -> bool {
        started > 0
            && self.attempts_started == started
            && self.exempt_attempts <= started
            && self.evidence.lineage == *lineage
    }
}
impl ProjectCeilings {
    /// Cost ceilings without hard enforcement are observed by the lineage spend ledger.
    pub fn soft_cost_limit(&self) -> Option<(u64, &str)> {
        if self.require_hard_cost_enforcement {
            return None;
        }
        Some((self.cost_limit_microunits?, self.currency.as_deref()?))
    }

    fn resources(&self) -> ResourceCeilings {
        ResourceCeilings {
            context_bytes: self.context_bytes,
            wall_time_ms: self.wall_time_ms,
            memory_bytes: self.memory_bytes,
            cpu_millis: self.cpu_millis,
            process_limit: self.process_limit,
            cost_limit_microunits: self.cost_limit_microunits,
            currency: self.currency.clone(),
            require_hard_cost_enforcement: self.require_hard_cost_enforcement,
        }
    }
}
impl Decision {
    pub fn valid_at(&self, time: u64) -> bool {
        let latest = [
            self.inputs.node.expires_at_ms,
            self.inputs.project.expires_at_ms,
            self.inputs.budget.expires_at_ms,
        ]
        .into_iter()
        .min()
        .expect("three bounds");
        (self.inputs.now_ms..latest).contains(&time)
            && self
                .outcome
                .resolved
                .as_ref()
                .is_none_or(|outcome| time < outcome.expires_at_ms)
    }
}

pub fn relevant(
    observations: Vec<AvailabilityObservation>,
    credentials: &[AuthorizedCredential],
    node: Uuid,
) -> Vec<AvailabilityObservation> {
    observations
        .into_iter()
        .filter(|o| {
            o.scope.node_id.is_none_or(|id| id == node)
                && credentials.iter().any(|c| {
                    c.profile == o.scope.credential_profile
                        && c.revision == o.scope.credential_revision
                        && c.provider == o.scope.provider
                        && c.account_reference == o.scope.account_reference
                        && c.endpoint_reference == o.scope.endpoint_reference
                })
        })
        .collect()
}
