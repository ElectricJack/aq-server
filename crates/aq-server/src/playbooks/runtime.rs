//! Explicit optional owner startup; uncertainty never starts a replacement.
use super::{config::Loaded, driver::Driver, service::Service};
use crate::{AppState, reviews};
use aq_playbook_host::{Limits, drive::Refused, drive::Step, protected};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
fn refused(why: &str) -> anyhow::Error {
    Refused(why.into()).into()
}
/// The owner as serving sees it: none until its engine has opened.
pub(crate) type Slot = Arc<tokio::sync::Mutex<Option<Runtime>>>;
mod bridge;
mod deferred;
mod drive;
mod engine;
pub(crate) use deferred::DeferredMount;
pub struct Runtime {
    pub driver: Arc<Driver>,
    pub reviews: Arc<reviews::Service>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    drain: Option<JoinHandle<super::domain::Result<()>>>,
    forward: Option<JoinHandle<()>>,
}
impl Runtime {
    /// Configuration is authenticated local operator input, never a HTTP payload.
    pub async fn start(state: &AppState, config: Arc<Loaded>) -> anyhow::Result<Self> {
        let open = config.timeouts.open();
        tokio::time::timeout(open, Self::start_inner(state, config))
            .await
            .step("the open passed engine_timeouts.open_ms")?
    }
    async fn start_inner(state: &AppState, config: Arc<Loaded>) -> anyhow::Result<Self> {
        let service = bridge::open(state, &config).await?;
        let driver = engine::open(&service, config.clone()).await?;
        if let Err(error) = engine::bootstrap(&driver, &service, &config).await {
            let _ = driver.shutdown().await;
            return Err(error);
        }
        Ok(Self::drive(state, &config, driver))
    }
    pub(crate) fn watch_shutdown(&mut self, mut receiver: watch::Receiver<bool>) {
        let stop = self.stop.clone();
        self.forward = Some(tokio::spawn(async move {
            let _ = receiver.wait_for(|stop| *stop).await;
            let _ = stop.send(true);
        }));
    }
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.stop.send(true);
        let mut drain = (self.drain.take()).ok_or_else(|| refused("already shut down"))?;
        let wait = self.driver.deadline.saturating_mul(2);
        let result = tokio::time::timeout(wait, &mut drain).await;
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
        {
            task.abort();
            let _ = task.await;
        }
        match result {
            Ok(Ok(Ok(()))) => Ok(()),
            _ => Err(anyhow::anyhow!(
                "playbook owner shutdown unresolved; no release proof"
            )),
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(task) = self.forward.take() {
            task.abort();
        }
        // A detached drain retains the real owner until it returns; never join
        // or invent a release proof in Drop.
    }
}
