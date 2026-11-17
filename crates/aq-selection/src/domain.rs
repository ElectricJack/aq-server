use aq_session_protocol::{ArtifactRef, Capability, Digest};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTarget {
    Worker { node_id: Uuid, boot_id: Uuid },
    Central { authority_id: Uuid, generation: u64 },
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskLineage {
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub task_id: Uuid,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCeilings {
    pub context_bytes: u64,
    pub wall_time_ms: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub process_limit: u32,
    pub cost_limit_microunits: Option<u64>,
    pub currency: Option<String>,
    pub require_hard_cost_enforcement: bool,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSnapshot {
    pub lineage: TaskLineage,
    pub revision: u64,
    /// The digest of the role snapshot the launch was resolved from.
    pub role_snapshot_digest: Digest,
    pub admission_snapshot_digest: Digest,
    pub expires_at_ms: u64,
    pub target: ExecutionTarget,
    pub ceilings: ResourceCeilings,
    pub permission_ceiling: Vec<aq_session_protocol::Scope>,
    pub allowed_environments: Vec<String>,
    pub role: String,
    pub intelligence_profile: String,
    pub execution_profile: String,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLimits {
    pub context_bytes: u64,
    pub max_output_tokens: u64,
    pub wall_time_ms: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u64,
    pub process_limit: u32,
    pub hard_cost_enforcer: Option<ArtifactRef>,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledHarness {
    pub harness: String,
    pub cli_version: String,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
    pub qualification: ArtifactRef,
    pub failure_classifier: Option<ArtifactRef>,
    pub capabilities: Vec<Capability>,
    pub limits: NativeLimits,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialBinding {
    pub credential_revision: u64,
    pub credential_profile: ArtifactRef,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<ProvisioningReadiness>,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitySnapshot {
    /// Trusted backend identity; absent until the node adapter supplies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_kind: Option<crate::BackendKind>,
    pub target: ExecutionTarget,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub harnesses: Vec<InstalledHarness>,
    pub credential_bindings: Vec<CredentialBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_delivery: Option<DeliveryCapability>,
}
impl CapabilitySnapshot {
    /// What a selection was resolved against: the target (node and boot), the
    /// installed harnesses, their limits, the credential bindings and delivery.
    /// The revision and the validity windows change on every republish and are
    /// left out, so a later report of the same boot with the same installation
    /// is the same content.
    pub fn content(&self) -> Self {
        let mut content = self.clone();
        (
            content.revision,
            content.observed_at_ms,
            content.expires_at_ms,
        ) = (0, 0, 0);
        for binding in &mut content.credential_bindings {
            (binding.observed_at_ms, binding.expires_at_ms) = (0, 0);
            if let Some(preparation) = &mut binding.preparation {
                (preparation.observed_at_ms, preparation.expires_at_ms) = (0, 0);
            }
        }
        content
    }
    /// Whether `current`, the node's stored report, still carries what this
    /// snapshot was resolved against: never an older report, never another
    /// boot, never a changed installation.
    pub fn carried_by(&self, current: &Self) -> bool {
        current.revision >= self.revision
            && matches!(
                (crate::digest(&self.content()), crate::digest(&current.content())),
                (Ok(mine), Ok(theirs)) if mine == theirs
            )
    }
}
/// Operator authority to provision a specific profile/account/endpoint. Presence
/// is separate from the node's installed binding and provider availability.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizedCredential {
    /// Derived from the operator credential profile, independently of node bindings.
    #[serde(
        default = "crate::default_allowed_backends",
        skip_serializing_if = "crate::unrestricted_backends"
    )]
    pub allowed_backends: Vec<crate::BackendKind>,
    pub profile: ArtifactRef,
    pub provider: String,
    pub account_reference: String,
    pub endpoint_reference: String,
    pub revision: u64,
    pub enabled: bool,
    pub provisionable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material_owner: Option<ArtifactRef>,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObservationScope {
    pub credential_revision: u64,
    pub provider: String,
    pub credential_profile: ArtifactRef,
    pub account_reference: String,
    pub endpoint_reference: String,
    pub model: Option<String>,
    pub node_id: Option<Uuid>,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationSource {
    Operator {
        principal_id: Uuid,
    },
    Worker {
        node_id: Uuid,
        boot_id: Uuid,
        attempt_id: Uuid,
        selection_digest: Digest,
    },
}
#[derive(JsonSchema, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    QuotaUnavailable,
    TemporaryUnavailable,
    AuthenticationFailed,
    ModelUnsupported,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AvailabilityObservation {
    pub observation_id: Uuid,
    pub scope: ObservationScope,
    pub source: ObservationSource,
    pub sequence: u64,
    pub revision: u64,
    pub state: Availability,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
    pub cooldown_until_ms: u64,
}
#[derive(JsonSchema, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccountingCompleteness {
    Complete,
    Incomplete,
    Unavailable,
}
fn is_zero(v: &u32) -> bool {
    *v == 0
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSnapshot {
    pub lineage: TaskLineage,
    pub revision: u64,
    pub attempts_started: u32,
    /// Started attempts that ended in an owner or service handoff (spec 0007
    /// Q1): not charged to the policy's attempt limit. Zero is omitted, so
    /// every earlier decision keeps its bytes and digest.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exempt_attempts: u32,
    pub last_attempt_id: Option<Uuid>,
    pub elapsed_ms: Option<u64>,
    pub spent_microunits: Option<u64>,
    pub currency: Option<String>,
    pub accounting: AccountingCompleteness,
    pub accounting_source: ArtifactRef,
    pub hard_budget_provider: Option<ArtifactRef>,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
/// Actual owning-domain receipts are required for replacement. Runtime adapters
/// must obtain and verify them; no caller-supplied `stopped=true` can create this.
/// These DTO references alone do not authenticate or establish receipt validity.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementEvidence {
    pub lineage: TaskLineage,
    pub predecessor_attempt_id: Uuid,
    pub stop_receipt: ArtifactRef,
    pub retained_source_receipt: ArtifactRef,
    pub effect_reconciliation_receipt: ArtifactRef,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateEstimate {
    pub credential_profile: ArtifactRef,
    pub provider: String,
    pub model: String,
    pub microunits: u64,
    pub currency: String,
    pub rate_card: ArtifactRef,
}
/// One startup text an offer could launch with: the role, level and execution
/// profile of the project snapshot, rendered for one installed harness. The
/// digest is the server's, over the rendered text and the permissions.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreparedLaunch {
    pub digest: Digest,
    pub harness: String,
    pub environment: String,
    pub startup_bytes: u64,
    /// The execution profile's startup byte limit after the project ceiling.
    pub startup_limit_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionInputs {
    pub project: ProjectSnapshot,
    pub node: CapabilitySnapshot,
    pub credentials: Vec<AuthorizedCredential>,
    pub observations: Vec<AvailabilityObservation>,
    pub registry_revision: u64,
    pub budget: BudgetSnapshot,
    pub replacement: Option<ReplacementEvidence>,
    pub estimates: Vec<CandidateEstimate>,
    pub now_ms: u64,
    /// Sandbox placement facts (C4); absent for host-native workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<crate::Placement>,
    /// A routed task's harness and provider limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::RouteLimits>,
    /// The startup texts of the launch, one per harness and environment
    /// (spec 0014).
    pub launches: Vec<PreparedLaunch>,
}
#[derive(JsonSchema, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Eligible,
    MissingCredentials,
    Unavailable,
    Unsupported,
    StaleUnknown,
    PolicyDenied,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateTrace {
    pub candidate_id: String,
    pub profile_candidate_id: String,
    pub provider: String,
    pub model: String,
    pub harness: String,
    pub environment: String,
    pub credential_profile: Option<ArtifactRef>,
    pub state: CandidateState,
    pub reason_codes: Vec<String>,
    pub observation_ids: Vec<Uuid>,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConcreteChoice {
    pub candidate_id: String,
    pub profile_candidate_id: String,
    pub provider: String,
    pub model: String,
    pub native_reasoning: String,
    pub harness: String,
    pub cli_version: String,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
    pub qualification: ArtifactRef,
    pub failure_classifier: Option<ArtifactRef>,
    pub credential_profile: ArtifactRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_delivery: Option<CredentialDelivery>,
    pub account_reference: String,
    pub endpoint_reference: String,
    /// The digest of the prepared launch this choice runs.
    pub launch_digest: Digest,
    pub resources: ResourceCeilings,
    pub max_output_tokens: u64,
    pub estimate: Option<CandidateEstimate>,
}
/// Immutable selection metadata, never a runnable offer, grant or replacement
/// permit. AQ-08 binds exact assignment/session/source/outcome/context artifacts.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedSelection {
    pub schema_version: u32,
    pub lineage: TaskLineage,
    pub target: ExecutionTarget,
    pub choice: ConcreteChoice,
    /// The digest of the selection inputs.
    pub snapshot_digest: Digest,
    pub role_snapshot_digest: Digest,
    pub project_revision: u64,
    pub node_revision: u64,
    pub registry_revision: u64,
    pub credential_revision: u64,
    pub budget_revision: u64,
    pub selected_at_ms: u64,
    pub expires_at_ms: u64,
    pub traces: Vec<CandidateTrace>,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionOutcome {
    pub resolved: Option<ResolvedSelection>,
    pub traces: Vec<CandidateTrace>,
    pub reason_codes: Vec<String>,
}

pub trait BudgetProvider {
    fn snapshot(&self, lineage: &TaskLineage, now_ms: u64) -> Result<BudgetSnapshot, String>;
}
pub trait ReplacementEvidenceProvider {
    fn evidence(
        &self,
        lineage: &TaskLineage,
        now_ms: u64,
    ) -> Result<Option<ReplacementEvidence>, String>;
}

/// Supported delivery is independent of credential presence or authorization.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryCapability {
    pub adapter: ArtifactRef,
    pub version: u32,
}
/// Daemon-derived readiness for later exact-attempt material preparation. This
/// is not an installed key, delivery receipt or runnable grant.
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisioningReadiness {
    pub owner: ArtifactRef,
    pub credential_profile: ArtifactRef,
    pub credential_revision: u64,
    pub target: ExecutionTarget,
    pub delivery: DeliveryCapability,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialDelivery {
    ServerLease {
        owner: ArtifactRef,
        adapter: ArtifactRef,
        version: u32,
    },
}

/// Whether a stored selection was recorded before roles and profiles (spec
/// 0014): it names the old catalog and not a role snapshot. Such a row is
/// attempt history; no type here reads its shape.
pub fn recorded_before_roles(selection: &serde_json::Value) -> bool {
    selection.get("catalog_digest").is_some() && selection.get("role_snapshot_digest").is_none()
}
impl ResolvedSelection {
    /// Whether this selection disagrees with the launch manifest its attempt
    /// was offered, on the node `target`.
    pub fn differs_from(
        &self,
        manifest: &aq_session_protocol::LaunchManifest,
        target: &ExecutionTarget,
    ) -> bool {
        let choice = &self.choice;
        self.role_snapshot_digest != manifest.catalog.digest
            || self.project_revision != manifest.bindings.project_revision
            || self.lineage.task_id != manifest.bindings.task_id
            || self.lineage.workspace_id != manifest.bindings.graph_id
            || self.lineage.project_id != manifest.bindings.project_id
            || self.target != *target
            || choice.provider != manifest.model.provider.as_str()
            || choice.model != manifest.model.model.as_str()
            || choice.native_reasoning != manifest.model.reasoning.as_str()
            || choice.max_output_tokens != manifest.model.max_output_tokens
            || choice.harness != manifest.harness.harness.as_str()
            || choice.cli_version != manifest.harness.cli_version.as_str()
            || choice.adapter != manifest.harness.adapter
            || choice.environment != manifest.harness.environment
            || choice.resources.memory_bytes != manifest.resources.memory_bytes
            || choice.resources.cpu_millis != manifest.resources.cpu_millis
            || choice.resources.process_limit != manifest.resources.process_limit
            || choice.resources.wall_time_ms != manifest.resources.wall_time_ms
            || manifest.credentials.is_empty()
            || manifest
                .credentials
                .iter()
                .any(|c| c.credential_profile != choice.credential_profile)
    }
}
