//! Bounded background loop on the dedicated notification pool.
use super::{Result, api::service};
use crate::AppState;
use std::time::Duration;
use uuid::Uuid;
/// The shutdown branch cancels outstanding HTTP futures. Their committed
/// dispatch records remain uncertain and recover only after lease expiry.
pub async fn run(
    state: AppState,
    id: Uuid,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let service = service(&state);
    let client = std::sync::Arc::new(aq_notifications::discord::Discord::new()?);
    let mut work = tokio::task::JoinSet::new();
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_warning = std::time::Instant::now() - Duration::from_secs(30);
    // Bounded producer observations and summaries never hold dispatch capacity.
    let policy = super::policy::Policy::new(service.clone());
    let mut flush = tokio::time::interval(Duration::from_secs(5));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut flushing: Option<tokio::task::JoinHandle<bool>> = None;
    // SBX25 central storage sample: root filesystem and database size.
    let mut central = tokio::time::interval(Duration::from_secs(60));
    central.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sampling: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            changed=shutdown.changed()=>{if changed.is_err() || *shutdown.borrow(){break;}}
            _=interval.tick()=>{
                let maintenance=crate::updates::spool_dir(&state).is_some_and(|p|crate::updates::marker_present(&p.join("status/maintenance")));
                if !maintenance && work.len()<4 {
                    let service=service.clone();let client=client.clone();
                    work.spawn(async move {
                        // Bound the entire SQL/HTTP/receipt operation as well as
                        // each individual database statement and HTTP request.
                        // Cancellation after commit leaves an uncertain attempt.
                        tokio::time::timeout(Duration::from_secs(15),service.tick(id,&client))
                            .await.unwrap_or(Err(super::Error::Unavailable))
                    });
                }
            }
            _=central.tick()=>{
                if sampling.as_ref().is_none_or(|h|h.is_finished()) {
                    let pool=service.pool.clone();
                    let now=u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
                    sampling=Some(tokio::spawn(async move {
                        let root=std::path::Path::new(crate::node_storage::CENTRAL_ROOT);
                        let sampled=tokio::time::timeout(Duration::from_secs(15),crate::node_storage::sample_central(&pool,root,now)).await;
                        if !matches!(sampled,Ok(Ok(_))) {
                            tracing::warn!("central storage sample deferred");
                        }
                    }));
                }
            }
            _=flush.tick()=>{
                if flushing.as_ref().is_none_or(|h|h.is_finished()) {
                    if let Some(handle)=flushing.take()
                        && !matches!(handle.await,Ok(true))
                        && last_warning.elapsed()>=Duration::from_secs(30)
                    {
                        last_warning=std::time::Instant::now();
                        tracing::warn!("notification policy deferred; retained sources will retry");
                    }
                    let policy=policy.clone();
                    let state=state.clone();
                    flushing=Some(tokio::spawn(async move {
                        tokio::time::timeout(Duration::from_secs(15),async {
                            // Independent sources cannot block one another or
                            // occupy execution/dispatch slots. Retry next tick.
                            let mut ok=true;
                            ok &= policy.retry_pending().await.is_ok();
                            ok &= policy.reconcile_reviews().await.is_ok();
                            let groups=serde_json::to_value(&state.config.projects.worker_groups).unwrap_or_default();
                            ok &= policy.reconcile_workers(state.config.workers.offline_after_secs,groups).await.is_ok();
                            ok &= policy.reconcile_tasks().await.is_ok();
                            let groups=serde_json::to_value(&state.config.projects.worker_groups).unwrap_or_default();
                            ok &= policy.reconcile_storage(groups).await.is_ok();
                            ok &= policy.flush().await.is_ok();
                            ok &= policy.garbage_collect().await.is_ok();
                            ok &= policy.reconcile_graph(&state).await.is_ok();
                            ok
                        }).await.unwrap_or(false)
                    }));
                }
            }
            Some(result)=work.join_next(),if !work.is_empty()=>{
                // Never print transport errors or secret URLs. A fenced old
                // runtime exits; storage errors leave durable state for recovery.
                if matches!(result,Ok(Err(super::Error::DispatcherFenced))){break;}
                if !matches!(result,Ok(Ok(_))) && last_warning.elapsed()>=Duration::from_secs(30){last_warning=std::time::Instant::now();tracing::warn!("notification dispatch deferred; inspect retained delivery state");}
            }
        }
    }
    if let Some(handle) = sampling {
        handle.abort();
    }
    if let Some(handle) = flushing {
        handle.abort();
    }
    work.abort_all();
    while work.join_next().await.is_some() {}
    Ok(())
}
