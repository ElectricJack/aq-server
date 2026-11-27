//! Negotiated global provisionability and independent one-grant presence facts.
//! No observation here issues execution authority or marks a global key installed.
mod qualification;
mod storage;
fn changed() -> crate::error::AppError {
    crate::error::AppError::Conflict(
        "scoped material observation is incompatible or no longer current".into(),
    )
}
fn unavailable() -> crate::error::AppError {
    crate::error::AppError::UpdatesUnavailable(
        "scoped material owner temporarily unavailable".into(),
    )
}
fn now(state: &crate::AppState) -> Result<u64, crate::error::AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, crate::error::AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| crate::error::AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| crate::error::AppError::Internal)
}

pub mod inventory;
mod private_inventory;
pub(crate) use storage::legacy as record_legacy_inventory_locked;

/// A presence transaction. The presence pool bounds its statements, row
/// locks and idle time (`reserved_*` in `[workers.limits]`).
pub(super) async fn begin(
    state: &crate::AppState,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, crate::error::AppError> {
    state.store.pool.begin().await.map_err(|_| unavailable())
}

mod anchor;
pub mod report;

mod continuation;
pub(crate) use continuation::{HeldMaterial, scoped_locked};

mod api;
pub use api::routes;

mod current_inventory;
pub(crate) use current_inventory::{current_inventory_locked, inventory_epoch_locked};
