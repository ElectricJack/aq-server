//! Explicit local owner configuration. No default activation or TCP fallback.
use super::{domain::GrantScope, review_domain::CatalogProfile, service::Namespace};
use crate::protected_storage::{Keyring, ProtectionConfig};
use aq_playbook_host::{
    model::Artifact,
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
use uuid::Uuid;
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
    deployments: Vec<DeploymentInput>,
    /// D7 integration policy (Host7/Host8 only). Absent leaves the D1 port unbound.
    #[serde(default)]
    integration: Option<IntegrationInput>,
}
/// Commit identity for central candidates and the publication evidence policy.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrationInput {
    commit_name: String,
    commit_email: String,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    review: bool,
    #[serde(default)]
    judgment_choices: Vec<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct IntegrationSettings {
    pub(crate) commit_name: String,
    pub(crate) commit_email: String,
    pub(crate) evidence: super::host_adapters::EvidencePolicy,
}
impl IntegrationInput {
    fn load(self) -> Result<IntegrationSettings> {
        let identity =
            |s: &str| !s.is_empty() && s.len() <= 256 && !s.contains(['\n', '\r', '\0', '<', '>']);
        let names = |v: &[String]| {
            v.len() <= 16
                && v.iter().enumerate().all(|(i, x)| {
                    (1..=64).contains(&x.len()) && !x.contains('\0') && !v[..i].contains(x)
                })
        };
        if !identity(&self.commit_name)
            || !identity(&self.commit_email)
            || !names(&self.checks)
            || !names(&self.judgment_choices)
            || (self.checks.is_empty() && !self.review && self.judgment_choices.is_empty())
        {
            return Err("invalid playbook integration policy");
        }
        Ok(IntegrationSettings {
            commit_name: self.commit_name,
            commit_email: self.commit_email,
            evidence: super::host_adapters::EvidencePolicy {
                checks: self.checks,
                review: self.review,
                judgment_choices: self.judgment_choices,
            },
        })
    }
}
#[derive(Deserialize)]
enum Profile {
    #[serde(rename = "human_review_v2")]
    HumanReview2,
    #[serde(rename = "graph_events_v3")]
    GraphEvents3,
    #[serde(rename = "settlement_events_v4")]
    SettlementEvents4,
    #[serde(rename = "monitoring_v6")]
    Monitoring6,
    #[serde(rename = "host_v7")]
    Host7,
    #[serde(rename = "host_v8")]
    Host8,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentInput {
    artifact_file: PathBuf,
    binding: String,
    grant_id: Uuid,
    scope: GrantScope,
    expires_at_ms: i64,
}
pub(crate) struct Deployment {
    pub artifact: Artifact,
    pub binding: String,
    pub grant_id: Uuid,
    pub scope: GrantScope,
    pub expires_at_ms: i64,
}
pub struct Loaded {
    pub(crate) namespace: Namespace,
    pub(crate) keys: Arc<Keyring>,
    pub(crate) engine_keys: ProtectionKeyring,
    pub(crate) connection: pg::Config,
    directory: File,
    directory_path: PathBuf,
    socket_owner_uid: u32,
    port: u16,
    pub(crate) interval: Duration,
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
fn private(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    if !path.is_absolute() {
        return Err("playbook private file must be absolute");
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "playbook private file unavailable")?;
    let m = file
        .metadata()
        .map_err(|_| "playbook private file unavailable")?;
    if !m.is_file()
        || m.mode() & 0o077 != 0
        || m.uid() != nix::unistd::geteuid().as_raw()
        || m.nlink() != 1
        || m.len() == 0
        || m.len() > max
    {
        return Err("unsafe playbook private file");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "playbook private file read failed")?;
    if bytes.len() as u64 > max {
        return Err("playbook private file bound");
    }
    Ok(bytes)
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
impl Configuration {
    fn load(self) -> Result<Loaded> {
        if !self.enabled {
            return Err("omit playbooks section to disable owner");
        }
        let profile = match self.profile {
            Profile::HumanReview2 => CatalogProfile::HumanReview2,
            Profile::GraphEvents3 => CatalogProfile::GraphEvents3,
            Profile::SettlementEvents4 => CatalogProfile::SettlementEvents4,
            Profile::Monitoring6 => CatalogProfile::Monitoring6,
            Profile::Host7 => CatalogProfile::Host7,
            Profile::Host8 => CatalogProfile::Host8,
        };
        if self.integration.is_some() && !profile.is_host2() {
            return Err("playbook integration requires the host_v7 or host_v8 profile");
        }
        let integration = self.integration.map(IntegrationInput::load).transpose()?;
        let n = &self.namespace;
        if [
            n.namespace_id,
            n.project_id,
            n.workspace_id,
            n.owner_user_id,
        ]
        .iter()
        .any(Uuid::is_nil)
            || n.generation <= 0
            || n.name.is_empty()
            || n.name.len() > 128
            || n.name.contains('\0')
            || !(250..=60000).contains(&self.drive_interval_ms)
            || self.deployments.len() > 4
        {
            return Err("invalid playbook owner identity or bounds");
        }
        let dsn = private(&self.engine_connection_file, 4096)?;
        let parsed: pg::Config = std::str::from_utf8(&dsn)
            .map_err(|_| "invalid playbook connection file")?
            .trim()
            .parse()
            .map_err(|_| "invalid playbook connection file")?;
        let directory_path = match parsed.get_hosts() {
            [Host::Unix(p)] if p.is_absolute() => p.clone(),
            _ => return Err("playbook owner requires one Unix socket endpoint"),
        };
        if parsed.get_ssl_mode() != SslMode::Disable
            || !parsed.get_hostaddrs().is_empty()
            || parsed.get_ports().len() != 1
            || parsed.get_ports()[0] == 0
            || parsed.get_options().is_some()
            || parsed
                .get_user()
                .is_none_or(|s| s.is_empty() || s.len() > 128)
            || parsed
                .get_dbname()
                .is_none_or(|s| s.is_empty() || s.len() > 128)
        {
            return Err("invalid local playbook connection policy");
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&directory_path)
            .map_err(|_| "playbook socket directory unavailable")?;
        let m = directory
            .metadata()
            .map_err(|_| "playbook socket directory unavailable")?;
        if !m.is_dir()
            || m.mode() & 0o022 != 0
            || (m.uid() != 0 && m.uid() != self.socket_owner_uid)
        {
            return Err("untrusted playbook socket directory");
        }
        let port = parsed.get_ports()[0];
        let mut connection = pg::Config::new();
        connection
            .host_path(format!("/proc/self/fd/{}", directory.as_raw_fd()))
            .port(port)
            .user(parsed.get_user().unwrap())
            .dbname(parsed.get_dbname().unwrap())
            .ssl_mode(SslMode::Disable)
            .connect_timeout(Duration::from_millis(1000))
            .application_name("aq-playbook-owner");
        if let Some(password) = parsed.get_password() {
            connection.password(password);
        }
        let raw = private(&self.engine_keyring_file, 4096)?;
        let mut input: EngineKeys =
            serde_json::from_slice(&raw).map_err(|_| "invalid playbook engine keys")?;
        if input.keys.is_empty()
            || input.keys.len() > 8
            || input.keys.keys().any(|k| k.is_empty() || k.len() > 64)
        {
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
        let engine_keys = ProtectionKeyring::new(input.active_key_id.clone(), keys)
            .map_err(|_| "invalid playbook engine keys")?;
        let keys = Arc::new(
            Keyring::load(&self.protection).map_err(|_| "invalid playbook bridge protection")?,
        );
        let mut deployments = vec![];
        let mut bindings = std::collections::BTreeSet::new();
        let mut total = 0;
        for d in self.deployments {
            if d.binding.is_empty()
                || d.binding.len() > 128
                || d.binding.contains('\0')
                || !bindings.insert(d.binding.clone())
                || d.grant_id.is_nil()
                || d.expires_at_ms <= 0
                || d.scope.project_id != n.project_id
                || d.scope.workspace_id != n.workspace_id
            {
                return Err("invalid playbook deployment binding");
            }
            profile
                .validate_scope(&d.scope)
                .map_err(|_| "invalid playbook deployment scope")?;
            let raw = private(&d.artifact_file, 245760)?;
            total += raw.len();
            if total > 1048576 {
                return Err("playbook deployment byte bound");
            }
            let artifact: Artifact =
                serde_json::from_slice(&raw).map_err(|_| "invalid compiled playbook artifact")?;
            artifact
                .verify()
                .map_err(|_| "invalid compiled playbook artifact")?;
            deployments.push(Deployment {
                artifact,
                binding: d.binding,
                grant_id: d.grant_id,
                scope: d.scope,
                expires_at_ms: d.expires_at_ms,
            });
        }
        let loaded = Loaded {
            namespace: self.namespace,
            keys,
            engine_keys,
            connection,
            directory,
            directory_path,
            socket_owner_uid: self.socket_owner_uid,
            port,
            interval: Duration::from_millis(self.drive_interval_ms),
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
        let held = self
            .directory
            .metadata()
            .map_err(|_| "playbook socket directory unavailable")?;
        let current = std::fs::symlink_metadata(&self.directory_path)
            .map_err(|_| "playbook socket directory unavailable")?;
        if !current.is_dir()
            || current.dev() != held.dev()
            || current.ino() != held.ino()
            || current.mode() & 0o022 != 0
        {
            return Err("playbook socket directory changed");
        }
        let socket = std::fs::symlink_metadata(format!(
            "/proc/self/fd/{}/.s.PGSQL.{}",
            self.directory.as_raw_fd(),
            self.port
        ))
        .map_err(|_| "playbook socket unavailable")?;
        if !socket.file_type().is_socket() || socket.uid() != self.socket_owner_uid {
            return Err("untrusted playbook socket");
        }
        Ok(())
    }
}
pub fn deserialize<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Arc<Loaded>>, D::Error> {
    Option::<Configuration>::deserialize(d)?
        .map(|c| c.load().map(Arc::new).map_err(serde::de::Error::custom))
        .transpose()
}
