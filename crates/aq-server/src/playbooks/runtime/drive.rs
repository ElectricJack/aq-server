use super::{AppState, Arc, Driver, Loaded, Runtime, reviews, watch};
impl Runtime {
    pub(super) fn drive(state: &AppState, config: &Loaded, driver: Arc<Driver>) -> Self {
        let reviews = Arc::new(reviews::Service::new(state.clone(), config.keys.clone()));
        state.internal_work.authoring().set(driver.clone());
        tokio::spawn(crate::routing::reconcile(state.clone()));
        let (stop, receiver) = watch::channel(false);
        driver.set_automatic_state(1);
        let owned = driver.clone();
        // A failed quantum is retried after a backoff from the drive interval
        // up to the engine statement timeout; only shutdown stops the drive.
        let quantum = move || {
            let owned = owned.clone();
            async move {
                let result = match owned.quantum().await {
                    Ok(_) | Err(crate::playbooks::domain::Error::Overloaded) => Ok(()),
                    Err(error) => Err(error),
                };
                owned.drive_failed(result.is_err());
                result
            }
        };
        let task = tokio::spawn(aq_playbook_host::drive::run(
            receiver,
            aq_playbook_host::drive::Backoff::new(config.interval, config.timeouts.statement()),
            quantum,
            |error, delay| tracing::warn!(?error, ?delay, "playbook quantum failed; retrying"),
        ));
        let mut stopping = stop.subscribe();
        let draining = driver.clone();
        let drain = tokio::spawn(async move {
            let _ = stopping.wait_for(|stop| *stop).await;
            draining.set_automatic_state(3);
            let result = draining.shutdown().await;
            draining.set_automatic_state(if result.is_ok() { 4 } else { 2 });
            result
        });
        Self {
            driver,
            reviews,
            stop,
            task: Some(task),
            drain: Some(drain),
            forward: None,
        }
    }
}
