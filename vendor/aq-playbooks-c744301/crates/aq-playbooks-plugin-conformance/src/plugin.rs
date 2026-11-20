use anyhow::{Context, Result, ensure};
use aq_playbooks_model::{
    CatalogExtension, CommandContract, EventEnvelope, Registry, canonical_bytes,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::timeout,
};
use uuid::Uuid;

const PROTOCOL: &str = "aq.plugin/1";
const STDOUT_LIMIT: usize = 1_048_576;
const STDERR_LIMIT: usize = 65_536;
const EVENT_ENVELOPE_LIMIT: usize = 65_536;

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

pub(super) struct Plugin {
    pub(super) extension: CatalogExtension,
    executable: PathBuf,
    vault: PathBuf,
    state: PathBuf,
    secrets: BTreeMap<String, PathBuf>,
    bindings: Option<PathBuf>,
}

fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn contained(root: &Path, path: &Path) -> Result<PathBuf> {
    ensure!(relative(path), "plugin path must be relative and contained");
    let file = root.join(path).canonicalize()?;
    ensure!(file.starts_with(root), "plugin path escapes its root");
    Ok(file)
}

fn private_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && mode(&metadata) & 0o777 == 0o600)
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

impl Plugin {
    pub(super) fn load(package: &Path, state_root: &Path, vault: &Path) -> Result<Self> {
        ensure!(cfg!(unix), "F5 conformance requires a Unix host");
        let name = package
            .file_name()
            .and_then(|name| name.to_str())
            .context("plugin directory must have a name")?;
        let package = package
            .canonicalize()
            .context("plugin directory is unavailable")?;
        let manifest: Manifest = toml::from_str(
            &fs::read_to_string(package.join("plugin.toml")).context("cannot read plugin.toml")?,
        )
        .context("invalid plugin.toml")?;
        ensure!(
            manifest.name == name && manifest.protocol == PROTOCOL && !manifest.version.is_empty(),
            "plugin identity, directory name, version, or protocol is invalid"
        );
        let executable =
            contained(&package, &manifest.exe).context("plugin executable unavailable")?;
        let executable_info = fs::metadata(&executable)?;
        ensure!(
            executable_info.is_file() && mode(&executable_info) & 0o111 != 0,
            "plugin executable must be executable"
        );
        let registry: Registry = super::read_json(
            &contained(&package, &manifest.contracts)?,
            "plugin registry",
        )?;
        let notes = super::read_json(
            &contained(&package, &manifest.notes)?,
            "plugin catalog notes",
        )?;
        let extension = CatalogExtension::new(name, registry, notes)?;
        let vault = vault
            .canonicalize()
            .context("test vault directory is unavailable")?;
        let mut secrets = BTreeMap::new();
        for name in manifest.secrets {
            let path = contained(&vault, Path::new(&name)).context("plugin secret unavailable")?;
            ensure!(private_file(&path), "plugin secret must be a 0600 file");
            ensure!(
                secrets.insert(name, path).is_none(),
                "duplicate plugin secret"
            );
        }
        ensure!(
            manifest.bindings.as_deref().is_none_or(relative),
            "bindings path must be relative and contained"
        );
        let state = state_root.join(name);
        fs::create_dir_all(state.join("tmp")).context("cannot create plugin state/TMPDIR")?;
        let state = state.canonicalize()?;
        Ok(Self {
            extension,
            executable,
            vault,
            state,
            secrets,
            bindings: manifest.bindings,
        })
    }

