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
            Some(result)=work.join_next(),if !work.is_empty()=>{
                // Never print transport errors or secret URLs. A fenced old
                // runtime exits; storage errors leave durable state for recovery.
                if matches!(result,Ok(Err(super::Error::DispatcherFenced))){break;}
                if !matches!(result,Ok(Ok(_))) && last_warning.elapsed()>=Duration::from_secs(30){last_warning=std::time::Instant::now();tracing::warn!("notification dispatch deferred; inspect retained delivery state");}
            }
        }
    }
    work.abort_all();
    while work.join_next().await.is_some() {}
    Ok(())
}
