//! Bounded background loop on the dedicated notification pool.
use super::{Error, Result, api::service, persistence::Service, policy::Policy};
use crate::AppState;
use aq_notifications::discord::Discord;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    task::{JoinHandle, JoinSet},
    time::Interval,
};
use uuid::Uuid;

const TIMEOUT: Duration = Duration::from_secs(15);
const WARNING_INTERVAL: Duration = Duration::from_secs(30);

fn interval(period: Duration) -> Interval {
    let mut timer = tokio::time::interval(period);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    timer
}

struct Warnings(Instant);
impl Warnings {
    fn deferred(&mut self, message: &'static str) {
        if self.0.elapsed() >= WARNING_INTERVAL {
            self.0 = Instant::now();
            tracing::warn!("{message}");
        }
    }
}

struct Dispatch {
    work: JoinSet<Result<bool>>,
    service: Service,
    client: Arc<Discord>,
    id: Uuid,
}
impl Dispatch {
    fn tick(&mut self, state: &AppState) {
        let maintenance = crate::updates::spool_dir(state)
            .is_some_and(|p| crate::updates::marker_present(&p.join("status/maintenance")));
        if maintenance || self.work.len() >= 4 {
            return;
        }
        let service = self.service.clone();
        let client = self.client.clone();
        let id = self.id;
        self.work.spawn(async move {
            // Cancellation after a commit leaves an uncertain attempt.
            tokio::time::timeout(TIMEOUT, service.tick(id, &client))
                .await
                .unwrap_or(Err(Error::Unavailable))
        });
    }
}

fn sample(state: &AppState, service: &Service, sampling: &mut Option<JoinHandle<()>>) {
    if sampling.as_ref().is_some_and(|h| !h.is_finished()) {
        return;
    }
    let pool = service.pool.clone();
    let now = u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
    *sampling = Some(tokio::spawn(async move {
        let root = std::path::Path::new(crate::node_storage::CENTRAL_ROOT);
        let sampled = tokio::time::timeout(
            TIMEOUT,
            crate::node_storage::sample_central(&pool, root, now),
        )
        .await;
        if !matches!(sampled, Ok(Ok(_))) {
            tracing::warn!("central storage sample deferred");
        }
    }));
}

async fn reconcile(policy: &Policy, state: &AppState) -> bool {
    // Each independent source is attempted even after another one fails.
    let mut ok = true;
    ok &= policy.retry_pending().await.is_ok();
    ok &= policy.reconcile_reviews().await.is_ok();
    let groups = serde_json::to_value(&state.config.projects.worker_groups).unwrap_or_default();
    ok &= policy
        .reconcile_workers(state.config.workers.offline_after_secs, groups)
        .await
        .is_ok();
    ok &= policy.reconcile_tasks().await.is_ok();
    let groups = serde_json::to_value(&state.config.projects.worker_groups).unwrap_or_default();
    ok &= policy.reconcile_storage(groups).await.is_ok();
    ok &= policy.flush().await.is_ok();
    ok &= policy.garbage_collect().await.is_ok();
    ok &= policy.reconcile_graph(state).await.is_ok();
    ok
}

struct Maintenance {
    policy: Policy,
    flushing: Option<JoinHandle<bool>>,
}
impl Maintenance {
    async fn tick(&mut self, state: &AppState, warnings: &mut Warnings) {
        if self.flushing.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        if let Some(handle) = self.flushing.take()
            && !matches!(handle.await, Ok(true))
        {
            warnings.deferred("notification policy deferred; retained sources will retry");
        }
        let policy = self.policy.clone();
        let state = state.clone();
        self.flushing = Some(tokio::spawn(async move {
            tokio::time::timeout(TIMEOUT, reconcile(&policy, &state))
                .await
                .unwrap_or(false)
        }));
    }
}

fn fenced(
    result: std::result::Result<Result<bool>, tokio::task::JoinError>,
    warnings: &mut Warnings,
) -> bool {
    // Never print transport errors or secret URLs.
    match result {
        Ok(Err(Error::DispatcherFenced)) => true,
        Ok(Ok(_)) => false,
        _ => {
            warnings.deferred("notification dispatch deferred; inspect retained delivery state");
            false
        }
    }
}

async fn shutdown_requested(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

/// The shutdown branch cancels outstanding HTTP futures. Their committed
/// dispatch records remain uncertain and recover only after lease expiry.
pub async fn run(
    state: AppState,
    id: Uuid,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let service = service(&state);
    let mut dispatch = Dispatch {
        work: JoinSet::new(),
        service: service.clone(),
        client: Arc::new(Discord::new()?),
        id,
    };
    let mut dispatch_tick = interval(Duration::from_millis(250));
    let mut warnings = Warnings(Instant::now() - WARNING_INTERVAL);
    let mut maintenance = Maintenance {
        policy: Policy::new(service.clone()),
        flushing: None,
    };
    let mut flush = interval(Duration::from_secs(5));
    let mut central = interval(Duration::from_secs(60));
    let mut sampling = None;
    while !*shutdown.borrow() {
        tokio::select! {
            _=shutdown_requested(&mut shutdown)=>break,
            _=dispatch_tick.tick()=>dispatch.tick(&state),
            _=central.tick()=>sample(&state,&service,&mut sampling),
            _=flush.tick()=>maintenance.tick(&state,&mut warnings).await,
            Some(result)=dispatch.work.join_next(),if !dispatch.work.is_empty()=>{
                if fenced(result, &mut warnings) { break; }
            }
        }
    }
    if let Some(handle) = sampling {
        handle.abort();
    }
    if let Some(handle) = maintenance.flushing {
        handle.abort();
    }
    dispatch.work.abort_all();
    while dispatch.work.join_next().await.is_some() {}
    Ok(())
}
