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

/// B4 backfill: replays the D3 owner for finally reworked attempts that still
/// hold their task without both replacement facts (for example a rework that
/// was recorded before its source receipt, or before this owner existed). The
/// owner re-checks everything under its locks; this cursor supplies no proof.
#[derive(Default)]
pub struct ReworkSweep {
    after: Option<Uuid>,
}
impl ReworkSweep {
    pub async fn tick(&mut self, state: &AppState) -> Result<Progress, AppError> {
        let reports = super::settlement::reworked_without_facts(state, self.after, PAGE).await?;
        if reports.is_empty() {
            self.after = None;
            return Ok(Progress::default());
        }
        let mut progress = Progress::default();
        for report in reports {
            self.after = Some(report);
            progress.inspected += 1;
            match super::settlement::record_replacement_facts(state, report).await {
                Ok(facts) if facts.changed => progress.closed += 1,
                Ok(_) => {}
                Err(_) => progress.refused += 1,
            }
        }
        Ok(progress)
    }
}
/// B5b/AQ-08: settles held granted attempts the worker closed before Started
/// (`LaunchClosedAwaitingSettlement`). The closure settlement owner re-checks
/// everything under its locks; this cursor supplies no proof.
#[derive(Default)]
pub struct ClosureSweep {
    after: Option<Uuid>,
}
impl ClosureSweep {
    pub async fn tick(&mut self, state: &AppState) -> Result<Progress, AppError> {
        let attempts = super::closure::settlement::awaiting(state, self.after, PAGE).await?;
        if attempts.is_empty() {
            self.after = None;
            return Ok(Progress::default());
        }
        let mut progress = Progress::default();
        for attempt in attempts {
            self.after = Some(attempt);
            progress.inspected += 1;
            match super::closure::settlement::settle_and_release(state, attempt).await {
                Ok(super::closure::settlement::ClosureSettlement::Settled) => {
                    tracing::info!(%attempt, "closed launch settled: not started, source retained, effects quiescent");
                    progress.closed += 1
                }
                Ok(super::closure::settlement::ClosureSettlement::Released) => {
                    tracing::info!(%attempt, "closed launch settled and its task claim released for B4 replacement");
                    progress.closed += 1
                }
                Ok(super::closure::settlement::ClosureSettlement::Held(reason)) => {
                    tracing::debug!(%attempt, reason, "closed launch kept held");
                }
                Ok(_) => {}
                Err(_) => progress.refused += 1,
            }
        }
        Ok(progress)
    }
}
const REWORK_SWEEP_EVERY: u32 = 15;

/// Central-only housekeeping. No new scheduler, external service, or worker
/// connection is required. Shutdown can cancel an uncertain close; its atomic
/// owner receipt, not this cursor, remains authoritative on the next startup.
pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    if state.config.profiled_execution.is_none() {
        return;
    }
    let mut sweep = Sweep::default();
    let mut rework = ReworkSweep::default();
    let mut closures = ClosureSweep::default();
    let mut terminations = crate::agent_access::human::task_termination::Sweep::default();
    let mut tick_count: u32 = 0;
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut warn_after = tokio::time::Instant::now();
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            _ = ticks.tick() => {}
        }
        tick_count = tick_count.wrapping_add(1);
        if tick_count.is_multiple_of(3) {
            let _ = tokio::time::timeout(Duration::from_secs(4), terminations.tick(&state)).await;
        }
        if tick_count.is_multiple_of(REWORK_SWEEP_EVERY) {
            tokio::select! {
                biased;
                _ = stop.wait_for(|value| *value) => return,
                outcome = tokio::time::timeout(Duration::from_secs(4), rework.tick(&state)) => {
                    if let Ok(Ok(progress)) = outcome {
                        tracing::debug!(inspected=progress.inspected, recorded=progress.closed, refused=progress.refused, "rework replacement-facts sweep");
                    }
                }
            }
            tokio::select! {
                biased;
                _ = stop.wait_for(|value| *value) => return,
                outcome = tokio::time::timeout(Duration::from_secs(4), closures.tick(&state)) => {
                    if let Ok(Ok(progress)) = outcome {
                        tracing::debug!(inspected=progress.inspected, settled=progress.closed, refused=progress.refused, "launch closure settlement sweep");
                    }
                }
            }
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
