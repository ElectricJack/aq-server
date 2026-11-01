//! Daemon-owned native offer assembly. No provider calls or caller-supplied
//! compositions, readiness, retention or qualification success flags.
pub mod api;
mod artifacts;
mod build;
mod compose;
use super::native_offer::observation;

mod publication;
mod redelivery;
mod startup;
mod workflow;
pub(crate) use super::native_offer::load_original_offer_locked;
use crate::{AppState, error::AppError};
pub use workflow::poll;
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(refused)
}
fn refused() -> AppError {
    AppError::Conflict("native offer admission requirements unavailable".into())
}
