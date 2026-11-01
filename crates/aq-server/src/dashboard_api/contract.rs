//! dashboard-read-1 projections. These read claims never grant execution/control.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct Revision(#[schemars(regex(pattern = "^[1-9][0-9]{0,39}$"))] pub String);
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Timestamp(
    #[schemars(regex(
        pattern = "^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\\.[0-9]{3}Z$"
    ))]
    pub String,
);
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Digest(#[schemars(regex(pattern = "^sha256:[0-9a-f]{64}$"))] pub String);
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    AllAuthorized,
    Project { project_id: Uuid },
}
impl Scope {
    pub fn project(&self) -> Option<Uuid> {
        match self {
            Self::AllAuthorized => None,
            Self::Project { project_id } => Some(*project_id),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum ApiVersion {
    #[serde(rename = "dashboard-read-1")]
    Read1,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub state: Completeness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Reported,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub observed_at: Timestamp,
    pub fresh_until: Timestamp,
    pub source: String,
    pub quality: Quality,
    pub coverage: Completeness,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Envelope<T> {
    pub api_version: ApiVersion,
    pub request_id: Uuid,
    pub server_time: Timestamp,
    pub scope: Scope,
    pub data: T,
    pub observation: Observation,
    pub pending_operations: Vec<Uuid>,
    pub coverage: Coverage,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub enum CapabilityVersion {
    #[serde(rename = "1")]
    V1,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub name: String,
    pub version: CapabilityVersion,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub id: Uuid,
    pub display_name: String,
    pub authorization_revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Product {
    pub version: String,
    pub dashboard_digest: Option<Digest>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContractRef {
    pub name: String,
    pub version: ApiVersion,
    pub digest: Digest,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub page_size: u16,
    pub max_page_size: u16,
    pub snapshot_members: u16,
    pub snapshot_ttl_ms: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Supervisor {
    pub status: String,
    pub conversation_id: Option<Uuid>,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub principal: Principal,
    pub product: Product,
    pub contracts: Vec<ContractRef>,
    pub capabilities: Vec<Capability>,
    pub limits: Limits,
    pub default_scope: Scope,
    pub global_supervisor: Supervisor,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Accuracy {
    Exact,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Total {
    pub value: Option<u32>,
    pub accuracy: Accuracy,
}
impl Total {
    pub fn unknown() -> Self {
        Self {
            value: None,
            accuracy: Accuracy::Unknown,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub snapshot_id: Uuid,
    pub total: Total,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectRef {
    pub id: Uuid,
    pub label: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TitledRef {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    pub title: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionIdentity {
    Remote {
        node_id: Uuid,
        boot_id: Uuid,
        attempt_id: Uuid,
        session_id: Uuid,
        assignment_generation: Revision,
    },
    Central {
        authority_id: Uuid,
        session_id: Uuid,
        generation: Revision,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub harness: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessObservation {
    pub state: String,
    pub observed_at: Timestamp,
    pub fresh_until: Timestamp,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitRef {
    pub id: Uuid,
    pub explanation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Controller {
    pub state: String,
    pub label: Option<String>,
}
/// Historical authenticated worker receipts, independent of process freshness.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionLifecycle {
    pub started_at: Option<Timestamp>,
    pub stopped_at: Option<Timestamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub id: Uuid,
    pub label: String,
    pub task: Option<TitledRef>,
    pub attempt_id: Option<Uuid>,
    pub project: ProjectRef,
    pub profile: Profile,
    pub authority: String,
    pub observation: ProcessObservation,
    pub wait: Option<WaitRef>,
    pub controller: Controller,
    pub terminal: Option<EntityRef>,
    pub session_identity: SessionIdentity,
    pub record_revision: Option<Revision>,
    pub execution_authority: String,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<SessionLifecycle>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileRevision {
    pub name: String,
    pub revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub profile_revisions: Vec<ProfileRevision>,
    pub permissions: Vec<String>,
    pub last_activity_at: Option<Timestamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub running: Total,
    pub ready: Total,
    pub needs_you: Total,
    pub recovering: Total,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub kind: String,
    pub explanation: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InMotion {
    pub project: ProjectRef,
    pub running: Total,
    pub next_constraint: Option<Constraint>,
    pub last_progress_at: Option<Timestamp>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Overview {
    pub counts: Counts,
    #[schemars(length(max = 0))]
    pub attention: Vec<serde_json::Value>,
    pub in_motion: Vec<InMotion>,
    #[schemars(length(max = 0))]
    pub recent_outcomes: Vec<serde_json::Value>,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Response {
    Bootstrap(Envelope<Bootstrap>),
    Overview(Envelope<Overview>),
    Sessions(Envelope<Page<SessionSummary>>),
    Session(Box<Envelope<SessionDetail>>),
    Projects(Envelope<Page<ProjectRef>>),
}
pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Response)).expect("static schema")
}
pub fn schema_bytes() -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&schema()).expect("static schema encoding");
    bytes.push(b'\n');
    bytes
}
