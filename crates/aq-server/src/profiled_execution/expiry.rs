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
        let candidates =
            aq_machine_control::attempts::ungranted_candidates(&state.store.pool, self.after, PAGE)
                .await
                .map_err(crate::error::db)?;
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
            match super::closure::settlement::settle_and_release(state, attempt, None).await {
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

<<<<<<< HEAD
enum SweepOutcome {
    Stopped,
    Incomplete,
    Complete(Progress),
}

async fn run_sweep(
    stop: &mut watch::Receiver<bool>,
    sweep: impl std::future::Future<Output = Result<Progress, AppError>>,
) -> SweepOutcome {
    tokio::select! {
        biased;
        _ = stop.wait_for(|value| *value) => SweepOutcome::Stopped,
        outcome = tokio::time::timeout(Duration::from_secs(4), sweep) => match outcome {
            Ok(Ok(progress)) => SweepOutcome::Complete(progress),
            _ => SweepOutcome::Incomplete,
        }
    }
=======
/// Playbook completions are replayed through their grant and intent owner.
/// Only default-policy Graph effects belong in this independent sweep.
pub async fn check_graph_completion(
    state: &AppState,
    after: &mut Option<Uuid>,
) -> Result<(), AppError> {
    let report = aq_profiled_state::graph_completion::next_due(
        &state.store.pool,
        *after,
        (state.clock)().timestamp_millis(),
    )
    .await
    .map_err(crate::error::db)?;
    *after = report;
    if let Some(report) = report {
        super::settlement::settle_after_record(state, report).await?;
    }
    Ok(())
>>>>>>> refs/remotes/central/main
}

/// Central-only housekeeping. No new scheduler, external service, or worker
/// connection is required. Shutdown can cancel an uncertain close; its atomic
/// owner receipt, not this cursor, remains authoritative on the next startup.
pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    let profiled = state.config.profiled_execution.is_some();
    let retry_checks = state.agent_access.is_some();
    if !profiled && !retry_checks {
        return;
    }
    let mut sweep = Sweep::default();
    let mut rework = ReworkSweep::default();
    let mut closures = ClosureSweep::default();
    let mut graph_completion = None;
    let mut terminations = crate::agent_access::human::task_termination::Sweep::default();
    let retry_reads = crate::agent_access::human::ReadRuntime::new(&state);
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
        if retry_checks && tick_count.is_multiple_of(3) {
            let retry =
                crate::agent_access::human::task_execution::check_retry(&state, &retry_reads);
            if let Ok(Err(error)) = tokio::time::timeout(Duration::from_secs(8), retry).await {
                tracing::debug!(?error, "owner task retry check deferred");
            }
        }
        if !profiled {
            continue;
        }
        if tick_count.is_multiple_of(3) {
            let _ = tokio::time::timeout(Duration::from_secs(4), terminations.tick(&state)).await;
        }
        if tick_count.is_multiple_of(REWORK_SWEEP_EVERY) {
<<<<<<< HEAD
            match run_sweep(&mut stop, rework.tick(&state)).await {
                SweepOutcome::Stopped => return,
                SweepOutcome::Complete(progress) => {
                    tracing::debug!(
                        inspected = progress.inspected,
                        recorded = progress.closed,
                        refused = progress.refused,
                        "rework replacement-facts sweep"
                    );
=======
            tokio::select! {
                biased;
                _ = stop.wait_for(|value| *value) => return,
                outcome = tokio::time::timeout(Duration::from_secs(8), check_graph_completion(&state, &mut graph_completion)) => {
                    if let Ok(Err(error)) = outcome {
                        tracing::debug!(?error, "Graph completion check deferred");
                    }
                }
            }
            tokio::select! {
                biased;
                _ = stop.wait_for(|value| *value) => return,
                outcome = tokio::time::timeout(Duration::from_secs(4), rework.tick(&state)) => {
                    if let Ok(Ok(progress)) = outcome {
                        tracing::debug!(inspected=progress.inspected, recorded=progress.closed, refused=progress.refused, "rework replacement-facts sweep");
                    }
>>>>>>> refs/remotes/central/main
                }
                SweepOutcome::Incomplete => {}
            }
            match run_sweep(&mut stop, closures.tick(&state)).await {
                SweepOutcome::Stopped => return,
                SweepOutcome::Complete(progress) => {
                    tracing::debug!(
                        inspected = progress.inspected,
                        settled = progress.closed,
                        refused = progress.refused,
                        "launch closure settlement sweep"
                    );
                }
                SweepOutcome::Incomplete => {}
            }
        }
        let incomplete = match run_sweep(&mut stop, sweep.tick(&state)).await {
            SweepOutcome::Stopped => return,
            SweepOutcome::Complete(progress) => {
                tracing::debug!(
                    inspected = progress.inspected,
                    closed = progress.closed,
                    refused = progress.refused,
                    "native ungranted expiry sweep"
                );
                progress.refused > 0
            }
            SweepOutcome::Incomplete => true,
        };
        if incomplete && tokio::time::Instant::now() >= warn_after {
            tracing::warn!("native expiry sweep incomplete; unclosed attempts remain held");
            warn_after = tokio::time::Instant::now() + Duration::from_secs(30);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_precedes_ready_work_and_sender_loss_also_stops() {
        let (sender, mut stop) = watch::channel(true);
        assert!(matches!(
            run_sweep(&mut stop, async { panic!("shutdown must win") }).await,
            SweepOutcome::Stopped
        ));
        sender.send(false).unwrap();
        drop(sender);
        assert!(matches!(
            run_sweep(&mut stop, async { panic!("closed channel must stop") }).await,
            SweepOutcome::Stopped
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_and_shutdown_drop_in_flight_work() {
        for shutdown in [false, true] {
            let (sender, mut stop) = watch::channel(false);
            let (dropped, observed) = tokio::sync::oneshot::channel::<()>();
            let start = tokio::time::Instant::now();
            let outcome = run_sweep(&mut stop, async move {
                let _guard = dropped;
                if shutdown {
                    sender.send(true).unwrap();
                }
                std::future::pending().await
            })
            .await;
            assert!(observed.await.is_err());
            if shutdown {
                assert!(matches!(outcome, SweepOutcome::Stopped));
                assert_eq!(start.elapsed(), Duration::ZERO);
            } else {
                assert!(matches!(outcome, SweepOutcome::Incomplete));
                assert_eq!(start.elapsed(), Duration::from_secs(4));
            }
        }
    }

    #[tokio::test]
    async fn completed_sweeps_retain_progress_and_refusals_are_incomplete() {
        let (_sender, mut stop) = watch::channel(false);
        let progress = Progress {
            inspected: 3,
            closed: 1,
            refused: 2,
        };
        assert!(matches!(
            run_sweep(&mut stop, async { Ok(progress) }).await,
            SweepOutcome::Complete(Progress {
                inspected: 3,
                closed: 1,
                refused: 2
            })
        ));
        assert!(matches!(
            run_sweep(&mut stop, async { Err(AppError::Internal) }).await,
            SweepOutcome::Incomplete
        ));
    }
}
