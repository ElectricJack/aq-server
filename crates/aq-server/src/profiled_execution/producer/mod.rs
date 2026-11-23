//! Daemon-owned native offer assembly. No provider calls or caller-supplied
//! compositions, readiness, retention or qualification success flags.
pub mod api;
mod artifacts;
mod build;
mod compose;
pub(crate) mod fairness;
use super::native_offer::observation;

mod publication;
mod redelivery;
mod result_startup;
mod startup;
mod workflow;
pub(crate) use super::native_offer::load_original_offer_locked;
use crate::{AppState, error::AppError};
pub use workflow::poll;

async fn task_git_author<'e>(
    state: &AppState,
    executor: impl sqlx::PgExecutor<'e>,
    project: uuid::Uuid,
    workspace: uuid::Uuid,
    task: uuid::Uuid,
) -> Result<Option<aq_human_work::task_git_author::TaskGitAuthor>, AppError> {
    if predates_task_git_author(state) {
        return Ok(None);
    }
    aq_human_work::task_git_author::lookup(executor, project, workspace, task)
        .await
        .map_err(crate::error::db)
}

/// Historical test fixtures explicitly pin a schema prior to the task git
/// author store. Current stores must propagate lookup errors, including a
/// missing table.
#[cfg(feature = "test-support")]
fn predates_task_git_author(state: &AppState) -> bool {
    const TASK_GIT_AUTHOR_FORMAT: u32 = 162;
    state
        .store
        .fixture_profile
        .is_some_and(|v| v < TASK_GIT_AUTHOR_FORMAT)
}
#[cfg(not(feature = "test-support"))]
fn predates_task_git_author(_state: &AppState) -> bool {
    false
}

fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| refused())
}
use super::on_heap;
#[track_caller]
pub(super) fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
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
    Ok(result_startup::for_adapter(state, adapter)?.is_some_and(|v| v.long_phase))
}