    pub(super) fn binding(&self, project: Uuid, override_path: Option<&Path>) -> Result<Value> {
        if let Some(path) = override_path {
            let value: Value = super::read_json(path, "binding override")?;
            ensure!(value.is_object(), "binding override must be a JSON object");
            return Ok(value);
        }
        let Some(path) = &self.bindings else {
            return Ok(json!({}));
        };
        let absolute = match self.vault.join(path).canonicalize() {
            Ok(absolute) => absolute,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
            Err(error) => return Err(error.into()),
        };
        ensure!(
            absolute.starts_with(&self.vault) && private_file(&absolute),
            "bindings must be a 0600 vault file"
        );
        let value: toml::Table = fs::read_to_string(absolute)?.parse()?;
        let Some(projects) = value.get("projects") else {
            return Ok(json!({}));
        };
        let projects = projects.as_table().context("projects must be a table")?;
        let Some(binding) = projects.get(&project.to_string()) else {
            return Ok(json!({}));
        };
        let binding = binding
            .as_table()
            .context("project binding must be a table")?;
        Ok(serde_json::to_value(binding)?)
    }

    pub(super) async fn invoke(&self, call: Call<'_>) -> Result<Validated, Failure> {
        let binding = call
            .binding
            .as_ref()
            .map_err(|_| Failure::new(FailureKind::Unavailable, "invalid binding"))?;
        if self.secrets.values().any(|path| !private_file(path)) {
            return Err(Failure::new(
                FailureKind::Unavailable,
                "plugin secret unavailable",
            ));
        }
        let deadline_ms = (call.command.deadline_ms / 2).min(call.remaining_ms);
        if deadline_ms == 0 {
            return Err(Failure::new(
                FailureKind::Timeout,
                "fixture intent deadline elapsed",
            ));
        }
        let mut bytes = serde_json::to_vec(&Request {
            protocol: PROTOCOL,
            command: &call.command.name,
            input: call.input,
            idempotency_key: call.key,
            project_id: call.project,
            binding,
            state_dir: &self.state,
            secret_paths: &self.secrets,
            deadline_ms,
        })
        .map_err(|_| Failure::new(FailureKind::Unavailable, "could not encode invocation"))?;
        bytes.push(b'\n');
        let output = self.execute(&bytes, deadline_ms).await?;
        let value: Value = serde_json::from_slice(&output).map_err(|_| {
            Failure::new(
                FailureKind::InvalidJson,
                "plugin stdout must contain one JSON object",
            )
        })?;
        let response: Response = serde_json::from_value(value)
            .map_err(|_| Failure::invalid("invalid plugin response fields"))?;
        let envelopes = self.validate_response(&response, &call)?;
        Ok(Validated {
            bytes: output,
            response,
            envelopes,
        })
    }

    fn validate_response(
        &self,
        response: &Response,
        call: &Call<'_>,
    ) -> Result<Vec<EventEnvelope>, Failure> {
        let schema = call
            .command
            .outcomes
            .get(&response.outcome)
            .ok_or_else(|| {
                Failure::invalid("plugin outcome is absent from the command contract")
            })?;
        if schema.validate(&response.output).is_err()
            || !canonical_bytes(&response.output)
                .is_ok_and(|bytes| bytes.len() <= call.command.max_payload_bytes)
        {
            return Err(Failure::invalid(
                "plugin output violates the command contract",
            ));
        }
        response
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let envelope = self
                    .extension
                    .event_envelope(
                        call.intent_id,
                        index,
                        event.name.clone(),
                        event.payload.clone(),
                        call.now_ms,
                    )
                    .map_err(|_| Failure::invalid("plugin event violates its contract"))?;
                if !canonical_bytes(&envelope)
                    .is_ok_and(|bytes| bytes.len() <= EVENT_ENVELOPE_LIMIT)
                {
                    return Err(Failure::invalid(
                        "plugin event envelope exceeds host size limit",
                    ));
                }
                Ok(envelope)
            })
            .collect()
    }

    async fn execute(&self, input: &[u8], deadline_ms: u64) -> Result<Vec<u8>, Failure> {
        let mut command = Command::new(&self.executable);
        command
            .arg("invoke")
            .current_dir(&self.state)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", &self.state)
            .env("TMPDIR", self.state.join("tmp"))
            .env("AQ_PLUGIN_PROTOCOL", PROTOCOL)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|_| {
            Failure::new(FailureKind::Unavailable, "could not start plugin process")
        })?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let result = timeout(Duration::from_millis(deadline_ms), async {
            let write = async move { stdin.write_all(input).await };
            tokio::join!(
                write,
                capture(stdout, STDOUT_LIMIT),
                capture(stderr, STDERR_LIMIT),
                child.wait()
            )
        })
        .await;
        let Ok((written, stdout, stderr, status)) = result else {
            terminate(pid, &mut child).await;
            return Err(Failure::new(
                FailureKind::Timeout,
                "plugin exceeded its child deadline",
            ));
        };
        let (stdout, overflow) =
            stdout.map_err(|_| Failure::new(FailureKind::Io, "could not read plugin stdout"))?;
        if stderr.is_err() {
            return Err(Failure::new(
                FailureKind::Io,
                "could not collect plugin process result",
            ));
        }
        let status =
            status.map_err(|_| Failure::new(FailureKind::Io, "could not wait for plugin"))?;
        if !status.success() {
            return Err(Failure::new(
                FailureKind::NonzeroExit,
                "plugin exited with a failure status",
            ));
        }
        if written.is_err() {
            return Err(Failure::new(
                FailureKind::Io,
                "could not write plugin stdin",
            ));
        }
        if overflow {
            return Err(Failure::new(
                FailureKind::StdoutLimit,
                "plugin stdout exceeds host size limit",
            ));
        }
        Ok(stdout)
    }
}

