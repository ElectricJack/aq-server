//! Optional runtime built after AppState; AppState never owns this runtime.
use super::{AppState, Error, Owner, Result, config, sql};
use std::{
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinHandle,
};
pub struct Runtime {
    pub(super) owner: Arc<Owner>,
    pub(super) uploads_enabled: bool,
    pub(super) metadata: Arc<Semaphore>,
    pub(super) content: Arc<Semaphore>,
    pub(super) uploads: Arc<Semaphore>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<Result<()>>>>,
}
impl Runtime {
    pub async fn start(state: AppState, settings: &config::MediaConfig) -> Result<Arc<Self>> {
        settings.validate().map_err(|_| Error::Invalid)?;
        let owner = Arc::new(Owner::open(state, &settings.root, settings.storage_id).await?);
        let (stop, rx) = watch::channel(false);
        let runtime = Arc::new(Self {
            owner,
            uploads_enabled: settings.uploads_enabled,
            metadata: Arc::new(Semaphore::new(2)),
            content: Arc::new(Semaphore::new(2)),
            uploads: Arc::new(Semaphore::new(2)),
            stop,
            task: Mutex::new(None),
        });
        // Failed initial cleanup never produces mounted-success capabilities.
        runtime.cleanup_once().await?;
        let weak = Arc::downgrade(&runtime);
        *runtime.task.lock().map_err(|_| Error::Owner)? =
            Some(tokio::spawn(Self::maintain(weak, rx)));
        Ok(runtime)
    }
    pub fn owner(&self) -> &Arc<Owner> {
        &self.owner
    }
    pub fn read_enabled(&self) -> bool {
        true
    }
    pub fn uploads_enabled(&self) -> bool {
        self.uploads_enabled
    }
    pub async fn cleanup_once(&self) -> Result<usize> {
        let completed = self.owner.cleanup(16).await?;
        let blocked:i64=sqlx::query_scalar("SELECT count(*) FROM result_media_intents WHERE storage_id=$1 AND cleanup_state='blocked'")
            .bind(self.owner.root.id).fetch_one(&self.owner.state.store.pool).await.map_err(sql)?;
        if blocked > 0 {
            return Err(Error::Storage);
        }
        Ok(completed)
    }
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.stop.send(true);
        let task = self.task.lock().map_err(|_| Error::Owner)?.take();
        if let Some(task) = task {
            task.await.map_err(|_| Error::Owner)??;
        }
        Ok(())
    }
    // Weak ownership avoids an Arc cycle between the runtime and its task. A
    // cleanup already running retains the owner until its physical work ends.
    async fn maintain(runtime: Weak<Self>, mut stop: watch::Receiver<bool>) -> Result<()> {
        let period = Duration::from_secs(60);
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            tokio::select! {
                changed=stop.changed()=>{if changed.is_err() || *stop.borrow(){return Ok(());}},
                _=ticks.tick()=>{
                    let Some(runtime)=runtime.upgrade()else{return Ok(())};
                    if let Err(error)=runtime.cleanup_once().await{
                        tracing::warn!(error=%error,"result media maintenance failed; charges retained");
                    }
                }
            }
        }
    }
}
