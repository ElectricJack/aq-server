//! Actual profiled owner, separate from fixture execution wire and providers.
//! Runtime preparation never turns a pure proposal or qualification reference
//! into authority without the locked project/selection/source/material fences.
pub mod api;
pub mod artifact_api;
pub mod artifacts;
pub(crate) mod compute;
pub mod config;
pub use aq_selection::context_budget;
pub mod context_projection;
pub mod expiry;
pub mod git_grant;
/// Enrollment applies when assembling a new offer. The persisted grant is
/// authoritative once the offer is published.
pub(crate) fn delivery_enrolled(
    state: &crate::AppState,
    project: uuid::Uuid,
    task: uuid::Uuid,
) -> bool {
    state
        .config
        .delivery
        .as_ref()
        .is_some_and(|d| d.enrolls(project, task))
}

pub(crate) async fn has_delivery_grant<'e>(
    db: impl sqlx::PgExecutor<'e>,
    attempt: uuid::Uuid,
) -> Result<bool, crate::error::AppError> {
    aq_delivery::git_grant::exists(db, attempt)
        .await
        .map_err(|_| crate::error::AppError::Maintenance("git grants unavailable".into()))
}

pub mod handoff;
pub mod journal;
pub mod login_refresh;
pub mod lost;
pub mod material;
pub mod material_api;
pub mod no_grant;
pub mod placement;
pub mod policies;
pub mod replacement;

pub mod phase_terminal;
pub mod registration;
pub mod source;
pub mod started;

pub mod disposition;
pub mod stopped;

pub mod human_results;
pub mod results;
pub mod settlement;
pub mod source_exports;

pub mod source_api;

pub mod definitions;

pub mod preparation;

pub mod control_api;

pub mod initial_grant;
pub mod native_offer;
pub mod producer;

pub(crate) mod continuation_policy;
pub mod reconciliation;

pub mod closure;
pub mod renewal;

pub mod accepted_preparation;
pub mod accepted_refresh;
mod phase_profile;
pub mod presence;

pub mod phase_execution;
pub mod phase_refresh_execution;
pub mod phase_refresh_terminal;

pub(crate) mod initial_authority;

pub mod cancellation;

/// Builds a large owner future in this helper's own frame and moves it to the
/// heap. Unoptimized builds give every temporary future its own slot in the
/// caller's poll frame, so awaiting large owners inline made the native offer
/// and preparation paths use nearly all of a default 2 MiB test-thread stack.
/// The caller keeps only a pointer.
pub(crate) fn on_heap<F, Fut>(build: F) -> std::pin::Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    Box::pin(build())
}
