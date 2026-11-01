//! Explicit optional owner startup; uncertainty never starts a replacement.
use super::{config::Loaded, driver::Driver, service::Service};
use crate::{AppState, reviews};
use aq_playbook_host::{Limits, protected};
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
fn refused() -> anyhow::Error {
    anyhow::anyhow!("configured playbook owner startup refused")
}
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
        tokio::time::timeout(Duration::from_secs(30), Self::start_inner(state, config))
            .await
            .map_err(|_| refused())?
    }
    async fn start_inner(state: &AppState, config: Arc<Loaded>) -> anyhow::Result<Self> {
        let version: Option<String> =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_optional(&state.store.pool)
                .await
                .map_err(|_| refused())?;
        if crate::STATE_FORMAT_VERSION < 26
            || version.as_deref() != Some(&crate::STATE_FORMAT_VERSION.to_string())
        {
            return Err(refused());
        }
        let migrations: Vec<i32> =
            sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version")
                .fetch_all(&state.store.pool)
                .await
                .map_err(|_| refused())?;
        if migrations != (1..=crate::STATE_FORMAT_VERSION as i32).collect::<Vec<_>>() {
            return Err(refused());
        }
        for table in [
            "review_owner_budget",
            "review_lineage_budget",
            "review_subjects",
            "human_decision_requests",
            "human_decision_finality",
            "review_owner_outbox",
            "playbook_namespaces",
            "playbook_effects",
            "playbook_outbox",
        ] {
            let present: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(format!("public.{table}"))
                .fetch_one(&state.store.pool)
                .await
                .map_err(|_| refused())?;
            if !present {
                return Err(refused());
            }
        }
        let installed: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version=26)")
                .fetch_one(&state.store.pool)
                .await
                .map_err(|_| refused())?;
        let column:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='public' AND table_name='playbook_namespaces' AND column_name='catalog_profile')").fetch_one(&state.store.pool).await.map_err(|_|refused())?;
        if !installed
            || !column
            || config.connection.get_dbname() != state.store.pool.connect_options().get_database()
        {
            return Err(refused());
        }
        let budget: i64 =
            sqlx::query_scalar("SELECT count(*) FROM review_owner_budget WHERE singleton")
                .fetch_one(&state.store.pool)
                .await
                .map_err(|_| refused())?;
        if budget != 1
            || (state.source_retention.is_none()
                && config.deployments.iter().any(|d| {
                    d.artifact
                        .content
                        .playbook
                        .required_capabilities
                        .contains("decision.request")
                }))
        {
            return Err(refused());
        }
        config.check_socket().map_err(|_| refused())?;
        let service = Service::new_profile2(state, config.namespace.clone(), config.keys.clone())
            .await
            .map_err(|_| refused())?;
        service
            .initialize(config.namespace.owner_user_id)
            .await
            .map_err(|_| refused())?;
        let factory_config = config.clone();
        let limits = Limits {
            work_queue: 8,
            control_queue: 4,
            engine: aq_playbook_host::runner::EngineConfig {
                max_runs: 128,
                max_events: 2048,
                max_queued_destinations: 256,
                max_receipts: 4096,
                max_state_bytes: 8_388_608,
            },
            ..Limits::default()
        };
        let started = Driver::start(service.clone(), limits, move || {
            factory_config
                .check_socket()
                .map_err(|_| aq_playbook_host::Error::Invalid)?;
            let client = factory_config
                .connection
                .connect(aq_playbook_host::postgres_client::NoTls)
                .map_err(|_| {
                    aq_playbook_host::Error::Engine("local database connection failed".into())
                })?;
            protected::storage(
                client,
                factory_config.engine_keys.clone(),
                Duration::from_secs(2),
                Duration::from_millis(500),
            )
        });
        let driver = Arc::new(
            tokio::time::timeout(Duration::from_secs(30), started)
                .await
                .map_err(|_| refused())?
                .map_err(|_| refused())?,
        );
        let bootstrap = async {
            for deployment in &config.deployments {
                service
                    .grant(
                        config.namespace.owner_user_id,
                        deployment.grant_id,
                        deployment.scope.clone(),
                        deployment.expires_at_ms,
                    )
                    .await
                    .map_err(|_| refused())?;
                driver
                    .install(deployment.artifact.clone())
                    .await
                    .map_err(|_| refused())?;
                driver
                    .ensure_binding(
                        deployment.binding.clone(),
                        deployment.artifact.digest.clone(),
                        deployment.grant_id,
                        &deployment.scope,
                    )
                    .await
                    .map_err(|_| refused())?;
            }
            // The frozen startup-metadata profile represents 1..4 configured
            // artifacts. An explicit empty deployment list mounts no historical
            // snapshot; current catalog inspection remains independently owned.
            // In particular, [] never clears already installed owner state.
            if !config.deployments.is_empty() {
                driver
                    .record_loaded_definitions(
                        config
                            .deployments
                            .iter()
                            .map(|deployment| &deployment.artifact),
                    )
                    .map_err(|_| refused())?;
            }
            driver.recover().await.map_err(|_| refused())?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = bootstrap {
            let _ = driver.shutdown().await;
            return Err(error);
        }
        let reviews = Arc::new(reviews::Service::new(state.clone(), config.keys.clone()));
        let (stop, mut receiver) = watch::channel(false);
        driver.set_automatic_state(1);
        let owned = driver.clone();
        let task = tokio::spawn(async move {
            let mut timer = tokio::time::interval(config.interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {biased; _=receiver.wait_for(|stop|*stop)=>break,_=timer.tick()=>{}}
                match owned.quantum().await {
                    Ok(_) | Err(super::domain::Error::Overloaded) => {}
                    Err(_) => {
                        owned.set_automatic_state(2);
                        tracing::warn!(
                            "playbook drive held after owner uncertainty or refusal; explicit reconciliation required"
                        );
                        let _ = receiver.wait_for(|stop| *stop).await;
                        break;
                    }
                }
            }
        });
        let mut stopping = stop.subscribe();
        let draining = driver.clone();
        let drain = tokio::spawn(async move {
            let _ = stopping.wait_for(|stop| *stop).await;
            draining.set_automatic_state(3);
            let result = draining.shutdown().await;
            draining.set_automatic_state(if result.is_ok() { 4 } else { 2 });
            result
        });
        Ok(Self {
            driver,
            reviews,
            stop,
            task: Some(task),
            drain: Some(drain),
            forward: None,
        })
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
        let mut drain = self.drain.take().ok_or_else(refused)?;
        let result = tokio::time::timeout(Duration::from_secs(10), &mut drain).await;
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
