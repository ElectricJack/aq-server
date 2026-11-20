//! Operator-installed exec plugins for the playbook host.
use crate::model::{CatalogExtension, CommandContract, EventEnvelope, Registry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use uuid::Uuid;

pub const DEFAULT_PLUGINS_DIR: &str = "/opt/aq/plugins";
pub const DEFAULT_STATE_ROOT: &str = "/var/lib/aq-plugins";
const PROTOCOL: &str = "aq.plugin/1";
const STDERR_BYTES: usize = 65_536;
const STDOUT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Default)]
pub struct Plugins(BTreeMap<String, Plugin>);

#[derive(Clone, Debug)]
pub struct Plugin {
    extension: CatalogExtension,
    exe: PathBuf,
    state_dir: PathBuf,
    secrets: BTreeMap<String, PathBuf>,
    bindings: Option<PathBuf>,
    vault: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: String,
    version: String,
    protocol: String,
    exe: PathBuf,
    contracts: PathBuf,
    notes: PathBuf,
    #[serde(default)]
    secrets: Vec<String>,
    bindings: Option<PathBuf>,
}

pub fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    (2..=32).contains(&name.len())
        && bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub fn command_plugin(command: &str) -> Option<&str> {
    let (name, suffix) = command.strip_prefix("ext.")?.split_once('.')?;
    (valid_name(name) && !suffix.is_empty()).then_some(name)
}

pub fn extension_capability(capability: &str) -> Option<&str> {
    capability
        .strip_prefix("ext.")
        .filter(|name| valid_name(name))
}

pub fn extension_command(name: &str, command: &CommandContract) -> bool {
    let Some(plugin) = command_plugin(name) else {
        return false;
    };
    CatalogExtension::new(
        plugin,
        Registry {
            commands: BTreeMap::from([(name.to_owned(), command.clone())]),
            ..Default::default()
        },
        serde_json::json!({}),
    )
    .is_ok()
}

impl Plugins {
    pub fn discover(plugins_dir: &Path, state_root: &Path, vault: &Path) -> Self {
        let entries = match fs::read_dir(plugins_dir) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(%error, "playbook plugin directory unavailable");
                }
                return Self::default();
            }
        };
        let mut installed = BTreeMap::new();
        for entry in entries {
            let Ok(entry) = entry else {
                tracing::warn!("cannot read playbook plugin directory entry");
                continue;
            };
            let Some(name) = entry
                .file_name()
                .to_str()
                .filter(|name| valid_name(name))
                .map(str::to_owned)
            else {
                continue;
            };
            if !entry.path().is_dir() {
                continue;
            }
            match Plugin::load(&name, &entry.path(), state_root, vault) {
                Ok((plugin, version)) => {
                    tracing::info!(plugin = %name, %version, "playbook plugin loaded");
                    installed.insert(name, plugin);
                }
                Err(reason) => tracing::warn!(plugin = %name, %reason, "playbook plugin refused"),
            }
        }
        Self(installed)
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.0.get(name)
    }

    pub fn overlay(mut self, mut registry: Registry) -> (Self, Registry) {
        self.0.retain(|name, plugin| {
            match registry.with_extensions(std::slice::from_ref(&plugin.extension)) {
                Ok(composed) => {
                    registry = composed;
                    true
                }
                Err(error) => {
                    tracing::warn!(plugin = %name, %error, "playbook plugin overlay refused");
                    false
                }
            }
        });
        (self, registry)
    }
}

fn contained(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(relative).canonicalize().ok()?;
    path.starts_with(root).then_some(path)
}

fn private_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o7777 == 0o600)
}