async fn terminate(pid: Option<u32>, child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        use nix::{
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };
        let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.kill().await;
}

async fn capture(mut stream: impl AsyncRead + Unpin, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut buffer = [0; 8192];
    let mut bytes = Vec::new();
    let mut overflow = false;
    loop {
        let n = stream.read(&mut buffer).await?;
        if n == 0 {
            return Ok((bytes, overflow));
        }
        let keep = n.min(limit - bytes.len());
        bytes.extend_from_slice(&buffer[..keep]);
        overflow |= keep < n;
    }
}

pub(super) struct Call<'a> {
    pub(super) command: &'a CommandContract,
    pub(super) input: &'a Value,
    pub(super) key: &'a str,
    pub(super) project: Uuid,
    pub(super) binding: Result<Value>,
    pub(super) intent_id: &'a str,
    pub(super) now_ms: i64,
    pub(super) remaining_ms: u64,
}

#[derive(Serialize)]
struct Request<'a> {
    protocol: &'static str,
    command: &'a str,
    input: &'a Value,
    idempotency_key: &'a str,
    project_id: Uuid,
    binding: &'a Value,
    state_dir: &'a Path,
    secret_paths: &'a BTreeMap<String, PathBuf>,
    deadline_ms: u64,
}

pub(super) struct Validated {
    pub(super) bytes: Vec<u8>,
    pub(super) response: Response,
    pub(super) envelopes: Vec<EventEnvelope>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Response {
    pub(super) outcome: String,
    pub(super) output: Value,
    pub(super) events: Vec<ProducedEvent>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProducedEvent {
    pub(super) name: String,
    payload: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureKind {
    Unavailable,
    Timeout,
    NonzeroExit,
    Io,
    StdoutLimit,
    InvalidJson,
    InvalidResponse,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::NonzeroExit => "nonzero_exit",
            Self::Io => "io",
            Self::StdoutLimit => "stdout_limit",
            Self::InvalidJson => "invalid_json",
            Self::InvalidResponse => "invalid_response",
        };
        output.write_str(name)
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Failure {
    pub(super) kind: FailureKind,
    detail: &'static str,
}

impl Failure {
    pub(super) fn new(kind: FailureKind, detail: &'static str) -> Self {
        Self { kind, detail }
    }

    fn invalid(detail: &'static str) -> Self {
        Self::new(FailureKind::InvalidResponse, detail)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{} ({})", self.kind, self.detail)
    }
}

impl std::error::Error for Failure {}
