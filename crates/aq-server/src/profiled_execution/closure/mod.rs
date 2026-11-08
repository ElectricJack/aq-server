//! Terminal launch closure is independent of task/source/effect release.
pub(crate) mod api;
mod context;
pub(crate) mod history;
pub mod policy;
pub mod service;
pub mod settlement;
use crate::error::AppError;
use aq_admission::LaunchBinding;
use serde::Serialize;
use sqlx::{Postgres, Transaction};
fn conflict() -> AppError {
    AppError::Conflict("granted launch closure history unavailable or inconsistent".into())
}
pub(crate) fn database(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
/// Strict schema28 fence. Caller already holds the current node→attempt/hot
/// locks. A missing table is an error, never permission to mint fresh authority.
pub(crate) async fn ensure_launch_open_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<(), AppError> {
    let closed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_launch_closures WHERE attempt_id=$1)",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database)?;
    if closed {
        Err(AppError::Conflict(
            "launch is terminally closed pending settlement".into(),
        ))
    } else {
        Ok(())
    }
}
