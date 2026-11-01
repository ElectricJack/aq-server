//! Neutral structural records and pure transitions, not authenticated authority.
//! This crate cannot execute, dispatch, reply, access credentials or persist data.
#![forbid(unsafe_code)]
mod contracts;
mod reducer;
use aq_session_protocol::Validate;
pub use aq_session_protocol::{ArtifactRef, Digest, SessionIdentity, Text};
pub use contracts::*;
pub use reducer::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const MAX_WIRE_BYTES: usize = 65536;
pub const MAX_BATCH: usize = 16;
pub const MAX_WALL_MS: u64 = 3600000;
pub const MAX_PERMIT_MS: u64 = 120000;
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid central data: {0}")]
    Invalid(&'static str),
    #[error("central model transition refused: {0:?}")]
    Refused(Refusal),
}
pub type Result<T> = std::result::Result<T, Error>;
fn ensure(v: bool, s: &'static str) -> Result<()> {
    if v { Ok(()) } else { Err(Error::Invalid(s)) }
}
fn id(v: Uuid) -> Result<()> {
    ensure(!v.is_nil(), "nil identity")
}
fn revision(v: u64) -> Result<()> {
    ensure(v > 0 && v <= i64::MAX as u64, "revision range")
}
fn artifact(v: &ArtifactRef) -> Result<()> {
    v.id.validate().map_err(|_| Error::Invalid("artifact id"))?;
    v.version
        .validate()
        .map_err(|_| Error::Invalid("artifact version"))
}
fn central(v: &SessionIdentity, generation: u64) -> Result<()> {
    v.validate()
        .map_err(|_| Error::Invalid("session identity"))?;
    revision(generation)?;
    ensure(
        matches!(v,SessionIdentity::Central{generation:g,..}if *g==generation),
        "central generation mismatch or remote identity",
    )
}
fn digest<T: Serialize>(v: &T) -> Result<Digest> {
    let mut json = serde_json::to_value(v).map_err(|_| Error::Invalid("encoding"))?;
    json.sort_all_objects();
    let bytes = serde_json::to_vec(&json).map_err(|_| Error::Invalid("encoding"))?;
    ensure(bytes.len() <= MAX_WIRE_BYTES, "wire bound")?;
    Ok(Digest::of(&bytes))
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "document",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Document {
    Plan(CentralPlan),
    Permit(PermitData),
    Dispatch(DispatchAssociation),
    Event(Event),
    State(Run),
    Transition(Transition),
    Reply(ReplyAssociation),
    History(MailHistoryFence),
}
impl Document {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Plan(v) => v.validate(),
            Self::Permit(v) => v.validate(),
            Self::Dispatch(v) => v.validate(),
            Self::Event(v) => v.validate(),
            Self::State(v) => v.validate(),
            Self::Transition(v) => v.state.validate(),
            Self::Reply(v) => v.validate(),
            Self::History(v) => v.validate(),
        }
    }
}
pub fn decode(bytes: &[u8]) -> Result<Document> {
    ensure(
        !bytes.is_empty() && bytes.len() <= MAX_WIRE_BYTES,
        "wire bound",
    )?;
    let value: Document =
        serde_json::from_slice(bytes).map_err(|_| Error::Invalid("closed document shape"))?;
    value.validate()?;
    Ok(value)
}
pub fn encode(value: &Document) -> Result<Vec<u8>> {
    value.validate()?;
    let bytes = serde_json::to_vec(value).map_err(|_| Error::Invalid("encoding"))?;
    ensure(bytes.len() <= MAX_WIRE_BYTES, "wire bound")?;
    Ok(bytes)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RuntimeCapabilities {
    execution: bool,
    mail_dispatch: bool,
    replies: bool,
}
impl RuntimeCapabilities {
    pub const fn execution(&self) -> bool {
        self.execution
    }
    pub const fn mail_dispatch(&self) -> bool {
        self.mail_dispatch
    }
    pub const fn replies(&self) -> bool {
        self.replies
    }
}
pub const fn runtime_capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        execution: false,
        mail_dispatch: false,
        replies: false,
    }
}
pub fn schema() -> serde_json::Value {
    let mut s = serde_json::to_value(schemars::schema_for!(Document)).expect("static schema");
    s["$id"] = serde_json::json!("urn:agent-q:central-authority:1");
    s["title"] = serde_json::json!("AQ central authority neutral data v1");
    s["x-max-wire-bytes"] = serde_json::json!(MAX_WIRE_BYTES);
    s["x-execution-authority"] = serde_json::json!(false);
    // The shared Rust identity enum is reused; this new contract admits Central only.
    if let Some(branches) = s["$defs"]["SessionIdentity"]["oneOf"].as_array_mut() {
        branches.retain(|b| b["properties"]["kind"]["const"] == "central");
    }
    s
}
