use super::{Arc, Driver, Limits, Loaded, Service, Step, protected, refused};

pub(super) async fn open(service: &Service, config: Arc<Loaded>) -> anyhow::Result<Arc<Driver>> {
    let factory_config = config.clone();
    let defaults = Limits::default();
    let limits = Limits {
        work_queue: 8,
        control_queue: 4,
        request_bytes: config.request_bytes.unwrap_or(defaults.request_bytes),
        timeouts: config.timeouts,
        retention: config.retention,
        engine_retention: config.engine_retention,
        ..defaults
    };
    let started = Driver::open(service.clone(), limits, move || {
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
            factory_config.timeouts.statement(),
            factory_config.timeouts.lock(),
        )
    });
    Ok(Arc::new(
        tokio::time::timeout(config.timeouts.open(), started)
            .await
            .step("opening the engine (open_ms)")?
            .map_err(|(_, why)| refused(&format!("opening the engine: {why}")))?,
    ))
}

pub(super) async fn bootstrap(
    driver: &Driver,
    service: &Service,
    config: &Loaded,
) -> anyhow::Result<()> {
    for deployment in &config.deployments {
        if !aq_playbook_host::plugins_match(
            service.registry(),
            &deployment.artifact.content.registry,
        ) {
            tracing::warn!(binding = %deployment.binding, "plugin contract mismatch; playbook deployment refused");
            continue;
        }
        service
            .grant(
                config.namespace.owner_user_id,
                deployment.grant_id,
                deployment.scope.clone(),
                deployment.expires_at_ms,
            )
            .await
            .step("writing a deployment grant")?;
        driver
            .install(deployment.artifact.clone())
            .await
            .step("installing a deployment artifact")?;
        driver
            .ensure_binding(
                deployment.binding.clone(),
                deployment.artifact.digest.clone(),
                deployment.grant_id,
                &deployment.scope,
                &deployment.schedules,
            )
            .await
            .step("activating a deployment binding")?;
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
            .step("recording the loaded definitions")?;
    }
    driver.recover().await.step("recovering the engine")?;
    Ok(())
}
