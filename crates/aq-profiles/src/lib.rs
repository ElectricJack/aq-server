//! Deterministic composition, immutable catalog artifacts and activation CAS.
//! No provider selection, authority service, database or process launch.
mod catalog;
pub mod central;
mod compile;
mod composition_core;
mod graph_policy;
pub use graph_policy::*;
mod registry;
mod selection_policy;
use aq_session_protocol::Digest;
pub use catalog::*;
pub use compile::*;
pub use registry::*;
pub use selection_policy::*;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid profile catalog: {0}")]
    Invalid(String),
    #[error("catalog activation revision conflict")]
    RevisionConflict,
    #[error("immutable identity has conflicting content: {0}")]
    IdentityConflict(String),
    #[error("catalog artifact is retained or active")]
    Retained,
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(message.into()))
    }
}
fn text(s: &str) -> Result<()> {
    ensure(
        !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control),
        "nonempty bounded reference required",
    )
}
pub fn digest<T: Serialize>(v: &T) -> Result<String> {
    let mut json = serde_json::to_value(v).map_err(|e| Error::Invalid(e.to_string()))?;
    json.sort_all_objects();
    let bytes = serde_json::to_vec(&json).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(Digest::of(&bytes).as_str().into())
}
