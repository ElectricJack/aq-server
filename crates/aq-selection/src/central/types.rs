//! Closed data only. No snapshot, signature-looking ref or result grants authority.
use crate::{
    ArtifactRef, AuthorizedCredential, AvailabilityObservation, CandidateTrace, Digest,
    InstalledHarness, ResourceCeilings,
};
use aq_profiles::central::{CentralBundle, CentralProfileSubject};
use aq_session_api::Source;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pinned<T> {
    pub reference: ArtifactRef,
    pub value: T,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationPolicyKind {
    GlobalSupervisorReservation,
}
/// Exact schema37 LocalPolicy data shape, not an executable policy adapter.
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservationPolicy {
    pub schema_version: u32,
    pub kind: ReservationPolicyKind,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub enabled: bool,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReservedMail {
    pub message_id: Uuid,
    pub cursor: u64,
    pub payload_digest: Digest,
}
/// Actual bounded source projection. Pure validity does not authenticate a hold.
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReservationSnapshot {
    pub reservation_id: Uuid,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub conversation_revision: u64,
    pub actor_revision: u64,
    pub reservation_revision: u64,
    pub policy_revision: u64,
    pub policy: Pinned<ReservationPolicy>,
    pub through_cursor: u64,
    pub messages: Vec<ReservedMail>,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CentralAttemptPolicy {
    InitialSelectionOnly,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CentralPolicyKind {
    CentralTurnSelection,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralTurnPolicy {
    pub schema_version: u32,
    pub revision: u64,
    pub kind: CentralPolicyKind,
    pub subject: CentralProfileSubject,
    pub reservation: ArtifactRef,
    pub reservation_policy: ArtifactRef,
    pub composition_policy: ArtifactRef,
    pub compatibility: ArtifactRef,
    pub catalog: ArtifactRef,
    pub agent_profile: Source,
    pub intelligence_profile: Source,
    pub execution_profile: Source,
    pub enabled: bool,
    pub attempts: CentralAttemptPolicy,
    pub allow_unknown_cost: bool,
    pub ceilings: ResourceCeilings,
    pub max_output_tokens: u64,
    pub allowed_environments: Vec<ArtifactRef>,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrySnapshot {
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub credentials: Vec<AuthorizedCredential>,
    pub observations: Vec<AvailabilityObservation>,
}
/// Installed local presence data only; no worker delivery/preparation target.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralCredentialBinding {
    pub credential_revision: u64,
    pub credential_profile: ArtifactRef,
    pub available: bool,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
/// Local installation incarnation, deliberately not a Run generation or worker.
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSnapshot {
    pub installation_id: Uuid,
    pub incarnation_id: Uuid,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub harnesses: Vec<InstalledHarness>,
    pub credential_bindings: Vec<CentralCredentialBinding>,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralSelectionInputs {
    pub schema_version: u32,
    pub reservation: Pinned<ReservationSnapshot>,
    pub policy: Pinned<CentralTurnPolicy>,
    pub registry: Pinned<RegistrySnapshot>,
    pub installation: Pinned<InstallationSnapshot>,
    pub compositions: Vec<CentralBundle>,
    pub now_ms: u64,
    pub original_expires_at_ms: u64,
}
#[derive(JsonSchema, Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountingState {
    Unavailable,
}
#[derive(JsonSchema, Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityState {
    None,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralChoice {
    pub candidate_id: String,
    pub profile_candidate_id: String,
    pub model_settings: aq_session_protocol::ModelSettings,
    pub reasoning_configuration_reference: String,
    pub harness_settings: aq_session_protocol::HarnessSettings,
    pub qualification: ArtifactRef,
    pub credential_profile: ArtifactRef,
    pub credential_revision: u64,
    pub account_reference: String,
    pub endpoint_reference: String,
    pub composition: ArtifactRef,
    pub startup: ArtifactRef,
    pub resources: ResourceCeilings,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedCentralSelection {
    pub schema_version: u32,
    pub subject: CentralProfileSubject,
    pub reservation: ArtifactRef,
    pub policy: ArtifactRef,
    pub reservation_policy: ArtifactRef,
    pub composition_policy: ArtifactRef,
    pub compatibility: ArtifactRef,
    pub catalog: ArtifactRef,
    pub agent_profile: Source,
    pub intelligence_profile: Source,
    pub execution_profile: Source,
    pub registry: ArtifactRef,
    pub installation: ArtifactRef,
    pub snapshot_digest: Digest,
    pub choice: CentralChoice,
    pub selected_at_ms: u64,
    pub expires_at_ms: u64,
    pub accounting: AccountingState,
    pub token_count: (),
    pub spent_microunits: (),
    pub authority: AuthorityState,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralSelectionArtifact {
    pub reference: ArtifactRef,
    pub utf8: String,
    pub selection: ResolvedCentralSelection,
}
#[derive(JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CentralSelectionOutcome {
    pub resolved: Option<CentralSelectionArtifact>,
    pub traces: Vec<CandidateTrace>,
    pub reason_codes: Vec<String>,
    pub accounting: AccountingState,
    pub token_count: (),
    pub spent_microunits: (),
    pub authority: AuthorityState,
}
