//! AQ-owned transactions. Engine storage remains independent; all mailbox
//! effects and operation events share this transaction and its current grants.
use super::domain::*;
use super::review_domain::CatalogProfile;
use crate::{
    Clock,
    agent_access::messaging,
    protected_storage::{Keyring, Sealed},
};
use aq_playbook_host::{
    model::{CommandContract, ContractRef, EventEnvelope, Registry, canonical_bytes},
    plugins::{Plugins, command_plugin, extension_capability, extension_command},
    runner::{CancellationResult, Intent},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;
const SOURCE: &str = "bd1246e8e972ee1627e94598945f398812c89044106b463151a696511412ca39";
pub use super::domain::Namespace;
/// Process-local current-owner context, not a deserializable fresh HTTP permit.
#[derive(Clone, Debug)]
pub struct RuntimeOwner {
    namespace_id: Uuid,
    generation: i64,
    incarnation: Uuid,
    revision: i64,
}
#[derive(Clone)]
pub struct Service {
    pub namespace: Namespace,
    pub(super) pool: PgPool,
    pub(super) keys: Arc<Keyring>,
    clock: Clock,
    pub(crate) profile: CatalogProfile,
    registry: Arc<Registry>,
    pub(super) plugins: Arc<Plugins>,
    pub(super) notifications: crate::notifications::persistence::Service,
    pub(super) reviews: Option<Arc<crate::reviews::Service>>,
    pub(super) host: Option<Arc<super::host_adapters::HostPorts>>,
    /// R1 (Host9): the routing commands serve any project on the router.
    pub installation_routing: bool,
    /// Epic 0: what the server records of results (`aq.work.result_ready`, `work.read`).
    #[cfg(feature = "delivery_playbook")]
    pub(super) delivery: Arc<dyn aq_playbook_domain::delivery_store::DeliveryPort>,
    /// Epic 0: the server's settlement, for `work.complete` and `work.return`.
    #[cfg(feature = "delivery_playbook")]
    pub(super) settlement: Arc<dyn super::host_adapters::DeliverySettlement>,
}
#[derive(Clone, Debug)]
pub enum MessageResult {
    Queued(Value),
    Cancelled,
}
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
        graph: Arc<dyn super::host_adapters::WorkGraph>,
    ) -> Result<Self> {
        let mut service = Self::with_profile(state, namespace, keys, CatalogProfile::Host5).await?;
        service.host = Some(Arc::new(super::host_adapters::HostPorts {
            state: state.clone(),
            graph,
            checks: None,
            judgments: None,
            integration: None,
            routing: Arc::new(super::host_adapters::ServerRouting(state.clone())),
            #[cfg(feature = "delivery_playbook")]
            delivery_ops: Arc::new(super::host_adapters::ServerDelivery::new(state.clone())),
            #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
            environment_ops: Arc::new(super::host_adapters::ServerEnvironments(state.clone())),
        }));
        Ok(service)
    }
    /// D6 host catalog 2.0.0 (profile 6) for a new namespace/generation. The
    /// sandbox check executor and judgment provider are injected ports; an
    /// unbound port refuses with a typed outcome, never a fabricated result.
    pub async fn new_host7(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn super::host_adapters::WorkGraph>,
        checks: Option<Arc<dyn super::host_adapters::CheckExecutor>>,
        judgments: Option<Arc<dyn super::host_adapters::JudgmentProvider>>,
    ) -> Result<Self> {
        Self::new_host7_integration(state, namespace, keys, graph, checks, judgments, None).await
    }
    /// Profile 7 with the D7 integration port (D1 delivery service); `None`
    /// refuses candidate.prepare / candidate.publish `unavailable`.
    pub async fn new_host7_integration(
        state: &crate::AppState,
        namespace: Namespace,
        keys: Arc<Keyring>,
        graph: Arc<dyn super::host_adapters::WorkGraph>,
        checks: Option<Arc<dyn super::host_adapters::CheckExecutor>>,
        judgments: Option<Arc<dyn super::host_adapters::JudgmentProvider>>,
        integration: Option<Arc<dyn super::host_adapters::Integration>>,
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
        graph: Arc<dyn super::host_adapters::WorkGraph>,
        checks: Option<Arc<dyn super::host_adapters::CheckExecutor>>,
        judgments: Option<Arc<dyn super::host_adapters::JudgmentProvider>>,
        integration: Option<Arc<dyn super::host_adapters::Integration>>,
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
        graph: Arc<dyn super::host_adapters::WorkGraph>,
        checks: Option<Arc<dyn super::host_adapters::CheckExecutor>>,
        judgments: Option<Arc<dyn super::host_adapters::JudgmentProvider>>,
        integration: Option<Arc<dyn super::host_adapters::Integration>>,
    ) -> Result<Self> {
        if !profile.is_host2() {
            return Err(Error::Invalid);
        }
        let mut service = Self::with_profile(state, namespace, keys, profile).await?;
        service.host = Some(Arc::new(super::host_adapters::HostPorts {
            state: state.clone(),
            graph,
            checks,
            judgments,
            integration,
            routing: Arc::new(super::host_adapters::ServerRouting(state.clone())),
            #[cfg(feature = "delivery_playbook")]
            delivery_ops: Arc::new(super::host_adapters::ServerDelivery::new(state.clone())),
            #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
            environment_ops: Arc::new(super::host_adapters::ServerEnvironments(state.clone())),
        }));
        Ok(service)
    }
    /// Replaces the routing port of a freshly built host service (tests).
    pub fn with_routing(mut self, routing: Arc<dyn super::host_adapters::RoutingPort>) -> Self {
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
    fn validate_new_scope(&self, scope: &GrantScope) -> Result<()> {
        self.profile
            .validate_scope_with_extensions(scope, |name| self.plugins.get(name).is_some())
    }
    pub(super) fn grant_authority(
        &self,
        id: Uuid,
        revision: i64,
        scope: &GrantScope,
    ) -> Result<aq_playbook_host::runner::Authority> {
        // Grants in storage were approved against the installed extensions at
        // issuance. Keep their authority identity after plugin removal.
        self.profile
            .authority_with_extensions(id, revision, scope, |_| true)
    }
    pub(super) fn known_contract(&self, contract: &CommandContract) -> bool {
        aq_playbook_host::accepts_command(self.registry(), contract)
            || extension_command(&contract.name, contract)
    }
    /// Replaces the delivery port of a freshly built host service (tests).
    #[cfg(feature = "delivery_playbook")]
    pub fn with_delivery_ops(
        mut self,
        delivery_ops: Arc<dyn super::host_adapters::DeliveryOps>,
    ) -> Self {
        if let Some(ports) = self.host.as_mut().and_then(Arc::get_mut) {
            ports.delivery_ops = delivery_ops;
        }
        self
    }
    /// Replaces the environment port of a freshly built host service (tests).
    #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
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
        graph: Arc<dyn super::host_adapters::WorkGraph>,
    ) -> Result<Self> {
        let mut service =
            Self::with_profile(state, namespace, keys, CatalogProfile::Monitoring6).await?;
        service.host = Some(Arc::new(super::host_adapters::HostPorts {
            state: state.clone(),
            graph,
            checks: None,
            judgments: None,
            integration: None,
            routing: Arc::new(super::host_adapters::ServerRouting(state.clone())),
            #[cfg(feature = "delivery_playbook")]
            delivery_ops: Arc::new(super::host_adapters::ServerDelivery::new(state.clone())),
            #[cfg(all(feature = "delivery_playbook", feature = "project_environments"))]
            environment_ops: Arc::new(super::host_adapters::ServerEnvironments(state.clone())),
        }));
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
            #[cfg(feature = "delivery_playbook")]
            delivery: Arc::new(aq_playbook_domain::delivery_store::PgDelivery(
                state.store.pool.clone(),
            )),
            #[cfg(feature = "delivery_playbook")]
            settlement: Arc::new(super::host_adapters::ServerSettlement(state.clone())),
        })
    }
    /// Replaces the delivery port of a freshly built host service (tests).
    #[cfg(feature = "delivery_playbook")]
    pub fn with_delivery(
        mut self,
        port: Arc<dyn aq_playbook_domain::delivery_store::DeliveryPort>,
    ) -> Self {
        self.delivery = port;
        self
    }
    /// Replaces the settlement port of a freshly built host service (tests).
    #[cfg(feature = "delivery_playbook")]
    pub fn with_settlement(
        mut self,
        port: Arc<dyn super::host_adapters::DeliverySettlement>,
    ) -> Self {
        self.settlement = port;
        self
    }
    pub(super) fn now(&self) -> Result<i64> {
        let at = (self.clock)().timestamp_millis();
        if at < 0 {
            return Err(Error::Invalid);
        }
        Ok(at)
    }
    pub(super) async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('statement_timeout','2000ms',true),set_config('lock_timeout','500ms',true)")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
    pub(super) async fn owner(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        if role.as_deref() != Some("owner") {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) async fn namespace_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<sqlx::postgres::PgRow> {
        let row = sqlx::query(
            "SELECT * FROM playbook_namespaces WHERE namespace_id=$1 AND generation=$2 FOR UPDATE",
        )
        .bind(self.namespace.namespace_id)
        .bind(self.namespace.generation)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Forbidden)?;
        if row.try_get::<i16, _>("catalog_profile").unwrap_or(1) != self.profile.number()
            || row.get::<Uuid, _>("owner_user_id") != self.namespace.owner_user_id
            || row.get::<Uuid, _>("workspace_id") != self.namespace.workspace_id
            || row.get::<Uuid, _>("project_id") != self.namespace.project_id
            || row.get::<String, _>("name") != self.namespace.name
            || row.get::<String, _>("catalog_digest") != digest(&self.profile.registry())?
            || row.get::<String, _>("source_digest") != SOURCE
        {
            return Err(Error::Conflict);
        }
        Ok(row)
    }
    /// Rolling deletion (architecture 0006): one bridge table's finished rows
    /// beyond the newest `keep_finished`, at most `delete_batch` of them.
    pub async fn prune(
        &self,
        owner: &RuntimeOwner,
        retention: &aq_playbook_host::retention::Retention,
        table: usize,
    ) -> Result<u64> {
        let (keep, batch) = retention.bounds().ok_or(Error::Invalid)?;
        let tables = aq_playbook_host::retention::TABLES;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let (namespace, generation) = (self.namespace.namespace_id, self.namespace.generation);
        let deleted = sqlx::query(tables[table % tables.len()].1)
            .bind(namespace)
            .bind(generation)
            .bind(keep)
            .bind(batch)
            .bind(self.now()?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(deleted)
    }
    pub async fn initialize(&self, actor: Uuid) -> Result<()> {
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        aq_playbook_domain::host_operations::Operations(&self.namespace)
            .require_project_workspace(&mut *tx)
            .await?;
        if self.profile == CatalogProfile::Mail1 {
            sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace_id) DO NOTHING").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.namespace.name).bind(self.namespace.workspace_id).bind(self.namespace.project_id).bind(actor).bind(digest(&self.profile.registry())?).bind(SOURCE).execute(&mut *tx).await?;
        } else {
            sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest,catalog_profile) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(namespace_id) DO NOTHING").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.namespace.name).bind(self.namespace.workspace_id).bind(self.namespace.project_id).bind(actor).bind(digest(&self.profile.registry())?).bind(SOURCE).bind(self.profile.number()).execute(&mut *tx).await?;
        }
        self.namespace_locked(&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn runtime_revision(&self) -> Result<i64> {
        let mut tx = self.tx().await?;
        let row = self.namespace_locked(&mut tx).await?;
        let r = row.get("runtime_revision");
        tx.commit().await?;
        Ok(r)
    }
    /// Expected revision is captured before engine ownership acquisition. A delayed
    /// initializer may only replay this exact incarnation/revision, never refresh it.
    pub async fn adopt_runtime(&self, incarnation: Uuid, expected: i64) -> Result<RuntimeOwner> {
        if incarnation.is_nil() || !(0..i64::MAX - 1).contains(&expected) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let row = self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let revision: i64 = row.get("runtime_revision");
        let old: Option<Uuid> = row.get("runtime_incarnation");
        if !(revision == expected || (revision == expected + 1 && old == Some(incarnation))) {
            return Err(Error::Fenced);
        }
        if revision == expected {
            sqlx::query("UPDATE playbook_namespaces SET runtime_revision=$2,runtime_incarnation=$3 WHERE namespace_id=$1").bind(self.namespace.namespace_id).bind(expected+1).bind(incarnation).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(RuntimeOwner {
            namespace_id: self.namespace.namespace_id,
            generation: self.namespace.generation,
            incarnation,
            revision: expected + 1,
        })
    }
    pub(super) fn runtime(&self, row: &sqlx::postgres::PgRow, owner: &RuntimeOwner) -> Result<()> {
        if owner.namespace_id != self.namespace.namespace_id
            || owner.generation != self.namespace.generation
            || row.get::<Option<Uuid>, _>("runtime_incarnation") != Some(owner.incarnation)
            || row.get::<i64, _>("runtime_revision") != owner.revision
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }
    pub async fn grant(
        &self,
        actor: Uuid,
        id: Uuid,
        scope: GrantScope,
        expires: i64,
    ) -> Result<()> {
        self.validate_new_scope(&scope)?;
        if actor != self.namespace.owner_user_id
            || id.is_nil()
            || scope.project_id != self.namespace.project_id
            || scope.workspace_id != self.namespace.workspace_id
        {
            return Err(Error::Forbidden);
        }
        let payload = digest(&(
            self.namespace.namespace_id,
            self.namespace.generation,
            id,
            &scope,
            expires,
            actor,
        ))?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        if let Some(old) = sqlx::query_scalar::<_, String>(
            "SELECT payload_digest FROM playbook_grants WHERE grant_id=$1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if old != payload {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(());
        }
        let now = self.now()?;
        if expires <= now || expires > now.saturating_add(86_400_000) {
            return Err(Error::Invalid);
        }
        sqlx::query("INSERT INTO playbook_grants(grant_id,namespace_id,generation,revision,owner_user_id,scope,expires_at_ms,payload_digest) VALUES($1,$2,$3,1,$4,$5,$6,$7)").bind(id).bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(actor).bind(json!(scope)).bind(expires).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    /// The grant the server holds for a playbook source on the owner's
    /// behalf (spec 0013 ruling R2): written as the namespace owner, and it
    /// never expires. [`Self::grant`] keeps its 24 h cap.
    pub async fn source_grant(&self, id: Uuid, scope: GrantScope) -> Result<()> {
        self.validate_new_scope(&scope)?;
        let (ns, actor) = (&self.namespace, self.namespace.owner_user_id);
        if id.is_nil() || scope.project_id != ns.project_id || scope.workspace_id != ns.workspace_id
        {
            return Err(Error::Forbidden);
        }
        let expires = aq_playbook_authoring::SOURCE_GRANT_EXPIRES_AT_MS;
        let payload = digest(&(ns.namespace_id, ns.generation, id, &scope, expires, actor))?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        sqlx::query("INSERT INTO playbook_grants(grant_id,namespace_id,generation,revision,owner_user_id,scope,expires_at_ms,payload_digest) VALUES($1,$2,$3,1,$4,$5,$6,$7) ON CONFLICT(grant_id) DO NOTHING").bind(id).bind(ns.namespace_id).bind(ns.generation).bind(actor).bind(json!(scope)).bind(expires).bind(payload).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn revoke(&self, actor: Uuid, id: Uuid) -> Result<()> {
        if actor != self.namespace.owner_user_id {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        let changed=sqlx::query("UPDATE playbook_grants SET revoked_at_ms=COALESCE(revoked_at_ms,$4) WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3").bind(id).bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(self.now()?).execute(&mut *tx).await?.rows_affected();
        if changed != 1 {
            return Err(Error::Forbidden);
        }
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn grant_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        revision: i64,
        live: bool,
    ) -> Result<GrantScope> {
        self.grant_record_locked(tx, id, revision, live, true).await
    }
    async fn grant_record_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        revision: i64,
        live: bool,
        check_owner: bool,
    ) -> Result<GrantScope> {
        let row=sqlx::query("SELECT * FROM playbook_grants WHERE grant_id=$1 AND namespace_id=$2 AND generation=$3 FOR UPDATE").bind(id).bind(self.namespace.namespace_id).bind(self.namespace.generation).fetch_optional(&mut **tx).await?.ok_or(Error::Forbidden)?;
        if row.get::<i64, _>("revision") != revision
            || row.get::<Uuid, _>("owner_user_id") != self.namespace.owner_user_id
        {
            return Err(Error::Forbidden);
        }
        if check_owner {
            Self::owner(tx, self.namespace.owner_user_id).await?;
        }
        if live
            && (row.get::<Option<i64>, _>("revoked_at_ms").is_some()
                || row.get::<i64, _>("expires_at_ms") <= self.now()?)
        {
            return Err(Error::Forbidden);
        }
        let scope: GrantScope = serde_json::from_value(row.get("scope"))?;
        self.grant_authority(id, revision, &scope)?;
        Ok(scope)
    }
    pub(super) fn intent_authority(
        &self,
        intent: &Intent,
        id: Uuid,
        revision: i64,
        scope: &GrantScope,
    ) -> Result<()> {
        if intent.authority != self.grant_authority(id, revision, scope)?
            || !self.known_contract(&intent.command)
            || !intent.command.capabilities.is_subset(&scope.capabilities)
        {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) async fn authorize_contract(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        contract: &aq_playbook_host::model::CommandContract,
        authority: &aq_playbook_host::runner::Authority,
        live: bool,
    ) -> Result<()> {
        if !self.known_contract(contract) {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self.grant_locked(&mut tx, id, revision, live).await?;
        if self.grant_authority(id, revision, &scope)? != *authority
            || !contract.capabilities.is_subset(&scope.capabilities)
        {
            return Err(Error::Forbidden);
        }
        if live {
            self.check_expiry(&mut tx, id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn active_authority(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
    ) -> Result<aq_playbook_host::runner::Authority> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self.grant_locked(&mut tx, id, revision, true).await?;
        self.check_expiry(&mut tx, id).await?;
        let authority = self.grant_authority(id, revision, &scope)?;
        tx.commit().await?;
        Ok(authority)
    }
    pub(super) fn identity(&self, intent: &Intent) -> Result<String> {
        digest(&(
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
            &intent.run,
            &intent.step,
            intent.invocation,
            intent.deadline_at_ms,
            &intent.command,
            &intent.input_digest,
            &intent.authority,
        ))
    }
    pub(super) async fn check_expiry(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
    ) -> Result<()> {
        let expiry: i64 =
            sqlx::query_scalar("SELECT expires_at_ms FROM playbook_grants WHERE grant_id=$1")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if expiry <= self.now()? {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub async fn authorize(&self, owner: &RuntimeOwner, id: Uuid, revision: i64) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        self.grant_locked(&mut tx, id, revision, true).await?;
        self.check_expiry(&mut tx, id).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn send_message(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<MessageResult> {
        let input = message_input(intent)?;
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        // Historical replay is exact but does not create or refresh any write authority.
        if let Some(row)=sqlx::query("SELECT request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await? {
   let scope=self.grant_locked(&mut tx,id,revision,false).await?;self.intent_authority(intent,id,revision,&scope)?;if row.get::<String,_>("request_digest")!=identity{return Err(Error::Conflict)}let result=if row.get::<String,_>("status")=="queued"{MessageResult::Queued(row.get("receipt"))}else{MessageResult::Cancelled};tx.commit().await?;return Ok(result)
  }
        let scope = self.grant_locked(&mut tx, id, revision, true).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) || intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        if let Some(cancel)=sqlx::query_scalar::<_,String>("SELECT request_digest FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await? {if cancel!=identity{return Err(Error::Conflict)}return Ok(MessageResult::Cancelled)}
        let op = stable_id(
            "aq.playbook.message.operation",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let message = stable_id(
            "aq.playbook.message.identity",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let recipient: messaging::Recipient = serde_json::from_value(json!(input.recipient))?;
        let response = messaging::owner_send_in_tx(
            &mut tx,
            messaging::Send {
                operation_id: op,
                message_id: message,
                thread_id: input.thread_id,
                recipient,
                body: input.body,
            },
            self.namespace.owner_user_id,
            self.now()? as u64,
        )
        .await
        .map_err(|e| match e {
            messaging::OwnerSendError::Invalid => Error::Invalid,
            messaging::OwnerSendError::NotFound => Error::Forbidden,
            messaging::OwnerSendError::Conflict => Error::Conflict,
            messaging::OwnerSendError::Unknown => Error::Unknown,
        })?;
        message_contract().outcomes["queued"]
            .validate(&response)
            .map_err(|_| Error::Unknown)?;
        let event_id = stable_id(
            "aq.playbook.message.queued.event",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let c = operation_event();
        let at = self.now()?;
        let event = EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: event_id.to_string(),
            contract: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), op.to_string())]),
            subject: Some(input.recipient.scope().2.to_string()),
            revision: None,
            causation: Some(intent.id.clone()),
            occurred_at_ms: Some(at),
            observed_at_ms: Some(at),
            payload: json!({"operation_id":op,"message_id":message,"disposition":"queued"}),
        };
        let event_digest = digest(&event)?;
        let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
        let aad = canonical_bytes(&(
            "aq.playbooks.outbox",
            1,
            self.namespace.namespace_id,
            self.namespace.generation,
            event_id,
            &event_digest,
        ))
        .map_err(|_| Error::Invalid)?;
        let sealed = self.keys.seal(&aad, &bytes).map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,'message.send',$5,$6,$7,$8,$9,$10,'queued',$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).bind(&intent.run).bind(&identity).bind(intent.command.fingerprint().map_err(|_|Error::Invalid)?).bind(&intent.input_digest).bind(id).bind(revision).bind(op).bind(&response).bind(at).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,'aq.host.bridge@1',0,$3,$4,$5,$6,$7,$8,$9,'pending')").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(event_id).bind(event_digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).execute(&mut *tx).await?;
        self.check_expiry(&mut tx, id).await?;
        if intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        tx.commit().await?;
        Ok(MessageResult::Queued(response))
    }
    pub async fn reconcile(
        &self,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<Option<MessageResult>> {
        message_input(intent)?;
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self.grant_locked(&mut tx, id, revision, false).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        let row=sqlx::query("SELECT request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await?;
        let result = match row {
            Some(row) => {
                if row.get::<String, _>("request_digest") != identity {
                    return Err(Error::Conflict);
                }
                Some(if row.get::<String, _>("status") == "queued" {
                    MessageResult::Queued(row.get("receipt"))
                } else {
                    MessageResult::Cancelled
                })
            }
            None => {
                let cancel=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await?;
                if let Some(cancel) = cancel {
                    if cancel.get::<String, _>("request_digest") != identity {
                        return Err(Error::Conflict);
                    }
                    if cancel.get::<String, _>("disposition") == "confirmed" {
                        Some(MessageResult::Cancelled)
                    } else {
                        return Err(Error::Unknown);
                    }
                } else {
                    None
                }
            }
        };
        tx.commit().await?;
        Ok(result)
    }
    pub async fn cancel(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<CancellationResult> {
        let input = message_input(intent)?;
        let identity = self.identity(intent)?;
        let key = aq_playbook_host::runner::cancellation_key(intent).map_err(|_| Error::Invalid)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self.grant_locked(&mut tx, id, revision, false).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        if let Some(row)=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND cancellation_key=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&key).fetch_optional(&mut *tx).await?{if row.get::<String,_>("request_digest")!=identity{return Err(Error::Conflict)}return Ok(if row.get::<String,_>("disposition")=="confirmed"{CancellationResult::Confirmed}else{CancellationResult::TooLate})}
        let existing=sqlx::query_scalar::<_,String>("SELECT request_digest FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await?;
        if existing.as_ref().is_some_and(|d| d != &identity) {
            return Err(Error::Conflict);
        }
        let disposition = if existing.is_some() {
            "too_late"
        } else {
            "confirmed"
        };
        sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(key).bind(&intent.id).bind(identity).bind(disposition).bind(self.now()?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(if existing.is_some() {
            CancellationResult::TooLate
        } else {
            CancellationResult::Confirmed
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub request_id: Uuid,
    pub grant_id: Uuid,
    pub grant_revision: i64,
    pub binding: String,
    pub expected_activation_revision: u64,
    pub entry: String,
    pub input: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionStatus {
    pub request_id: Uuid,
    pub engine_key: String,
    pub status: String,
    pub receipt: Option<aq_playbook_host::runner::KeyedAdmissionReceipt>,
}
/// Durable recovery data, not fresh write authorization. Only the driver can
/// inspect the retained exact request, and it must separately check current grant.
pub struct RetainedAdmission {
    pub status: AdmissionStatus,
    pub(super) request: aq_playbook_host::runner::KeyedAdmissionRequest,
    pub(super) grant_id: Uuid,
    pub(super) grant_revision: i64,
}
impl Service {
    pub(super) fn aad(&self, domain: &str, id: Uuid, digest: &str) -> Result<Vec<u8>> {
        canonical_bytes(&(
            domain,
            1,
            self.namespace.namespace_id,
            self.namespace.generation,
            id,
            digest,
        ))
        .map_err(|_| Error::Invalid)
    }
    pub async fn prepare(&self, actor: Uuid, request: PrepareRequest) -> Result<AdmissionStatus> {
        #[cfg(feature = "delivery_playbook")]
        if self.profile.is_delivery() && request.binding == super::domain::host10::DELIVERY_BINDING
        {
            return Err(Error::Forbidden);
        }
        if actor != self.namespace.owner_user_id
            || request.request_id.is_nil()
            || request.grant_id.is_nil()
            || request.grant_revision <= 0
            || request.expected_activation_revision == 0
            || request.expected_activation_revision > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        if request.binding.is_empty()
            || request.binding.len() > 512
            || request.binding.contains('\0')
            || request.entry.is_empty()
            || request.entry.len() > 512
            || request.entry.contains('\0')
        {
            return Err(Error::Invalid);
        }
        let key = stable_id(
            "aq.playbook.manual.admission",
            self.namespace.namespace_id,
            self.namespace.generation,
            &request.request_id.to_string(),
        )?
        .to_string();
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self
            .grant_locked(&mut tx, request.grant_id, request.grant_revision, false)
            .await?;
        let binding = request.binding.clone();
        let engine_request = aq_playbook_host::runner::KeyedAdmissionRequest {
            key: key.clone(),
            binding: request.binding,
            expected_activation_revision: request.expected_activation_revision,
            entry: request.entry,
            input: request.input,
            authority: self.grant_authority(request.grant_id, request.grant_revision, &scope)?,
        };
        let bytes = canonical_bytes(&engine_request).map_err(|_| Error::Invalid)?;
        if bytes.len() > 65536 {
            return Err(Error::Invalid);
        }
        let digest = digest(&engine_request)?;
        if let Some(old)=sqlx::query("SELECT request_digest,status,receipt FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(request.request_id).fetch_optional(&mut *tx).await? {if old.get::<String,_>("request_digest")!=digest{return Err(Error::Conflict)}let receipt:Option<Value>=old.get("receipt");let status=AdmissionStatus{request_id:request.request_id,engine_key:key,status:old.get("status"),receipt:receipt.map(serde_json::from_value).transpose()?};tx.commit().await?;return Ok(status)}
        self.grant_locked(&mut tx, request.grant_id, request.grant_revision, true)
            .await?;
        let sealed = self
            .keys
            .seal(
                &self.aad("aq.playbooks.admission", request.request_id, &digest)?,
                &bytes,
            )
            .map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_admissions(namespace_id,generation,request_id,request_digest,engine_key,grant_id,grant_revision,expected_activation_revision,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,binding) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,'pending',$14)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(request.request_id).bind(digest).bind(&key).bind(request.grant_id).bind(request.grant_revision).bind(request.expected_activation_revision as i64).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(self.now()?).bind(&binding).execute(&mut *tx).await?;
        self.check_expiry(&mut tx, request.grant_id).await?;
        tx.commit().await?;
        Ok(AdmissionStatus {
            request_id: request.request_id,
            engine_key: key,
            status: "pending".into(),
            receipt: None,
        })
    }
    pub async fn admission(&self, actor: Uuid, id: Uuid) -> Result<RetainedAdmission> {
        if actor != self.namespace.owner_user_id || id.is_nil() {
            return Err(Error::Forbidden);
        }
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        Self::owner(&mut tx, actor).await?;
        let row=sqlx::query("SELECT * FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        let digest: String = row.get("request_digest");
        let sealed = Sealed {
            version: row.get::<i32, _>("sealed_version") as u32,
            key_id: row.get("key_id"),
            nonce: row.get("nonce"),
            ciphertext: row.get("ciphertext"),
        };
        let bytes = self
            .keys
            .open(&self.aad("aq.playbooks.admission", id, &digest)?, &sealed)
            .map_err(|_| Error::Unknown)?;
        let request: aq_playbook_host::runner::KeyedAdmissionRequest =
            serde_json::from_slice(&bytes)?;
        let key: String = row.get("engine_key");
        let grant_id: Uuid = row.get("grant_id");
        let revision: i64 = row.get("grant_revision");
        let scope = self
            .grant_locked(&mut tx, grant_id, revision, false)
            .await?;
        if super::domain::digest(&request)? != digest
            || request.key != key
            || request.authority != self.grant_authority(grant_id, revision, &scope)?
        {
            return Err(Error::Conflict);
        }
        let receipt: Option<Value> = row.get("receipt");
        let status = AdmissionStatus {
            request_id: id,
            engine_key: key,
            status: row.get("status"),
            receipt: receipt.map(serde_json::from_value).transpose()?,
        };
        tx.commit().await?;
        Ok(RetainedAdmission {
            status,
            request,
            grant_id,
            grant_revision: revision,
        })
    }
    pub async fn acknowledge_admission(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        receipt: aq_playbook_host::runner::KeyedAdmissionReceipt,
    ) -> Result<AdmissionStatus> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let row=sqlx::query("SELECT request_digest,engine_key,expected_activation_revision,receipt,status FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND request_id=$3 FOR UPDATE").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        if receipt.request_digest != row.get::<String, _>("request_digest")
            || receipt.key != row.get::<String, _>("engine_key")
            || receipt.activation_revision
                != row.get::<i64, _>("expected_activation_revision") as u64
        {
            return Err(Error::Conflict);
        }
        if let Some(old) = row.get::<Option<Value>, _>("receipt")
            && old != json!(receipt)
        {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE playbook_admissions SET status='engine_accepted',held_reason=NULL,receipt=$4 WHERE namespace_id=$1 AND generation=$2 AND request_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).bind(json!(receipt)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(AdmissionStatus {
            request_id: id,
            engine_key: receipt.key.clone(),
            status: "engine_accepted".into(),
            receipt: Some(receipt),
        })
    }
    pub async fn pending_admissions(&self, owner: &RuntimeOwner, limit: u32) -> Result<Vec<Uuid>> {
        if !(1..=16).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let rows=sqlx::query_scalar("SELECT request_id FROM playbook_admissions WHERE namespace_id=$1 AND generation=$2 AND status='pending' ORDER BY held_reason IS NOT NULL,created_at_ms,request_id LIMIT $3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        Ok(rows)
    }
    pub async fn outbox(
        &self,
        owner: &RuntimeOwner,
        after: i64,
        limit: u32,
    ) -> Result<Vec<(i64, Uuid, EventEnvelope)>> {
        let (page, undecryptable) = self.outbox_page(owner, after, limit).await?;
        if undecryptable.is_some() {
            return Err(Error::Unknown);
        }
        Ok(page.into_iter().map(|(s, id, e, _)| (s, id, e)).collect())
    }
    /// [`Self::outbox`], each row with whether the engine accepted it (its
    /// receipt is recorded). A queued event that cannot be decrypted is not
    /// held (fixing the keyring recovers it): the page ends before it, with
    /// the reason naming the row and its key id.
    pub(super) async fn outbox_page(
        &self,
        owner: &RuntimeOwner,
        after: i64,
        limit: u32,
    ) -> Result<(Vec<(i64, Uuid, EventEnvelope, bool)>, Option<String>)> {
        if after < 0 || !(1..=16).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let rows=sqlx::query("SELECT * FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND sequence>$3 AND (status='pending' OR (status='engine_accepted' AND NOT fanout_complete)) ORDER BY sequence LIMIT $4").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(after).bind(limit as i64).fetch_all(&mut *tx).await?;
        let (mut result, mut undecryptable) = (Vec::new(), None);
        for row in rows {
            let id: Uuid = row.get("event_id");
            let digest: String = row.get("event_digest");
            let sealed = Sealed {
                version: row.get::<i32, _>("sealed_version") as u32,
                key_id: row.get("key_id"),
                nonce: row.get("nonce"),
                ciphertext: row.get("ciphertext"),
            };
            let aad = self.aad("aq.playbooks.outbox", id, &digest)?;
            let producer: String = row.get("producer");
            let Ok(bytes) = self.keys.open(&aad, &sealed) else {
                let key = &sealed.key_id;
                undecryptable = Some(format!(
                    "cannot decrypt queued event {id} (key id {key}): fix the keyring, or hold the event"
                ));
                break;
            };
            let plugin_event: Option<String> = row.get("plugin_event_id");
            let event_id = plugin_event
                .as_deref()
                .map_or_else(|| id.to_string(), str::to_owned);
            let event = (serde_json::from_slice::<EventEnvelope>(&bytes).ok()).filter(|event| {
                super::domain::digest(event).is_ok_and(|d| d == digest)
                    && event.event_id == event_id
                    && event.producer == producer
                    && if plugin_event.is_some() {
                        // Sealed only after validation against the installed contract.
                        // It may have been upgraded since this row was committed.
                        extension_capability(&producer)
                            .is_some_and(|name| command_plugin(&event.contract.name) == Some(name))
                    } else {
                        aq_playbook_host::model::validate_event(event, self.registry()).is_ok()
                    }
            });
            let Some(event) = event else {
                // A row whose event does not match its record is held, not
                // retried forever.
                sqlx::query("UPDATE playbook_outbox SET status='unknown',held_reason='invalid' WHERE namespace_id=$1 AND generation=$2 AND event_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).execute(&mut *tx).await?;
                continue;
            };
            let accepted = row.get::<Option<Value>, _>("engine_receipt").is_some();
            result.push((row.get("sequence"), id, event, accepted));
        }
        tx.commit().await?;
        Ok((result, undecryptable))
    }
    pub async fn acknowledge_event(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        receipt: aq_playbook_host::runner::EventDisposition,
        fanout_complete: bool,
    ) -> Result<()> {
        if receipt.destinations > 10000 {
            return Err(Error::Conflict);
        }
        let stable = json!({"key":receipt.key,"destinations":receipt.destinations});
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows=sqlx::query("SELECT producer,engine_receipt,plugin_event_id FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 FOR UPDATE").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).fetch_all(&mut *tx).await?;
        let row = rows
            .into_iter()
            .find(|row| {
                let producer: String = row.get("producer");
                let event_id: Option<String> = row.get("plugin_event_id");
                digest(&(&producer, 0_u64, event_id.unwrap_or_else(|| id.to_string())))
                    .ok()
                    .is_some_and(|key| stable["key"] == key)
            })
            .ok_or(Error::Conflict)?;
        let producer: String = row.get("producer");
        if extension_capability(&producer).is_none()
            && !self
                .registry()
                .events
                .values()
                .any(|c| c.trusted_producers.contains(&producer))
        {
            return Err(Error::Conflict);
        }
        let old: Option<Value> = row.get("engine_receipt");
        if old.is_some_and(|v| {
            v["key"] != stable["key"] || v["destinations"] != stable["destinations"]
        }) {
            return Err(Error::Conflict);
        }
        sqlx::query("UPDATE playbook_outbox SET status='engine_accepted',engine_receipt=COALESCE(engine_receipt,$4),fanout_complete=fanout_complete OR $5 WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND producer=$6").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).bind(stable).bind(fanout_complete).bind(&producer).execute(&mut *tx).await?;
        if producer == super::review_domain::GRAPH_PRODUCER {
            // The engine has durably accepted this exact event key; only now is
            // the graph outbox row delivered. The owner route keeps the row.
            sqlx::query("UPDATE agent_graph_outbox SET delivered=true WHERE event_id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if producer == super::review_domain::SETTLEMENT_PRODUCER {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE profiled_task_completions SET outbox_acknowledged=true WHERE operation_id=$1 AND state='confirmed' AND NOT outbox_acknowledged")
                .bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn retire_runtime(&self, owner: &RuntimeOwner) -> Result<()> {
        self.retire(owner, true).await
    }
    pub(crate) async fn retire_for_shutdown(&self, owner: &RuntimeOwner) -> Result<()> {
        self.retire(owner, false).await
    }
    async fn retire(&self, owner: &RuntimeOwner, current_user: bool) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        let next = owner.revision.checked_add(1).ok_or(Error::Overloaded)?;
        if ns.get::<Option<Uuid>, _>("runtime_incarnation").is_none()
            && ns.get::<i64, _>("runtime_revision") == next
        {
            tx.commit().await?;
            return Ok(());
        }
        self.runtime(&ns, owner)?;
        if current_user {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        }
        sqlx::query("UPDATE playbook_namespaces SET runtime_revision=$2,runtime_incarnation=NULL WHERE namespace_id=$1").bind(self.namespace.namespace_id).bind(next).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Service {
    /// Hold pending work with its reason (an outbox event, else an
    /// admission): it is kept, listed with the reason, no longer delivered,
    /// and can be retried. An admission whose reason is `uncertain` (its
    /// outcome is unknown) stays pending, delivered after the others.
    pub(super) async fn hold(
        &self,
        owner: &RuntimeOwner,
        event: bool,
        id: Uuid,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let sql = if event {
            "UPDATE playbook_outbox SET status='unknown',held_reason=$4 WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND status='pending'"
        } else {
            "UPDATE playbook_admissions SET status=CASE WHEN $4='uncertain' THEN 'pending' ELSE 'unknown' END,held_reason=$4 WHERE namespace_id=$1 AND generation=$2 AND request_id=$3 AND receipt IS NULL AND status='pending'"
        };
        sqlx::query(sql)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(id)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    /// The engine has started or forgotten every destination of an accepted event.
    pub(super) async fn complete_event(&self, owner: &RuntimeOwner, id: Uuid) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        sqlx::query("UPDATE playbook_outbox SET fanout_complete=true WHERE namespace_id=$1 AND generation=$2 AND event_id=$3 AND status='engine_accepted'").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Service {
    pub(super) fn current_time(&self) -> i64 {
        (self.clock)().timestamp_millis()
    }
}

#[derive(Debug, Clone)]
pub(super) enum DecisionResult {
    Accepted { handle: String },
    Completed { outcome: String, output: Value },
    Cancelled,
}
fn review_error(error: crate::error::AppError) -> Error {
    use crate::error::AppError;
    match error {
        AppError::BadRequest(_) => Error::Invalid,
        AppError::Forbidden(_) | AppError::Unauthenticated(_) => Error::Forbidden,
        AppError::Conflict(_) => Error::Conflict,
        AppError::PayloadTooLarge(_) => Error::Overloaded,
        _ => Error::Unknown,
    }
}
impl Service {
    pub fn human_owner(&self) -> Option<Arc<crate::reviews::Service>> {
        self.reviews.clone()
    }
    fn decision_input(&self, intent: &Intent) -> Result<super::review_domain::DecisionInput> {
        if !self.profile.has_reviews()
            || intent.command != super::review_domain::decision_contract()
        {
            return Err(Error::Forbidden);
        }
        if intent.input_digest != digest(&intent.input)?
            || intent.id.is_empty()
            || intent.id.len() > 512
            || intent.run.is_empty()
            || intent.run.len() > 512
            || intent.deadline_at_ms <= 0
        {
            return Err(Error::Invalid);
        }
        let input: super::review_domain::DecisionInput =
            serde_json::from_value(intent.input.clone())?;
        input.validate()?;
        Ok(input)
    }
    fn decision_id(&self, intent: &Intent) -> Result<Uuid> {
        stable_id(
            "aq.playbook.human-decision",
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )
    }
    fn exact_review_recipient(
        recipient: &Recipient,
        subject: &aq_review_protocol::ReviewSubject,
    ) -> bool {
        if recipient.scope()
            != (
                subject.task.workspace_id,
                subject.project_id,
                subject.task.task_id,
            )
        {
            return false;
        }
        match (recipient, &subject.binding.session) {
            (Recipient::Task { .. }, _) => true,
            (
                Recipient::Session {
                    session_id,
                    attempt_id,
                    generation,
                    ..
                },
                aq_session_protocol::SessionIdentity::Remote {
                    session_id: actual_session,
                    attempt_id: actual_attempt,
                    assignment_generation,
                    ..
                },
            ) => {
                session_id == actual_session
                    && attempt_id == actual_attempt
                    && generation == assignment_generation
            }
            _ => false,
        }
    }
    pub(super) async fn request_decision(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<DecisionResult> {
        let input = self.decision_input(intent)?;
        // Exact historical lookup precedes fresh Git I/O and cannot admit a new
        // question. Unknown reads remain unknown, never a second operation ID.
        if let Some(old) = self.reconcile_decision(id, revision, intent).await? {
            return Ok(old);
        }
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let prepared = reviews
            .prepare(input.report_id, input.source_operation_id)
            .await
            .map_err(review_error)?;
        if prepared.subject().artifact().map_err(|_| Error::Invalid)? != input.expected_subject
            || !Self::exact_review_recipient(&input.recipient, prepared.subject())
        {
            return Err(Error::Forbidden);
        }
        let request = aq_review_protocol::DecisionRequest {
            schema_version: 1,
            operation_id: stable_id(
                "aq.playbook.human-operation",
                self.namespace.namespace_id,
                self.namespace.generation,
                &intent.id,
            )?,
            decision_id: self.decision_id(intent)?,
            subject: input.expected_subject.clone(),
            eligible_owners: input.eligible_owners,
            eligibility_project_revision: input.eligibility_project_revision,
            review_policy: prepared.subject().validation_policy.clone(),
            expires_at_ms: input.expires_at_ms,
        };
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        crate::reviews::Service::lock_budget(&mut tx)
            .await
            .map_err(review_error)?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_record_locked(&mut tx, id, revision, true, false)
            .await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) || intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        if let Some(cancel)=sqlx::query_scalar::<_,String>("SELECT request_digest FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await? {
            if cancel!=identity {return Err(Error::Conflict);}
            Self::owner(&mut tx,self.namespace.owner_user_id).await?;
            tx.commit().await?;
            return Ok(DecisionResult::Cancelled);
        }
        if let Some(row)=sqlx::query("SELECT request_digest,operation_id,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await? {
            if row.get::<String,_>("request_digest")!=identity || row.get::<Uuid,_>("operation_id")!=request.operation_id || row.get::<Value,_>("receipt")!=json!({"handle":request.decision_id.to_string(),"subject":request.subject}) {return Err(Error::Conflict);}
            Self::owner(&mut tx,self.namespace.owner_user_id).await?;
            tx.commit().await?;
            return Ok(DecisionResult::Accepted {handle:request.decision_id.to_string()});
        }
        // SAME transaction: current namespace/grant, human owner question and
        // bridge effect receipt. No grant revocation/expiry admission window.
        reviews
            .admit_in_tx(&mut tx, self.namespace.owner_user_id, &prepared, &request)
            .await
            .map_err(review_error)?;
        self.check_expiry(&mut tx, id).await?;
        let receipt = json!({"handle":request.decision_id.to_string(),"subject":request.subject});
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,'decision.request',$5,$6,$7,$8,$9,$10,'accepted',$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).bind(&intent.run).bind(identity).bind(intent.command.fingerprint().map_err(|_|Error::Invalid)?).bind(&intent.input_digest).bind(id).bind(revision).bind(request.operation_id).bind(receipt).bind(self.now()?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(DecisionResult::Accepted {
            handle: request.decision_id.to_string(),
        })
    }
    pub(super) async fn reconcile_decision(
        &self,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<Option<DecisionResult>> {
        let input = self.decision_input(intent)?;
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let scope = self.grant_locked(&mut tx, id, revision, false).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        let row=sqlx::query("SELECT command_name,request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await?;
        let decision = self.decision_id(intent)?;
        if let Some(row) = row {
            if row.get::<String, _>("command_name") != "decision.request"
                || row.get::<String, _>("request_digest") != identity
                || row.get::<Value, _>("receipt")
                    != json!({"handle":decision.to_string(),"subject":input.expected_subject})
            {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return self
                .decision_status(decision, &input.expected_subject)
                .await
                .map(Some);
        }
        if let Some(row)=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await? {
            if row.get::<String,_>("request_digest")!=identity {return Err(Error::Conflict);}
            if row.get::<String,_>("disposition")!="confirmed" {return Err(Error::Unknown);}
            tx.commit().await?;return Ok(Some(DecisionResult::Cancelled));
        }
        tx.commit().await?;
        Ok(None)
    }
    async fn decision_status(
        &self,
        decision: Uuid,
        expected: &aq_session_protocol::ArtifactRef,
    ) -> Result<DecisionResult> {
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let view = reviews
            .inspect(self.namespace.owner_user_id, decision)
            .await
            .map_err(review_error)?;
        let request: aq_review_protocol::DecisionRequest =
            serde_json::from_value(view["request"].clone())?;
        if request.decision_id != decision || &request.subject != expected {
            return Err(Error::Conflict);
        }
        let Some(value) = view.get("receipt").filter(|v| !v.is_null()) else {
            return Ok(DecisionResult::Accepted {
                handle: decision.to_string(),
            });
        };
        let receipt: aq_review_protocol::DecisionReceipt = serde_json::from_value(value.clone())?;
        receipt
            .validate_for(&request)
            .map_err(|_| Error::Conflict)?;
        let outcome = match receipt.finality {
            aq_review_protocol::DecisionFinality::HumanAnswer {
                choice: aq_review_protocol::Choice::Approve,
                ..
            } => "approved",
            aq_review_protocol::DecisionFinality::HumanAnswer {
                choice: aq_review_protocol::Choice::Reject,
                ..
            } => "rejected",
            aq_review_protocol::DecisionFinality::Expired {} => "expired",
            aq_review_protocol::DecisionFinality::Cancelled { .. } => "cancelled",
        };
        Ok(DecisionResult::Completed {
            outcome: outcome.into(),
            output: json!({"receipt":receipt.artifact().map_err(|_|Error::Invalid)?,"subject":request.subject}),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionLookup {
    kind: String,
    handle: Uuid,
    recipient: Recipient,
}
impl Service {
    pub(super) async fn operation_get(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<DecisionResult> {
        if !self.profile.has_reviews()
            || intent.command != super::review_domain::operation_contract()
            || intent.input_digest != digest(&intent.input)?
        {
            return Err(Error::Forbidden);
        }
        let input: DecisionLookup = serde_json::from_value(intent.input.clone())?;
        input.recipient.validate()?;
        if input.kind != "human_decision" || input.handle.is_nil() {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self.grant_locked(&mut tx, id, revision, true).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        let reference:Value=sqlx::query_scalar("SELECT receipt->'subject' FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND command_name='decision.request' AND receipt->>'handle'=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(input.handle.to_string()).fetch_optional(&mut *tx).await?.ok_or(Error::Forbidden)?;
        let expected: aq_session_protocol::ArtifactRef = serde_json::from_value(reference)?;
        self.check_expiry(&mut tx, id).await?;
        tx.commit().await?;
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let view = reviews
            .inspect(self.namespace.owner_user_id, input.handle)
            .await
            .map_err(review_error)?;
        let subject: aq_review_protocol::ReviewSubject =
            serde_json::from_value(view["subject"].clone())?;
        if subject.artifact().map_err(|_| Error::Invalid)? != expected
            || !Self::exact_review_recipient(&input.recipient, &subject)
        {
            return Err(Error::Forbidden);
        }
        match self.decision_status(input.handle, &expected).await? {
            DecisionResult::Accepted { handle } => Ok(DecisionResult::Completed {
                outcome: "pending".into(),
                output: json!({"handle":handle,"subject":expected}),
            }),
            result => Ok(result),
        }
    }
}

impl Service {
    pub(super) async fn cancel_decision(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<CancellationResult> {
        let input = self.decision_input(intent)?;
        let identity = self.identity(intent)?;
        let key = aq_playbook_host::runner::cancellation_key(intent).map_err(|_| Error::Invalid)?;
        let operation = stable_id(
            "aq.playbook.human-cancellation",
            self.namespace.namespace_id,
            self.namespace.generation,
            &key,
        )?;
        let decision = self.decision_id(intent)?;
        let mut tx = self.tx().await?;
        crate::reviews::Service::lock_budget(&mut tx)
            .await
            .map_err(review_error)?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let scope = self
            .grant_record_locked(&mut tx, id, revision, false, false)
            .await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if !scope.permits(&input.recipient) {
            return Err(Error::Forbidden);
        }
        if let Some(row)=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND cancellation_key=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&key).fetch_optional(&mut *tx).await? {
            if row.get::<String,_>("request_digest")!=identity {return Err(Error::Conflict);}
            Self::owner(&mut tx,self.namespace.owner_user_id).await?;
            return Ok(if row.get::<String,_>("disposition")=="confirmed" {CancellationResult::Confirmed}else{CancellationResult::TooLate});
        }
        let existing=sqlx::query("SELECT request_digest,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&intent.id).fetch_optional(&mut *tx).await?;
        let mut disposition = "confirmed";
        if let Some(row) = existing {
            if row.get::<String, _>("request_digest") != identity
                || row.get::<Value, _>("receipt")
                    != json!({"handle":decision.to_string(),"subject":input.expected_subject})
            {
                return Err(Error::Conflict);
            }
            // Finality cannot be retracted. A known final receipt is TooLate;
            // missing/corrupt owner records remain Unknown and retain the key.
            let finality: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM human_decision_finality WHERE decision_id=$1)",
            )
            .bind(decision)
            .fetch_one(&mut *tx)
            .await?;
            if finality {
                disposition = "too_late";
                Self::owner(&mut tx, self.namespace.owner_user_id).await?;
            } else {
                self.reviews
                    .as_ref()
                    .ok_or(Error::Forbidden)?
                    .cancel_in_tx(&mut tx, self.namespace.owner_user_id, operation, decision)
                    .await
                    .map_err(review_error)?;
            }
        } else {
            Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        }
        sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(key).bind(&intent.id).bind(identity).bind(disposition).bind(self.now()?).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(if disposition == "confirmed" {
            CancellationResult::Confirmed
        } else {
            CancellationResult::TooLate
        })
    }
    /// Only this namespace's retained accepted questions; expiry admits no new
    /// vote. Release namespace locks before taking the review budget lock.
    pub(super) async fn expire_decisions(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        if !self.profile.has_reviews() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT d.decision_id FROM human_decision_requests d JOIN playbook_effects e ON e.receipt->>'handle'=d.decision_id::text WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND e.status='accepted' AND d.state='pending' AND d.expires_at_ms<=$3 ORDER BY d.expires_at_ms,d.decision_id LIMIT $4")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(self.now()?).bind(limit as i64).fetch_all(&mut *tx).await?;
        tx.commit().await?;
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let mut expired = 0;
        for id in ids {
            match reviews.expire(id).await {
                Ok(_) => expired += 1,
                Err(crate::error::AppError::Conflict(_)) => {}
                Err(e) => return Err(review_error(e)),
            }
        }
        Ok(expired)
    }
    /// Bounded copy to the existing engine outbox; owner-copy ACK and insert
    /// share SQL, engine acceptance/fanout ACK is a later independent boundary.
    pub(super) async fn collect_decision_events(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.has_reviews() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let reviews = self.reviews.as_ref().ok_or(Error::Forbidden)?;
        let mut tx = self.tx().await?;
        crate::reviews::Service::lock_budget(&mut tx)
            .await
            .map_err(review_error)?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows=sqlx::query("SELECT e.intent_id,e.receipt FROM playbook_effects e JOIN review_owner_outbox o ON o.decision_id=(e.receipt->>'handle')::uuid WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='decision.request' AND NOT o.acknowledged ORDER BY o.event_id LIMIT $3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).fetch_all(&mut *tx).await?;
        for row in &rows {
            let accepted: Value = row.get("receipt");
            let decision: Uuid = serde_json::from_value(accepted["handle"].clone())?;
            let (subject, payload) = reviews
                .completion_in_tx(&mut tx, decision)
                .await
                .map_err(review_error)?;
            if accepted["subject"] != json!(subject.artifact().map_err(|_| Error::Invalid)?)
                || subject.project_id != self.namespace.project_id
                || subject.task.workspace_id != self.namespace.workspace_id
            {
                return Err(Error::Conflict);
            }
            let c = super::review_domain::decision_event();
            let at = self.now()?;
            let event = EventEnvelope {
                producer: "aq.human.decisions@1".into(),
                event_id: decision.to_string(),
                contract: ContractRef {
                    name: c.name.clone(),
                    fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("decision".into(), decision.to_string())]),
                subject: None,
                revision: None,
                causation: Some(row.get("intent_id")),
                occurred_at_ms: None,
                observed_at_ms: Some(at),
                payload: json!({"outcome":payload["outcome"],"output":payload["output"]}),
            };
            aq_playbook_host::model::validate_event(&event, self.registry())
                .map_err(|_| Error::Invalid)?;
            let digest = digest(&event)?;
            let old:Option<String>=sqlx::query_scalar("SELECT event_digest FROM playbook_outbox WHERE namespace_id=$1 AND generation=$2 AND producer='aq.human.decisions@1' AND event_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(decision).fetch_optional(&mut *tx).await?;
            if let Some(old) = old {
                if old != digest {
                    return Err(Error::Conflict);
                }
            } else {
                let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
                let sealed = self
                    .keys
                    .seal(&self.aad("aq.playbooks.outbox", decision, &digest)?, &bytes)
                    .map_err(|_| Error::Unknown)?;
                sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,'aq.human.decisions@1',0,$3,$4,$5,$6,$7,$8,$9,'pending')").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(decision).bind(digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).execute(&mut *tx).await?;
            }
            sqlx::query("UPDATE review_owner_outbox SET acknowledged=true WHERE decision_id=$1")
                .bind(decision)
                .execute(&mut *tx)
                .await?;
        }
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        tx.commit().await?;
        Ok(rows.len())
    }
    /// Bounded seal of undelivered confirmed graph operations bound to this
    /// namespace's project into the engine outbox. `delivered` is set only by
    /// `acknowledge_event`, after durable engine acceptance; a crash between
    /// seal and ACK replays the same sealed event and the engine dedups on
    /// (producer, epoch 0, event_id).
    pub(super) async fn collect_graph_events(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !matches!(
            self.profile,
            CatalogProfile::GraphEvents3
                | CatalogProfile::SettlementEvents4
                | CatalogProfile::Monitoring6
        ) {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let producer = super::review_domain::GRAPH_PRODUCER;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let rows = sqlx::query("SELECT o.event_id,g.operation_id,g.attempt_id,b.binding->>'task_id' AS task_id FROM agent_graph_outbox o JOIN agent_graph_operations g ON g.operation_id=o.operation_id JOIN agent_launch_bindings b ON b.attempt_id=g.attempt_id WHERE NOT o.delivered AND g.phase='confirmed' AND b.binding->>'project_id'=$1 AND b.binding->>'workspace_id'=$2 AND NOT EXISTS(SELECT 1 FROM playbook_outbox p WHERE p.namespace_id=$3 AND p.generation=$4 AND p.producer='aq.graph.operations@1' AND p.event_id=o.event_id) ORDER BY g.operation_seq LIMIT $5")
            .bind(self.namespace.project_id.to_string())
            .bind(self.namespace.workspace_id.to_string())
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(limit as i64)
            .fetch_all(&mut *tx)
            .await?;
        let c = super::review_domain::graph_operation_event();
        let fingerprint = c.fingerprint().map_err(|_| Error::Invalid)?;
        for row in &rows {
            let id: Uuid = row.get("event_id");
            let operation: Uuid = row.get("operation_id");
            let attempt: Uuid = row.get("attempt_id");
            let task = row.get::<Option<String>, _>("task_id");
            let task: Option<Uuid> = task.and_then(|t| t.parse().ok());
            let at = self.now()?;
            let event = EventEnvelope {
                producer: producer.into(),
                event_id: id.to_string(),
                contract: ContractRef {
                    name: c.name.clone(),
                    fingerprint: fingerprint.clone(),
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("operation".into(), operation.to_string())]),
                subject: None,
                revision: None,
                causation: None,
                occurred_at_ms: None,
                observed_at_ms: Some(at),
                payload: json!({"operation_id":operation,"task_id":task,"attempt_id":attempt}),
            };
            // A bad source item (no task id, or not this contract) is queued
            // held `invalid`, so it is listed and later items are collected.
            let registry = self.registry();
            let valid = aq_playbook_host::model::validate_event(&event, registry).is_ok();
            let held = (task.is_none() || !valid).then_some("invalid");
            let status = if held.is_some() { "unknown" } else { "pending" };
            let digest = digest(&event)?;
            let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
            let sealed = self
                .keys
                .seal(&self.aad("aq.playbooks.outbox", id, &digest)?, &bytes)
                .map_err(|_| Error::Unknown)?;
            sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,held_reason) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,$11,$12)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(producer).bind(id).bind(digest).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(at).bind(status).bind(held).execute(&mut *tx).await?;
        }
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        tx.commit().await?;
        Ok(rows.len())
    }
}

#[path = "inspection/admissions.rs"]
mod inspection_admissions;

#[path = "inspection/catalog_auth.rs"]
mod catalog_auth;

#[path = "settlement_events.rs"]
mod settlement_events;
