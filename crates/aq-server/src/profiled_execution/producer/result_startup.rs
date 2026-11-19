//! Server access to the pure, pinned result-startup contract.
use crate::{AppState, error::AppError, profiled_execution::definitions::Definition};
use aq_admission::ArtifactRef;
pub(super) use aq_result_startup::{ReportingInstruction, STARTUP_ID};
pub(super) fn from_definitions(
    adapter: &ArtifactRef,
    definitions: &[Definition],
) -> Result<Option<ReportingInstruction>, AppError> {
    aq_result_startup::from_definitions(
        adapter,
        definitions.iter().map(|d| (d.artifact(), d.body())),
    )
    .map_err(|_| super::refused())
}
pub(super) fn interactive_runtime(
    runtime: &ArtifactRef,
    definitions: &[Definition],
) -> Result<bool, AppError> {
    aq_result_startup::interactive_runtime(
        runtime,
        definitions.iter().map(|d| (d.artifact(), d.body())),
    )
    .map_err(|_| super::refused())
}
pub(super) fn for_adapter(
    state: &AppState,
    adapter: &ArtifactRef,
) -> Result<Option<ReportingInstruction>, AppError> {
    if !matches!(
        adapter.id.as_str(),
        "aq.codex-result-local-adapter" | "aq.claude-result-local-adapter"
    ) {
        return Ok(None);
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(super::refused)?;
    from_definitions(adapter, &config.definition_bodies)
}
