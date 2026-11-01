//! Disposable keyset cursor over held pre-grant claims. Only the existing
//! transactional NeverGranted owner can close one; this loop supplies no proof.
use crate::{AppState, error::AppError};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

const PAGE: i64 = 8;
#[derive(Default)]
pub struct Sweep {
    after: Option<Uuid>,
}
#[derive(Default, Debug)]
pub struct Progress {
    pub inspected: usize,
    pub closed: usize,
    pub refused: usize,
}
impl Sweep {
    /// Output and work count are bounded, not the PostgreSQL scan cost. Every
    /// candidate advances the disposable cursor even when its owner refuses.
    pub async fn tick(&mut self, state: &AppState) -> Result<Progress, AppError> {
        let mut tx = state
            .store
            .pool
            .begin()
            .await
            .map_err(|_| AppError::Internal)?;
        sqlx::query("SET LOCAL statement_timeout='2s'")
            .execute(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
        let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT a.attempt_id,a.node_id FROM execution_attempts a \
             JOIN profiled_authority_hot h USING(attempt_id) \
             WHERE a.kind='profiled_v1' AND a.holds_task \
             AND h.state->>'status' IN ('offered','preparing') \
             AND (h.state->'grant' IS NULL OR h.state->'grant'='null'::jsonb) \
             AND ($1::UUID IS NULL OR a.attempt_id>$1) \
             ORDER BY a.attempt_id LIMIT $2",
        )
        .bind(self.after)
        .bind(PAGE)
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        if candidates.is_empty() {
            self.after = None;
            return Ok(Progress::default());
        }
        let mut progress = Progress::default();
        for (attempt, node) in candidates {
            self.after = Some(attempt);
            progress.inspected += 1;
            match super::no_grant::close_expired(state, node, attempt).await {
                Ok(Some(closure)) if closure.changed => progress.closed += 1,
                Ok(_) => {}
                Err(_) => progress.refused += 1,
            }
        }
        Ok(progress)
    }
}

/// Central-only housekeeping. No new scheduler, external service, or worker
/// connection is required. Shutdown can cancel an uncertain close; its atomic
/// owner receipt, not this cursor, remains authoritative on the next startup.
pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    if state.config.profiled_execution.is_none() {
        return;
    }
    let mut sweep = Sweep::default();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut warn_after = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            _ = ticks.tick() => {}
        }
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            outcome = tokio::time::timeout(Duration::from_secs(4), sweep.tick(&state)) => {
                let incomplete = match outcome {
                    Ok(Ok(progress)) => {
                        tracing::debug!(inspected=progress.inspected, closed=progress.closed, refused=progress.refused, "native ungranted expiry sweep");
                        progress.refused > 0
                    }
                    _ => true,
                };
                if incomplete && tokio::time::Instant::now() >= warn_after {
                    tracing::warn!("native expiry sweep incomplete; unclosed attempts remain held");
                    warn_after = tokio::time::Instant::now() + Duration::from_secs(30);
                }
            }
        }
    }
}
