//! Pure deterministic selection over bounded snapshots. No inputs confer authority.
//! Server-owned adapters authenticate facts and recheck them at actual admission.
mod candidate_core;
mod credential_policy;
pub use credential_policy::*;
pub mod central;
mod domain;
mod explanation;
pub use explanation::human_selection;
mod feedback;
pub use feedback::*;
mod placement;
pub use placement::*;
mod select;
mod validate;
pub use aq_session_protocol::{ArtifactRef, Digest, Scope, Text, Validate};
pub use domain::*;
pub use select::{OFFER_SELECTION_TTL_MS, select, select_for_offer, select_legacy_for_inspection};
use serde::Serialize;
#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("invalid selection snapshot: {0}")]
    Invalid(String),
}
pub type Result<T> = std::result::Result<T, SelectionError>;
fn ensure(ok: bool, reason: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(SelectionError::Invalid(reason.into()))
    }
}
pub fn digest<T: Serialize>(value: &T) -> Result<Digest> {
    let mut json =
        serde_json::to_value(value).map_err(|_| SelectionError::Invalid("serialization".into()))?;
    json.sort_all_objects();
    let bytes =
        serde_json::to_vec(&json).map_err(|_| SelectionError::Invalid("serialization".into()))?;
    Ok(Digest::of(&bytes))
}
