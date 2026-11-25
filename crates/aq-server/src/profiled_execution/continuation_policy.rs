//! Immutable pinned policy/budget inputs (`aq_profiled_state::continuation_policy`).
use crate::error::AppError;
use aq_profiled_state::continuation_policy::{self as policy, PolicyError};
use aq_profiles::SelectionPolicy;
fn error(e: PolicyError) -> AppError {
    match e {
        PolicyError::Refused => {
            AppError::Conflict("pinned native policy budget unavailable or exhausted".into())
        }
        PolicyError::Internal => AppError::Internal,
    }
}
/// The selection policy the attempt's launch pinned; `None` when it cannot be read (spec 0007
/// Q1). Its limits are the task's retry budget (`replacement::limits`).
pub(crate) async fn pinned_policy(
    conn: &mut sqlx::PgConnection,
    attempt: uuid::Uuid,
) -> Result<Option<SelectionPolicy>, AppError> {
    policy::pinned_policy(conn, attempt).await.map_err(error)
}
pub(crate) async fn load_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    binding: &aq_admission::LaunchBinding,
) -> Result<SelectionPolicy, AppError> {
    policy::load_locked(tx, binding).await.map_err(error)
}
