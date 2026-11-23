use super::{AppState, Arc, Loaded, Service, Step, refused};
use crate::playbooks::{
    host_adapters as host, internal_judgments::InternalJudgments, review_domain::CatalogProfile,
};
use aq_playbook_domain::namespace_store::startup::BridgeSchema;

pub(super) async fn open(state: &AppState, config: &Loaded) -> anyhow::Result<Service> {
    let requires_reviews = || {
        config.deployments.iter().any(|d| {
            d.artifact
                .content
                .playbook
                .required_capabilities
                .contains("decision.request")
        })
    };
    BridgeSchema {
        format: crate::STATE_FORMAT_VERSION,
        database: config.connection.get_dbname(),
        reviews_available: state.source_retention.is_some() || !requires_reviews(),
    }
    .verify(&state.store.pool)
    .await?;
    config.check_socket().step("checking the engine socket")?;
    let service = service(state, config).await?;
    let settings = &state.config.server;
    let plugins = aq_playbook_host::plugins::Plugins::discover(
        &settings.plugins_dir,
        &settings.state_root,
        &settings.vault_dir,
    );
    let mut service = service.with_plugins(plugins);
    service.installation_routing = config.installation_routing;
    initialize(state, config, service).await
}
async fn service(state: &AppState, config: &Loaded) -> anyhow::Result<Service> {
    let service = if config.profile == CatalogProfile::Monitoring6 {
        let graph = graph(state)?;
        Service::new_monitoring6(state, config.namespace.clone(), config.keys.clone(), graph).await
    } else if config.profile.is_host2() {
        let graph = graph(state)?;
        let integration = integration(state, config)?;
        Service::new_host2(
            state,
            config.namespace.clone(),
            config.keys.clone(),
            config.profile,
            graph,
            None,
            Some(Arc::new(InternalJudgments::new(
                state.internal_work.clone(),
                aq_internal_work::Purpose::PlaybookCall,
            ))),
            integration,
        )
        .await
    } else {
        Service::with_profile(
            state,
            config.namespace.clone(),
            config.keys.clone(),
            config.profile,
        )
        .await
    }
    .step("opening the bridge service")?;
    Ok(service)
}
fn graph(state: &AppState) -> anyhow::Result<Arc<host::GatewayGraph>> {
    let graph = state
        .agent_access
        .clone()
        .ok_or_else(|| refused("no graph access"))?;
    Ok(Arc::new(host::GatewayGraph(graph)))
}
fn integration(
    state: &AppState,
    config: &Loaded,
) -> anyhow::Result<Option<Arc<dyn host::Integration>>> {
    // No production sandbox check executor (server->node check-attempt
    // execution with profiled check evidence) exists yet, so
    // check.request refuses `unavailable`. judgment.request is served
    // by internal work (spec 0012 §6) with purpose "playbook call".
    //
    // The D7 integration port binds to D1 delivery only when an
    // integration policy is configured and aq-git (source retention)
    // is available; otherwise candidate.* refuses `unavailable`.
    match (&config.integration, state.delivery()) {
        (Some(settings), Some(delivery)) => Ok(Some(Arc::new(host::DeliveryIntegration::new(
            state, delivery, settings, config,
        )))),
        (Some(_), None) => Err(refused("integration needs source retention")),
        (None, _) => Ok(None),
    }
}

async fn initialize(
    state: &AppState,
    config: &Loaded,
    service: Service,
) -> anyhow::Result<Service> {
    let initialized = service.initialize(config.namespace.owner_user_id).await;
    if initialized.is_err() {
        // A stored namespace of another profile or generation: say which,
        // and what the operator does (there is no move between profiles).
        let stored = sqlx::query_as("SELECT catalog_profile,generation FROM playbook_namespaces WHERE namespace_id=$1 OR name=$2 LIMIT 1")
                .bind(config.namespace.namespace_id)
                .bind(&config.namespace.name)
                .fetch_optional(&state.store.pool)
                .await
                .step("reading the stored namespace")?;
        let generation = config.namespace.generation;
        if let Some(why) = config.profile.namespace_refusal(generation, stored) {
            return Err(refused(&why));
        }
    }
    initialized.step("initializing the namespace")?;
    Ok(service)
}
