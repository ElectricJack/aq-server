use aq_profiles::FrozenComposition;
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
    pub catalog_revision: u64,
    pub catalog_digest: Digest,
    pub admission_snapshot_digest: Digest,
    pub expires_at_ms: u64,
    pub target: ExecutionTarget,
    pub ceilings: ResourceCeilings,
    pub permission_ceiling: Vec<aq_session_protocol::Scope>,
    pub allowed_environments: Vec<String>,
    pub agent_profile: String,
    pub intelligence_profile: String,
    pub execution_profile: String,
}
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModelSupport {
    pub provider: String,
    pub model: String,
    pub reasoning_configuration_reference: String,
    pub native_reasoning: String,
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
    pub models: Vec<NativeModelSupport>,
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
#[derive(JsonSchema, Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetSnapshot {
    pub lineage: TaskLineage,
    pub revision: u64,
    pub attempts_started: u32,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedComposition {
    pub digest: Digest,
    pub composition: FrozenComposition,
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
    pub compositions: Vec<PreparedComposition>,
    pub now_ms: u64,
    /// Sandbox placement facts (C4); absent for host-native workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<crate::Placement>,
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
    pub reasoning_configuration_reference: String,
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
    pub composition_digest: Digest,
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
    pub snapshot_digest: Digest,
    pub catalog_digest: Digest,
    pub project_revision: u64,
    pub catalog_revision: u64,
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