impl Plugin {
    fn load(
        name: &str,
        package: &Path,
        state_root: &Path,
        vault: &Path,
    ) -> Result<(Self, String), String> {
        let manifest: Manifest = fs::read_to_string(package.join("plugin.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .ok_or("invalid plugin.toml")?;
        if manifest.name != name || manifest.version.is_empty() || manifest.protocol != PROTOCOL {
            return Err("invalid plugin identity or protocol".into());
        }
        let file = |relative: &Path| contained(package, relative).filter(|p| p.is_file());
        let exe = file(&manifest.exe)
            .filter(|p| fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0))
            .ok_or("plugin executable unavailable")?;
        let registry: Registry = file(&manifest.contracts)
            .and_then(|p| fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or("invalid plugin registry")?;
        let notes: Value = file(&manifest.notes)
            .and_then(|p| fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or("invalid plugin notes")?;
        let extension =
            CatalogExtension::new(name, registry, notes).map_err(|error| error.to_string())?;
        let refusal_bytes = r#"{"reason":"invalid_plugin_response"}"#.len();
        if extension
            .registry()
            .commands
            .values()
            .any(|c| c.max_payload_bytes < refusal_bytes)
        {
            return Err("plugin command payload budget cannot encode a host refusal".into());
        }
        let vault = vault
            .canonicalize()
            .map_err(|_| "plugin vault unavailable")?;
        let mut secrets = BTreeMap::new();
        for relative in manifest.secrets {
            let path = contained(&vault, Path::new(&relative))
                .filter(|p| private_file(p))
                .ok_or("plugin secret unavailable or mode differs from 0600")?;
            if secrets.insert(relative, path).is_some() {
                return Err("duplicate plugin secret".into());
            }
        }
        if manifest.bindings.as_ref().is_some_and(|path| {
            path.as_os_str().is_empty()
                || !path
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        }) {
            return Err("invalid plugin bindings path".into());
        }
        let bindings = manifest.bindings;
        let state_dir = state_root.join(name);
        fs::create_dir_all(state_dir.join("tmp")).map_err(|_| "plugin state unavailable")?;
        let state_dir = state_dir
            .canonicalize()
            .map_err(|_| "plugin state unavailable")?;
        Ok((
            Self {
                extension,
                exe,
                state_dir,
                secrets,
                bindings,
                vault,
            },
            manifest.version,
        ))
    }

    pub fn registry(&self) -> &Registry {
        self.extension.registry()
    }

    fn binding(&self, project_id: Uuid) -> Option<toml::Table> {
        let Some(path) = &self.bindings else {
            return Some(toml::Table::new());
        };
        let resolved = match self.vault.join(path).canonicalize() {
            Ok(resolved) if resolved.starts_with(&self.vault) && private_file(&resolved) => {
                resolved
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Some(toml::Table::new());
            }
            _ => return None,
        };
        let text = fs::read_to_string(resolved).ok()?;
        let table: toml::Table = text.parse().ok()?;
        let Some(projects) = table.get("projects") else {
            return Some(toml::Table::new());
        };
        match projects.as_table()?.get(&project_id.to_string()) {
            Some(table) => table.as_table().cloned(),
            None => Some(toml::Table::new()),
        }
    }

    pub async fn invoke(&self, call: Call<'_>) -> Invocation {
        let Some(binding) = self.binding(call.project_id) else {
            return Invocation::Unavailable;
        };
        if self.secrets.values().any(|path| !private_file(path)) {
            return Invocation::Unavailable;
        }
        let request = Request {
            protocol: PROTOCOL,
            command: &call.command.name,
            input: call.input,
            idempotency_key: call.idempotency_key,
            project_id: call.project_id,
            binding,
            state_dir: &self.state_dir,
            secret_paths: &self.secrets,
            deadline_ms: (call.command.deadline_ms / 2).min(call.remaining_ms),
        };
        let Ok(mut bytes) = serde_json::to_vec(&request) else {
            return Invocation::Unavailable;
        };
        bytes.push(b'\n');
        let mut child = match Command::new(&self.exe)
            .arg("invoke")
            .current_dir(&self.state_dir)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.state_dir)
            .env("TMPDIR", self.state_dir.join("tmp"))
            .env("AQ_PLUGIN_PROTOCOL", PROTOCOL)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return Invocation::Unavailable,
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let pid = child.id();
        let mut stderr_bytes = Vec::new();
        let result = tokio::time::timeout(Duration::from_millis(request.deadline_ms), async {
            let write = async move { stdin.write_all(&bytes).await };
            let mut stdout_bytes = Vec::new();
            let (written, stdout, stderr, status) = tokio::join!(
                write,
                capture(stdout, &mut stdout_bytes, STDOUT_BYTES),
                capture(stderr, &mut stderr_bytes, STDERR_BYTES),
                child.wait(),
            );
            (written.is_ok()
                && stdout.is_ok_and(|overflow| !overflow)
                && stderr.is_ok()
                && status.is_ok_and(|status| status.success()))
            .then_some(stdout_bytes)
        })
        .await;
        if result.is_err() {
            if let Some(pid) = pid {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.kill().await;
        }
        if !stderr_bytes.is_empty() {
            tracing::warn!(call_id = call.idempotency_key, stderr = %String::from_utf8_lossy(&stderr_bytes), "playbook plugin stderr");
        }
        let Some(bytes) = result.ok().flatten() else {
            return Invocation::Unknown;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            return Invocation::Unknown;
        };
        match serde_json::from_value(value) {
            Ok(response) => Invocation::Response(response),
            Err(_) => Invocation::Invalid,
        }
    }

    pub fn event(
        &self,
        intent_id: &str,
        index: usize,
        event: ProducedEvent,
        now_ms: i64,
    ) -> Option<EventEnvelope> {
        self.extension
            .event_envelope(intent_id, index, event.name, event.payload, now_ms)
            .ok()
    }
}

async fn capture(
    mut stream: impl AsyncRead + Unpin,
    bytes: &mut Vec<u8>,
    limit: usize,
) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    let mut overflow = false;
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            return Ok(overflow);
        }
        let keep = n.min(limit - bytes.len());
        bytes.extend_from_slice(&buffer[..keep]);
        overflow |= keep != n;
    }
}

pub struct Call<'a> {
    pub command: &'a CommandContract,
    pub input: &'a Value,
    pub idempotency_key: &'a str,
    pub project_id: Uuid,
    pub remaining_ms: u64,
}

#[derive(Serialize)]
struct Request<'a> {
    protocol: &'static str,
    command: &'a str,
    input: &'a Value,
    idempotency_key: &'a str,
    project_id: Uuid,
    binding: toml::Table,
    state_dir: &'a Path,
    secret_paths: &'a BTreeMap<String, PathBuf>,
    deadline_ms: u64,
}

pub enum Invocation {
    Response(Response),
    Unavailable,
    Unknown,
    Invalid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub outcome: String,
    pub output: Value,
    pub events: Vec<ProducedEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducedEvent {
    name: String,
    payload: Value,
}
