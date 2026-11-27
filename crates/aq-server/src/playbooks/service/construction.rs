use super::*;
use crate::playbooks::host_adapters as host;

impl Service {
    pub async fn new(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
    ) -> Result<Self> {
        Self::with_profile(state, namespace, keys, CatalogProfile::Mail1).await
    }
    /// Explicit new catalog for a new namespace/generation. Existing metadata
    /// is never restamped; a profile/catalog mismatch refuses adoption.
    pub async fn new_profile2(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
    ) -> Result<Self> {
        Self::with_profile(state, namespace, keys, CatalogProfile::HumanReview2).await
    }
    /// Explicit D4 host catalog (profile 5) for a new namespace/generation.
    /// The graph port is the gateway in production and a fake in tests.
    pub async fn new_host5(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn host::WorkGraph>,
    ) -> Result<Self> {
        let mut service = Self::with_profile(state, namespace, keys, CatalogProfile::Host5).await?;
        service.host = Some(Arc::new(default_host_ports(state, graph)));
        Ok(service)
    }
    /// D6 host catalog 2.0.0 (profile 6) for a new namespace/generation. The
    /// sandbox check executor and judgment provider are injected ports; an
    /// unbound port refuses with a typed outcome, never a fabricated result.
    pub async fn new_host7(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn host::WorkGraph>,
        checks: Option<Arc<dyn host::CheckExecutor>>,
        judgments: Option<Arc<dyn host::JudgmentProvider>>,
    ) -> Result<Self> {
        Self::new_host7_integration(state, namespace, keys, graph, checks, judgments, None).await
    }
    /// Profile 7 with the D7 integration port (D1 delivery service); `None`
    /// refuses candidate.prepare / candidate.publish `unavailable`.
    pub async fn new_host7_integration(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn host::WorkGraph>,
        checks: Option<Arc<dyn host::CheckExecutor>>,
        judgments: Option<Arc<dyn host::JudgmentProvider>>,
        integration: Option<Arc<dyn host::Integration>>,
    ) -> Result<Self> {
        Self::new_host2(
            state,
            namespace,
            keys,
            CatalogProfile::Host7,
            graph,
            checks,
            judgments,
            integration,
        )
        .await
    }
    /// Host catalog 2.1.0 (profile 8) with the D7 integration port: the Host7
    /// adapters, with candidate.prepare emitting the prepared source OID.
    pub async fn new_host8_integration(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn host::WorkGraph>,
        checks: Option<Arc<dyn host::CheckExecutor>>,
        judgments: Option<Arc<dyn host::JudgmentProvider>>,
        integration: Option<Arc<dyn host::Integration>>,
    ) -> Result<Self> {
        Self::new_host2(
            state,
            namespace,
            keys,
            CatalogProfile::Host8,
            graph,
            checks,
            judgments,
            integration,
        )
        .await
    }
    /// A catalog 2.x host profile (Host7 or Host8) with its ports.
    #[allow(clippy::too_many_arguments)]
    pub async fn new_host2(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        profile: CatalogProfile,
        graph: Arc<dyn host::WorkGraph>,
        checks: Option<Arc<dyn host::CheckExecutor>>,
        judgments: Option<Arc<dyn host::JudgmentProvider>>,
        integration: Option<Arc<dyn host::Integration>>,
    ) -> Result<Self> {
        if !profile.is_host2() {
            return Err(Error::Invalid);
        }
        let mut service = Self::with_profile(state, namespace, keys, profile).await?;
        service.host = Some(Arc::new(host::HostPorts {
            checks,
            judgments,
            integration,
            ..default_host_ports(state, graph)
        }));
        Ok(service)
    }
    /// Replaces the routing port of a freshly built host service (tests).
    pub fn with_routing(mut self, routing: Arc<dyn host::RoutingPort>) -> Self {
        if let Some(ports) = self.host.as_mut().and_then(Arc::get_mut) {
            ports.routing = routing;
        }
        self
    }
    pub fn with_plugins(mut self, plugins: Plugins) -> Self {
        let (plugins, registry) = plugins.overlay(self.profile.registry());
        self.registry = Arc::new(registry);
        self.plugins = Arc::new(plugins);
        self
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    pub(in crate::playbooks) fn known_contract(&self, contract: &CommandContract) -> bool {
        aq_playbook_host::accepts_command(self.registry(), contract)
            || extension_command(&contract.name, contract)
    }
    /// Replaces the delivery port of a freshly built host service (tests).
    pub fn with_delivery_ops(mut self, delivery_ops: Arc<dyn host::DeliveryOps>) -> Self {
        if let Some(ports) = self.host.as_mut().and_then(Arc::get_mut) {
            ports.delivery_ops = delivery_ops;
        }
        self
    }
    /// Replaces the environment port of a freshly built host service (tests).
    #[cfg(feature = "project_environments")]
    pub fn with_environment_ops(
        mut self,
        ops: Arc<dyn aq_environments::commands::EnvironmentOps>,
    ) -> Self {
        if let Some(ports) = self.host.as_mut().and_then(Arc::get_mut) {
            ports.environment_ops = ops;
        }
        self
    }
    /// D8 host services with the existing D4 contracts and confirmed domain events.
    pub async fn new_monitoring6(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn host::WorkGraph>,
    ) -> Result<Self> {
        let mut service =
            Self::with_profile(state, namespace, keys, CatalogProfile::Monitoring6).await?;
        service.host = Some(Arc::new(default_host_ports(state, graph)));
        Ok(service)
    }
    /// Profile 2 plus delivery of confirmed graph operations as typed events.
    pub async fn new_profile3(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
    ) -> Result<Self> {
        Self::with_profile(state, namespace, keys, CatalogProfile::GraphEvents3).await
    }
    pub async fn new_profile4(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
    ) -> Result<Self> {
        Self::with_profile(state, namespace, keys, CatalogProfile::SettlementEvents4).await
    }
    pub(crate) async fn with_profile(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        profile: CatalogProfile,
    ) -> Result<Self> {
        namespace.validate()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(500))
            .connect_with(
                state
                    .store
                    .pool
                    .connect_options()
                    .as_ref()
                    .clone()
                    .application_name("aq-playbook-bridge"),
            )
            .await?;
        let reviews = if profile.has_reviews() {
            Some(Arc::new(crate::reviews::Service::new(
                state.clone(),
                keys.clone(),
            )))
        } else {
            None
        };
        Ok(Self {
            namespace,
            pool,
            keys,
            clock: state.clock.clone(),
            profile,
            registry: Arc::new(profile.registry()),
            plugins: Arc::default(),
            notifications: crate::notifications::api::service(state),
            reviews,
            host: None,
            installation_routing: false,
            delivery: Arc::new(aq_playbook_domain::delivery_store::PgDelivery(
                state.store.pool.clone(),
            )),
            settlement: Arc::new(host::ServerSettlement(state.clone())),
        })
    }
    /// Replaces the delivery port of a freshly built host service (tests).
    pub fn with_delivery(
        mut self,
        port: Arc<dyn aq_playbook_domain::delivery_store::DeliveryPort>,
    ) -> Self {
        self.delivery = port;
        self
    }
    /// Replaces the settlement port of a freshly built host service (tests).
    pub fn with_settlement(mut self, port: Arc<dyn host::DeliverySettlement>) -> Self {
        self.settlement = port;
        self
    }
}

fn default_host_ports(state: &crate::AppState, graph: Arc<dyn host::WorkGraph>) -> host::HostPorts {
    host::HostPorts {
        state: state.clone(),
        graph,
        checks: None,
        judgments: None,
        integration: None,
        routing: Arc::new(host::ServerRouting(state.clone())),
        delivery_ops: Arc::new(host::ServerDelivery::new(state.clone())),
        #[cfg(feature = "project_environments")]
        environment_ops: Arc::new(host::ServerEnvironments(state.clone())),
    }
}
