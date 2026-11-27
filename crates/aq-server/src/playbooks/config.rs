//! Explicit local owner configuration. No default activation or TCP fallback.
use super::{review_domain::CatalogProfile, service::Namespace};
use crate::protected_storage::{Keyring, ProtectionConfig};
use aq_playbook_host::{
    postgres_client::{
        self as pg,
        config::{Host, SslMode},
    },
    postgres_storage::ProtectionKeyring,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use zeroize::{Zeroize, Zeroizing};
type Result<T> = std::result::Result<T, &'static str>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    enabled: bool,
    profile: Profile,
    namespace: Namespace,
    engine_connection_file: PathBuf,
    socket_owner_uid: u32,
    engine_keyring_file: PathBuf,
    protection: ProtectionConfig,
    drive_interval_ms: u64,
    /// `[playbooks.engine_timeouts]`; defaults 30 s statement, 500 ms lock.
    #[serde(default)]
    engine_timeouts: aq_playbook_host::protected::Timeouts,
    /// `[playbooks.retention]`; defaults keep 10,000 finished rows, delete 500.
    #[serde(default)]
    retention: aq_playbook_host::retention::Retention,
    /// `[playbooks.engine_retention]`; the engine's defaults.
    #[serde(default)]
    engine_retention: aq_playbook_host::engine_retention::EngineRetention,
    /// Largest request to the engine owner, in bytes; an install carries its
    /// whole artifact. Default 262,144; the host accepts 1,024 to 1,048,576.
    #[serde(default)]
    request_bytes: Option<usize>,
    deployments: Vec<DeploymentInput>,
    /// R1: `work.read`, `work.route`, `work.unroutable` and `judgment.assess`
    /// serve a task of any project on a routing-capable host.
    #[serde(default)]
    installation_routing: bool,
    /// D7 integration policy (Host7/Host8 only). Absent leaves the D1 port unbound.
    #[serde(default)]
    integration: Option<IntegrationInput>,
}
mod local_socket;
use aq_playbook_domain::config_policy::integration::IntegrationInput;
pub(crate) use aq_playbook_domain::config_policy::integration::IntegrationSettings;
use aq_playbook_domain::config_policy::{
    Deployment, DeploymentInput, Profile, load_deployments, valid_binding,
};
use local_socket::{LocalSocket, private};
pub struct Loaded {
    pub(crate) namespace: Namespace,
    pub(crate) keys: Arc<Keyring>,
    pub(crate) engine_keys: ProtectionKeyring,
    pub(crate) connection: pg::Config,
    socket: LocalSocket,
    pub(crate) interval: Duration,
    pub(crate) timeouts: aq_playbook_host::protected::Timeouts,
    pub(crate) retention: aq_playbook_host::retention::Retention,
    pub(crate) engine_retention: aq_playbook_host::engine_retention::EngineRetention,
    pub(crate) request_bytes: Option<usize>,
    pub(crate) installation_routing: bool,
    pub(crate) deployments: Vec<Deployment>,
    pub(crate) profile: CatalogProfile,
    pub(crate) integration: Option<IntegrationSettings>,
}
impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaybookOwnerConfig")
            .field("namespace", &self.namespace)
            .field("deployments", &self.deployments.len())
            .finish_non_exhaustive()
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineKeys {
    active_key_id: String,
    keys: BTreeMap<String, String>,
}
impl Drop for EngineKeys {
    fn drop(&mut self) {
        for x in self.keys.values_mut() {
            x.zeroize();
        }
    }
}
impl EngineKeys {
    fn load(file: &Path) -> Result<ProtectionKeyring> {
        let raw = private(file, 4096)?;
        let mut input: EngineKeys =
            serde_json::from_slice(&raw).map_err(|_| "invalid playbook engine keys")?;
        let bounded_names = input.keys.keys().all(|key| (1..=64).contains(&key.len()));
        if !(1..=8).contains(&input.keys.len()) || !bounded_names {
            return Err("invalid playbook engine keys");
        }
        let mut keys = BTreeMap::new();
        for (k, v) in &mut input.keys {
            let decoded =
                Zeroizing::new(hex::decode(&*v).map_err(|_| "invalid playbook engine keys")?);
            v.zeroize();
            keys.insert(
                k.clone(),
                decoded
                    .as_slice()
                    .try_into()
                    .map_err(|_| "invalid playbook engine keys")?,
            );
        }
        ProtectionKeyring::new(input.active_key_id.clone(), keys)
            .map_err(|_| "invalid playbook engine keys")
    }
}
impl Configuration {
    fn validate_bounds(&self) -> Result<()> {
        let n = &self.namespace;
        let ids = [
            n.namespace_id,
            n.project_id,
            n.workspace_id,
            n.owner_user_id,
        ]
        .iter()
        .all(|id| !id.is_nil());
        let identity = ids && n.generation > 0 && valid_binding(&n.name);
        let retention = self.retention.valid() && self.engine_retention.valid();
        let bounds = self.engine_timeouts.valid()
            && self.deployments.len() <= 4
            && (250..=60000).contains(&self.drive_interval_ms);
        if [identity, retention, bounds].contains(&false) {
            return Err("invalid playbook owner identity or bounds");
        }
        Ok(())
    }

    fn load(mut self) -> Result<Loaded> {
        if !self.enabled {
            return Err("omit playbooks section to disable owner");
        }
        let profile = CatalogProfile::from(self.profile);
        if self.installation_routing && !profile.has_routing() {
            return Err(
                "playbook installation_routing requires the host_v9 profile or a later routing profile",
            );
        }
        if self.integration.is_some() && !profile.is_host2() {
            return Err("playbook integration requires the host_v7 or host_v8 profile");
        }
        let integration = self
            .integration
            .take()
            .map(IntegrationInput::load)
            .transpose()?;
        self.validate_bounds()?;
        let (connection, socket) =
            LocalSocket::connect(&self.engine_connection_file, self.socket_owner_uid)?;
        let engine_keys = EngineKeys::load(&self.engine_keyring_file)?;
        let keys = Arc::new(
            Keyring::load(&self.protection).map_err(|_| "invalid playbook bridge protection")?,
        );
        let deployments = load_deployments(self.deployments, &self.namespace, profile, |path| {
            private(path, 245760)
        })?;
        // Poll faster than configured intervals so a slow general-purpose
        // drive setting does not turn every interval into catch-up.
        let drive_interval_ms = deployments
            .iter()
            .flat_map(|d| &d.schedules)
            .map(|schedule| schedule.every_ms / 2)
            .fold(self.drive_interval_ms, u64::min);
        let loaded = Loaded {
            namespace: self.namespace,
            keys,
            engine_keys,
            connection,
            socket,
            interval: Duration::from_millis(drive_interval_ms),
            timeouts: self.engine_timeouts,
            retention: self.retention,
            engine_retention: self.engine_retention,
            request_bytes: self.request_bytes,
            installation_routing: self.installation_routing,
            deployments,
            profile,
            integration,
        };
        loaded.check_socket()?;
        Ok(loaded)
    }
}
impl Loaded {
    pub(crate) fn check_socket(&self) -> Result<()> {
        self.socket.check()
    }
}
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Arc<Loaded>>, D::Error> {
    Option::<Configuration>::deserialize(d)?
        .map(|c| c.load().map(Arc::new).map_err(serde::de::Error::custom))
        .transpose()
}
