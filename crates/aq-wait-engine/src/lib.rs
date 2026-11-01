//! Deterministic plans only. The caller owns authentication, source-owner proof,
//! durable CAS/holds/outbox, IO admission and execution/parking authority.
pub use aq_lifecycle_api as wire;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wire::{Validate, canonical_digest};
mod cycle;
mod model;
mod reduce;
pub use cycle::*;
pub use model::*;
pub const MAX_SNAPSHOT_BYTES: usize = 262_144;
pub const MAX_TIME_MS: u64 = 9_007_199_254_740_991;
pub const MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid bounded wait input")]
    Invalid,
    #[error("wait identity or payload conflict")]
    Conflict,
    #[error("stale wait generation or source work")]
    Stale,
    #[error("wait source proof is unqualified or incomplete")]
    Proof,
    #[error("known wait cycle")]
    Cycle,
    #[error("bounded wait proof or work capacity exceeded")]
    Capacity,
    #[error("wait arithmetic overflow")]
    Overflow,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<wire::ContractError> for Error {
    fn from(_: wire::ContractError) -> Self {
        Self::Invalid
    }
}
fn require(ok: bool) -> Result<()> {
    if ok { Ok(()) } else { Err(Error::Invalid) }
}
fn instant(at: u64) -> Result<()> {
    require(at > 0 && at <= MAX_TIME_MS)
}
fn add(at: u64, duration: u64) -> Result<u64> {
    at.checked_add(duration)
        .filter(|v| *v <= MAX_TIME_MS)
        .ok_or(Error::Overflow)
}

#[cfg(test)]
mod tests;
