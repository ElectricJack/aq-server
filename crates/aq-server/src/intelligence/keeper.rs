//! Spec 0007 Q2: owner availability is sticky. While the owner's switch is on,
//! the keeper keeps every owner `available` observation fresh, so an always-on
//! swarm is not cut off by the selection policy's observation age. It never
//! refreshes over negative worker feedback (`aq_machine_control`).
use crate::{AppState, error::AppError};
use aq_machine_control::{self as mc, Availability};
use std::time::Duration;
use tokio::sync::watch;

pub const KEEPER_INTERVAL: Duration = Duration::from_secs(10);
pub const REFRESH_AFTER_MS: u64 = 15_000;

pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    let mut interval = tokio::time::interval(KEEPER_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = stop.changed() => if changed.is_err() || *stop.borrow() { break },
            _ = interval.tick() => {
                let now = u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
                if let Err(e) = refresh_once(&state, now).await {
                    tracing::warn!(error = ?e, "owner availability keeper deferred; it retries");
                }
            }
        }
    }
}

/// One keeper pass: 0 unless the store has the switch and it is on.
pub async fn refresh_once(state: &AppState, now_ms: u64) -> Result<u32, AppError> {
    let db = |_| AppError::Internal;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    if state.config.intelligence.is_none()
        || !mc::has_format(&mut tx)
            .await
            .map_err(|_| AppError::Internal)?
        || mc::owner_availability(&mut tx)
            .await
            .map_err(|_| AppError::Internal)?
            .state
            != Availability::On
    {
        return Ok(0);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(712009)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let registered = super::registry::check_current(state, &mut tx).await?;
    let revision = registered.checked_add(1).ok_or(AppError::Internal)?;
    let n = mc::refresh_owner_available(&mut tx, now_ms, REFRESH_AFTER_MS, revision)
        .await
        .map_err(|_| AppError::Internal)?;
    tx.commit().await.map_err(db)?;
    Ok(n)
}
