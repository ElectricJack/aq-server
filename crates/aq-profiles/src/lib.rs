//! Roles and profiles (spec 0014): the role, intelligence profile and
//! execution profile documents and the snapshot they compile into
//! ([`roles`]), the selection policy of a launch, and the graph access policy
//! sealed for it. Pure: no provider selection, database or process launch.
mod confinement;
pub use confinement::ExecutionConfinement;
mod graph_policy;
pub use graph_policy::*;
pub mod roles;
mod selection_policy;
use aq_session_protocol::Digest;
pub use selection_policy::*;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid: {0}")]
    Invalid(String),
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
