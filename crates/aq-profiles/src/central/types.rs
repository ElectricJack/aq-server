use super::*;
use schemars::JsonSchema;
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CentralProfileSubject {
    CentralConversation {
        actor_id: Uuid,
        conversation_id: Uuid,
        conversation_revision: u64,
        policy: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    CentralProfileComposition,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralPolicy {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub kind: PolicyKind,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub conversation_revision: u64,
    pub revision: u64,
    pub enabled: bool,
    pub catalog_digest: String,
    pub agent_profile: String,
    pub compatibility_digest: Digest,
    #[schemars(length(max = 128))]
    pub permissions: Vec<Permission>,
    pub context_limit_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityKind {
    CentralConversation,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralCompatibility {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub kind: CompatibilityKind,
    pub catalog_digest: String,
    pub agent_profile: Source,
    pub intelligence_profile: Source,
    pub execution_profile: Source,
    #[schemars(length(max = 128))]
    pub modules: Vec<Source>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralCeilings {
    #[schemars(length(max = 128))]
    pub installation: Vec<Permission>,
    #[schemars(length(max = 128))]
    pub execution: Vec<Permission>,
    #[schemars(length(max = 128))]
    pub caller: Vec<Permission>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailMessage {
    pub message_id: Uuid,
    pub cursor: u64,
    pub payload_digest: Digest,
    pub body_digest: Digest,
    pub body: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailTurn {
    pub conversation_id: Uuid,
    pub through_cursor: u64,
    #[schemars(length(min = 1, max = 16))]
    pub messages: Vec<MailMessage>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralCompileRequest {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub subject: CentralProfileSubject,
    pub policy: CentralPolicy,
    pub compatibility: CentralCompatibility,
    pub ceilings: CentralCeilings,
    #[schemars(length(max = 128))]
    pub available_tools: Vec<String>,
    #[schemars(length(max = 128))]
    pub available_adapters: Vec<String>,
    pub harness: String,
    pub environment: String,
    pub platform: ContextInput,
    pub turn: MailTurn,
    #[schemars(length(max = 128))]
    pub context: Vec<ContextInput>,
    #[schemars(length(max = 16))]
    pub overrides: Vec<Override>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Request,
    Policy,
    Compatibility,
    Compiler,
    Renderer,
    Composition,
    MailFrame,
    Startup,
}
impl ArtifactKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Policy => "policy",
            Self::Compatibility => "compatibility",
            Self::Compiler => "compiler",
            Self::Renderer => "renderer",
            Self::Composition => "composition",
            Self::MailFrame => "mail-frame",
            Self::Startup => "startup",
        }
    }
}
/// Actual generated UTF-8 data, never retained storage or authenticated authority.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratedArtifact {
    pub kind: ArtifactKind,
    pub reference: ArtifactRef,
    pub utf8: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SelectionState {
    Unresolved,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityState {
    None,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralComposition {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub subject: CentralProfileSubject,
    pub request: ArtifactRef,
    pub catalog: ArtifactRef,
    pub compiler: ArtifactRef,
    pub compatibility: ArtifactRef,
    pub renderer: ArtifactRef,
    pub mail_frame: ArtifactRef,
    pub agent_profile: Source,
    pub intelligence_profile: Source,
    pub execution_profile: Source,
    #[schemars(length(max = 128))]
    pub modules: Vec<Source>,
    pub harness: String,
    pub environment: String,
    pub instruction_order: Vec<Instruction>,
    pub requested_permissions: Vec<Permission>,
    pub effective_permissions: Vec<Permission>,
    pub denied_requirements: Vec<Permission>,
    pub context_limit_bytes: u64,
    pub required_startup_bytes: u64,
    pub optional_startup_bytes: u64,
    pub omissions: Vec<Omission>,
    pub overrides: Vec<OverrideExplanation>,
    pub reference_material: Vec<Source>,
    pub startup: ArtifactRef,
    pub startup_bytes: u64,
    pub selection: SelectionState,
    pub selected_model: (),
    pub token_count: (),
    pub cost: (),
    pub authority: AuthorityState,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralBundle {
    pub composition: CentralComposition,
    pub request: GeneratedArtifact,
    pub policy: GeneratedArtifact,
    pub compatibility: GeneratedArtifact,
    pub compiler: GeneratedArtifact,
    pub renderer: GeneratedArtifact,
    pub mail_frame: GeneratedArtifact,
    pub startup: GeneratedArtifact,
    pub artifact: GeneratedArtifact,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralCompilation {
    pub catalog_valid: bool,
    pub composition_valid: bool,
    pub blocking_reasons: Vec<String>,
    pub bundle: Option<CentralBundle>,
    pub selection: SelectionState,
    pub authority: AuthorityState,
}
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "document",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Document {
    Policy(CentralPolicy),
    Compatibility(CentralCompatibility),
    Request(CentralCompileRequest),
    Composition(CentralComposition),
    Compilation(Box<CentralCompilation>),
    Artifact(GeneratedArtifact),
}
