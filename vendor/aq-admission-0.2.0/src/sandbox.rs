//! Pure delegated sandbox contracts. Validation is never authentication or a grant.
mod fixtures;
mod permit;
mod readoption;
mod registration;
mod renewal;
use crate::transport::WorkerIdentity;
pub use fixtures::*;
pub use permit::*;
pub use readoption::*;
pub use registration::*;
pub use renewal::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_SANDBOX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_SANDBOXES: usize = 128;
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, thiserror::Error,
)]
#[serde(rename_all = "snake_case")]
#[error("{self:?}")]
pub enum ContractError {
    InvalidShape,
    BindingMismatch,
    ScopeDenied,
    DeadlineMismatch,
    Expired,
    ReplayConflict,
    UnsupportedRequiredCapability,
}
pub type Result<T> = std::result::Result<T, ContractError>;
fn check(ok: bool, error: ContractError) -> Result<()> {
    if ok { Ok(()) } else { Err(error) }
}
fn nonnil(value: Uuid) -> Result<()> {
    check(!value.is_nil(), ContractError::InvalidShape)
}
fn positive(value: u64) -> Result<()> {
    check(
        value > 0 && value <= i64::MAX as u64,
        ContractError::InvalidShape,
    )
}
fn node(value: &WorkerIdentity) -> Result<()> {
    nonnil(value.node_id)?;
    nonnil(value.boot_id)
}
fn label(value: &str, max: usize) -> Result<()> {
    check(
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        ContractError::InvalidShape,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SandboxDocument {
    Permit(DelegatedPermit),
    PhaseAuthority(PhaseExecutionAuthority),
    RunnerRenewalRequest(RunnerRenewalRequest),
    RunnerRenewalResponse(RunnerRenewalResponse),
    ReadoptionRequest(ReadoptionRequest),
    ReadoptionResponse(ReadoptionResponse),
    Register(Registration),
    Renewal(RenewalReply),
    Channel(RunnerChannelIdentity),
    Compatibility(CompatibilityReply),
}
impl SandboxDocument {
    /// Shape only. Call the request/context-dependent validator before use.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Permit(v) => v.validate(),
            Self::PhaseAuthority(v) => v.validate(),
            Self::RunnerRenewalRequest(v) => v.validate(),
            Self::RunnerRenewalResponse(v) => v.validate(),
            Self::ReadoptionRequest(v) => v.validate(),
            Self::ReadoptionResponse(v) => v.validate(),
            Self::Register(v) => v.validate(),
            Self::Renewal(v) => v.validate(),
            Self::Channel(v) => v.validate(),
            Self::Compatibility(v) => v.validate(),
        }
    }
}
pub fn decode_document(bytes: &[u8]) -> Result<SandboxDocument> {
    check(
        bytes.len() <= MAX_SANDBOX_FRAME_BYTES,
        ContractError::InvalidShape,
    )?;
    let document: SandboxDocument =
        serde_json::from_slice(bytes).map_err(|_| ContractError::InvalidShape)?;
    document.validate()?;
    Ok(document)
}
pub fn schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(SandboxDocument)).expect("sandbox schema");
    schema["$comment"] = serde_json::json!(
        "Maximum encoded frame 1 MiB. Rust validators additionally enforce nonnil identities, UTF-8 byte bounds, duplicate rejection, currency and harness constraints, exact request/node/boot/attempt/generation/sandbox/channel correlation, operation scope, original and current deadlines, and replay equality. Authentication, revocation, current incarnation fencing, channel allocation uniqueness, aggregate limits and atomic replay persistence are server responsibilities. Awaiting node re-adoption grants no deadline extension."
    );
    schema
}
