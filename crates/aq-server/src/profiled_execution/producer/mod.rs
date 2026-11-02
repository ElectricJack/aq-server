//! Daemon-owned native offer assembly. No provider calls or caller-supplied
//! compositions, readiness, retention or qualification success flags.
pub mod api;
mod artifacts;
mod build;
mod compose;
use super::native_offer::observation;

mod publication;
mod redelivery;
mod result_startup;
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

/// Actual configured whole adapter definitions, not runtime labels, select the
/// one supported long interactive factory.
pub(super) fn interactive_runtime(
    config: &super::config::ProfiledConfig,
    runtime: &aq_admission::ArtifactRef,
) -> Result<bool, AppError> {
    result_startup::interactive_runtime(runtime, &config.definition_bodies)
}
pub(super) fn interactive_adapter(
    state: &AppState,
    adapter: &aq_admission::ArtifactRef,
) -> Result<bool, AppError> {
    Ok(result_startup::for_adapter(state, adapter)?.is_some_and(|v| v.interactive))
}
