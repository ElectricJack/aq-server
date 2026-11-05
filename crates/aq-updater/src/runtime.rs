//! The boundary between the update engine and the host: Docker, Compose and
//! the database container. `Runtime` is the trait the engine drives;
//! `DockerCli` implements it with the `docker` CLI.

use aq_release::ReadinessReport;
use async_trait::async_trait;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// Which release to fetch: an exact version, or the configured channel's
/// current release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseSelector {
    Version(semver::Version),
    Channel,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The operation failed; the engine decides what that means for the
    /// update (refuse, abort or roll back).
    #[error("{0}")]
    Failed(String),
    /// The updater is going away mid-operation (shutdown, or a simulated
    /// crash in tests). The engine stops at once and leaves the journal as
    /// written, so `Engine::recover` resumes from the persisted step.
    #[error("interrupted: {0}")]
    Interrupted(String),
}

pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[async_trait]
pub trait Runtime: Send + Sync {
    /// The release's `(manifest bytes, signature bundle bytes)`, unverified.
    async fn fetch_release(&self, selector: ReleaseSelector) -> RuntimeResult<(Vec<u8>, Vec<u8>)>;
    async fn pull(&self, image: &str) -> RuntimeResult<()>;
    async fn image_labels(&self, image: &str) -> RuntimeResult<HashMap<String, String>>;
    async fn free_bytes(&self, path: &Path) -> RuntimeResult<u64>;
    /// Write a consistent database backup to `dest`; returns its size.
    async fn backup(&self, dest: &Path) -> RuntimeResult<u64>;
    /// Replace the database with `src`. Only called with the server stopped.
    async fn restore(&self, src: &Path) -> RuntimeResult<()>;
    /// Start (or replace) the server container with `image`. Also records
    /// `image` as the one a plain `docker compose up` starts.
    async fn start_server(&self, image: &str) -> RuntimeResult<()>;
    /// Record `image` as the one a plain `docker compose up` starts
    /// (`release.env`), without starting anything. Called before the server
    /// is stopped for a rollback restore, so a manual `compose up` after a
    /// failure mid-restore starts the release being brought back.
    async fn select_release(&self, image: &str) -> RuntimeResult<()>;
    async fn stop_server(&self) -> RuntimeResult<()>;
    async fn wait_healthy(&self, deadline: Duration) -> RuntimeResult<()>;
    async fn readiness(&self) -> RuntimeResult<ReadinessReport>;
    /// The state format recorded in the database's `meta` table; `None` when
    /// there is no database or table yet.
    async fn server_state_format(&self) -> RuntimeResult<Option<u32>>;
    /// Local images of `repository` (no tag or digest) that have a digest,
    /// as digest references (`<repository>@sha256:<hex>`).
    async fn list_images(&self, repository: &str) -> RuntimeResult<Vec<String>>;
    /// Remove the local image `image` (a digest reference). Docker refuses
    /// images a container still uses.
    async fn remove_image(&self, image: &str) -> RuntimeResult<()>;
}

// ------------------------------------------------------------------ DockerCli

/// `docker pull` and the release fetch.
pub const PULL_TIMEOUT: Duration = Duration::from_secs(600);
/// `pg_dump` and `pg_restore`.
pub const BACKUP_TIMEOUT: Duration = Duration::from_secs(1800);
/// Every other compose, inspect and exec call.
pub const CMD_TIMEOUT: Duration = Duration::from_secs(120);
/// Health status poll interval in `wait_healthy`.
pub const HEALTH_POLL: Duration = Duration::from_secs(2);
/// Largest `manifest.json` or bundle accepted from a release image.
pub const MAX_RELEASE_FILE: usize = 1024 * 1024;
/// Largest stdout captured from any other command.
const MAX_CAPTURE: u64 = 4 * 1024 * 1024;
/// Stderr bytes kept for error messages (`stderr_tail` trims to 2 KiB).
const STDERR_KEEP: usize = 4096;

pub const RELEASE_ENV: &str = "release.env";
const SERVICE_SERVER: &str = "server";
const SERVICE_POSTGRES: &str = "postgres";
/// compose.yaml's one-shot provisioner of the server's role and database.
const SERVICE_SERVER_DB_INIT: &str = "server-db-init";
/// The bootstrap PostgreSQL administrator (compose.yaml `POSTGRES_USER`).
const DB_ADMIN: &str = "agent_q";
/// The database the server keeps its state in; owned by the `aq_server` role.
const SERVER_DB: &str = "aq_server";
const STATE_FORMAT_SQL: &str = "SELECT value FROM meta WHERE key = 'state_format_version'";
// `IF EXISTS`: a restore cut short after the drop can be retried.
const DROP_SQL: &str = "DROP DATABASE IF EXISTS aq_server WITH (FORCE)";
const CREATE_SQL: &str = "CREATE DATABASE aq_server OWNER aq_server";

fn tag_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$").expect("static"))
}

fn container_id_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[0-9a-f]{12,64}$").expect("static"))
}

fn fetch_container_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^aq-release-fetch-[0-9a-f]{32}$").expect("static"))
}

/// A fresh name for the temporary container `fetch_release` creates, so it
/// can be removed by name whatever `docker create` printed (or if it never
/// finished).
pub fn fetch_container_name() -> String {
    format!("aq-release-fetch-{}", uuid::Uuid::new_v4().simple())
}

fn fetch_container(name: &str) -> RuntimeResult<&str> {
    if fetch_container_re().is_match(name) {
        Ok(name)
    } else {
        Err(failed(format!(
            "unexpected fetch container name {name:.100?}"
        )))
    }
}

fn failed(m: impl Into<String>) -> RuntimeError {
    RuntimeError::Failed(m.into())
}

fn digest_ref(image: &str) -> RuntimeResult<&str> {
    if aq_release::is_digest_ref(image) {
        Ok(image)
    } else {
        Err(failed(format!(
            "refusing image reference {:.200?}: not digest-pinned (host[:port]/path@sha256:<64 hex>)",
            image
        )))
    }
}

/// A repository name with no tag or digest, as the part of a digest ref
/// before `@`.
fn repository(repo: &str) -> RuntimeResult<&str> {
    if aq_release::is_digest_ref(&format!("{repo}@sha256:{}", "0".repeat(64))) {
        Ok(repo)
    } else {
        Err(failed(format!(
            "refusing repository {repo:.200?}: not host[:port]/path with no tag or digest"
        )))
    }
}

/// Digest refs from `docker image ls --digests` lines; images without a
/// digest (`<none>`) are skipped.
pub fn parse_image_list(out: &[u8]) -> Vec<String> {
    let mut v: Vec<String> = String::from_utf8_lossy(out)
        .lines()
        .map(str::trim)
        .filter(|l| aq_release::is_digest_ref(l))
        .map(str::to_string)
        .collect();
    v.sort();
    v.dedup();
    v
}

fn container_id(id: &str) -> RuntimeResult<&str> {
    if container_id_re().is_match(id) {
        Ok(id)
    } else {
        Err(failed(format!(
            "unexpected container id {id:.100?} from docker"
        )))
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn fsync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) => std::fs::File::open(dir)?.sync_all(),
        None => Ok(()),
    }
}

/// The `AQ_SERVER_IMAGE` in `release.env` while no release is installed:
/// digest-pinned (so it is a valid value) on a reserved `.invalid` host, so
/// Compose can interpolate `compose.yaml` for postgres, psql and pg_dump on
/// a fresh host, while starting the server with it can only fail.
pub const NO_RELEASE_IMAGE: &str = "no-release-installed.invalid/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000000";

fn env_path_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^/[A-Za-z0-9._/+@:-]*$").expect("static"))
}

/// Can `path` be written unquoted as a Compose env-file value? Absolute, and
/// only characters that need no quoting and cannot interpolate (`$`), start
/// a comment (`#`) or add a line.
pub fn is_env_safe_path(path: &Path) -> bool {
    path.to_str().is_some_and(|p| env_path_re().is_match(p))
}

/// The full `release.env`: the server image (digest-pinned, which also rules
/// out anything that could add another variable) and the spool the updater
/// uses, so the server's spool mounts always match it.
fn release_env_body(image: &str, spool: &Path) -> RuntimeResult<String> {
    let image = digest_ref(image)?;
    if !is_env_safe_path(spool) {
        return Err(failed(format!(
            "refusing spool path {:.200?} for release.env: it must be absolute and use only A-Z a-z 0-9 . _ / + @ : -",
            spool
        )));
    }
    Ok(format!(
        "AQ_SERVER_IMAGE={image}\nAQ_UPDATER_SPOOL={}\n",
        spool.display()
    ))
}

/// Write `release.env` (`AQ_SERVER_IMAGE=<image>`, `AQ_UPDATER_SPOOL=<spool>`)
/// to `path` atomically (temp file, fsync, rename, fsync dir), mode 0640.
pub fn write_release_env(path: &Path, image: &str, spool: &Path) -> RuntimeResult<()> {
    write_env_file(path, release_env_body(image, spool)?.as_bytes())
}

fn write_env_file(path: &Path, body: &[u8]) -> RuntimeResult<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let err = |e: std::io::Error| failed(format!("cannot write {}: {e}", path.display()));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(&tmp)
        .map_err(err)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o640))
        .map_err(err)?;
    f.write_all(body).map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(err)?;
    fsync_parent(path).map_err(err)
}

/// The digest-pinned `AQ_SERVER_IMAGE` an existing `release.env` names.
fn release_env_image(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("AQ_SERVER_IMAGE="))
        .filter(|i| aq_release::is_digest_ref(i))
}

/// Make sure `release.env` is complete before Compose reads it. The image
/// it already names is kept (it is the one last started); otherwise `image`
/// (the active release) is used, else [`NO_RELEASE_IMAGE`]. The spool line
/// is always `spool`. A file that is already right is not rewritten.
pub fn ensure_release_env(path: &Path, image: Option<&str>, spool: &Path) -> RuntimeResult<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(failed(format!("cannot read {}: {e}", path.display()))),
    };
    let kept = existing
        .as_deref()
        .and_then(release_env_image)
        // A placeholder gives way to a known active release.
        .filter(|i| *i != NO_RELEASE_IMAGE || image.is_none());
    let body = release_env_body(kept.or(image).unwrap_or(NO_RELEASE_IMAGE), spool)?;
    if existing.as_deref() == Some(body.as_str()) {
        return Ok(());
    }
    write_env_file(path, body.as_bytes())
}

/// The content of the regular file `name` from a `docker cp <id>:/path -`
/// tar stream, at most `MAX_RELEASE_FILE` bytes.
pub fn extract_tar_file(tar_bytes: &[u8], name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut archive = tar::Archive::new(tar_bytes);
    let entries = archive
        .entries()
        .map_err(|e| format!("not a tar stream: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("bad tar stream: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("bad tar entry name: {e}"))?;
        if path.file_name().and_then(|n| n.to_str()) != Some(name) {
            continue;
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(format!("{name} in the release image is not a regular file"));
        }
        if entry.size() > MAX_RELEASE_FILE as u64 {
            return Err(format!(
                "{name} in the release image is larger than {MAX_RELEASE_FILE} bytes"
            ));
        }
        let mut data = Vec::new();
        entry
            .take(MAX_RELEASE_FILE as u64 + 1)
            .read_to_end(&mut data)
            .map_err(|e| format!("cannot read {name} from the tar stream: {e}"))?;
        if data.len() > MAX_RELEASE_FILE {
            return Err(format!(
                "{name} in the release image is larger than {MAX_RELEASE_FILE} bytes"
            ));
        }
        return Ok(data);
    }
    Err(format!("{name} not found in the release image"))
}

/// Interpret `psql -tAc <state format query>`: a missing database or `meta`
/// table means no state format yet.
pub fn parse_state_format(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> RuntimeResult<Option<u32>> {
    if !status.success() {
        let err = String::from_utf8_lossy(stderr);
        let missing = err.contains("database \"aq_server\" does not exist")
            || err.contains("relation \"meta\" does not exist");
        return if missing {
            Ok(None)
        } else {
            Err(failed(failure_text(
                "psql (state format)",
                status,
                stderr,
                true,
            )))
        };
    }
    let text = String::from_utf8_lossy(stdout);
    let value = text.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value.parse().map(Some).map_err(|_| {
        failed(format!(
            "unexpected state_format_version {value:.64?} in the database"
        ))
    })
}

/// The real runtime: the `docker` CLI against the configured Compose project.
/// Every call is a fixed argument vector (no shell), killed when it times
/// out. Image references reach `docker` only as validated digest refs
/// (`pull`, `image inspect`) or through `release.env`; release refs are
/// built from the configured repo and a validated version or channel tag.
#[derive(Debug, Clone)]
pub struct DockerCli {
    docker: std::path::PathBuf,
    compose: crate::config::ComposeConfig,
    release_env: std::path::PathBuf,
    /// Written into `release.env` as `AQ_UPDATER_SPOOL`.
    spool: std::path::PathBuf,
    release_repo: String,
    channel: String,
}

struct Ran {
    /// Run through `docker compose`: errors are sanitized.
    compose: bool,
    success: bool,
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Ran {
    fn ok(self, name: &str) -> RuntimeResult<Vec<u8>> {
        if self.success {
            Ok(self.stdout)
        } else {
            Err(failed(failure_text(
                name,
                &self.status,
                &self.stderr,
                self.compose,
            )))
        }
    }
}

impl DockerCli {
    pub fn new(
        compose: crate::config::ComposeConfig,
        state_dir: &Path,
        release_repo: String,
        channel: String,
    ) -> Self {
        DockerCli {
            docker: "docker".into(),
            compose,
            release_env: state_dir.join(RELEASE_ENV),
            spool: crate::spool::spool_dir(state_dir),
            release_repo,
            channel,
        }
    }

    pub fn from_config(cfg: &crate::config::UpdaterConfig) -> Self {
        Self::new(
            cfg.compose.clone(),
            &cfg.state_dir,
            cfg.release_repo.clone(),
            cfg.channel.clone(),
        )
    }

    /// Use another `docker` binary (tests).
    pub fn with_docker_binary(mut self, docker: std::path::PathBuf) -> Self {
        self.docker = docker;
        self
    }

    pub fn release_env_path(&self) -> &Path {
        &self.release_env
    }

    /// Create or complete `release.env` (see [`ensure_release_env`]) with
    /// `image`, the active release, if there is one. Call before any Compose
    /// command; every Compose call here also does.
    pub fn ensure_release_env(&self, image: Option<&str>) -> RuntimeResult<()> {
        ensure_release_env(&self.release_env, image, &self.spool)
    }

    // ------------------------------------------------- argv (pure, tested)

    /// `compose --project-name <n> --project-directory <d> -f <f>…
    /// --env-file <e>… --env-file <state_dir>/release.env <args>`.
    ///
    /// `release.env` is the last `--env-file`, so its `AQ_SERVER_IMAGE` and
    /// `AQ_UPDATER_SPOOL` override the operator's env files. Variables set in
    /// this process's own environment still beat every env file (Compose's
    /// precedence), so the updater must not run with those two exported.
    pub fn compose_argv(&self, args: &[&str]) -> Vec<OsString> {
        let c = &self.compose;
        let mut v: Vec<OsString> = os(&["compose", "--project-name", &c.project_name]);
        v.push("--project-directory".into());
        v.push(c.project_dir.clone().into());
        for f in &c.files {
            v.push("-f".into());
            v.push(f.clone().into());
        }
        for e in c.env_files.iter().chain(std::iter::once(&self.release_env)) {
            v.push("--env-file".into());
            v.push(e.clone().into());
        }
        v.extend(args.iter().map(OsString::from));
        v
    }

    /// Starts the server's dependencies too (database provisioning, state
    /// initialization, graph, Git); only the server itself is force-recreated,
    /// a dependency is recreated only when its configuration changed.
    pub fn start_server_argv(&self) -> Vec<OsString> {
        self.compose_argv(&["up", "-d", "--force-recreate", SERVICE_SERVER])
    }
    /// Idempotently create the server's role and database (a fresh host has
    /// neither before the first install), refusing retained owner drift.
    pub fn provision_database_argv(&self) -> Vec<OsString> {
        self.compose_argv(&["run", "--rm", "-T", SERVICE_SERVER_DB_INIT])
    }
    pub fn stop_server_argv(&self) -> Vec<OsString> {
        self.compose_argv(&["stop", SERVICE_SERVER])
    }
    pub fn server_container_argv(&self) -> Vec<OsString> {
        self.compose_argv(&["ps", "-q", SERVICE_SERVER])
    }
    pub fn readiness_argv(&self) -> Vec<OsString> {
        self.compose_argv(&["exec", "-T", SERVICE_SERVER, "aq-server", "readiness"])
    }
    fn psql_argv(&self, db: &str, flag: &str, sql: &str) -> Vec<OsString> {
        self.compose_argv(&[
            "exec",
            "-T",
            SERVICE_POSTGRES,
            "psql",
            "-U",
            DB_ADMIN,
            "-d",
            db,
            flag,
            sql,
        ])
    }
    pub fn backup_argv(&self) -> Vec<OsString> {
        self.compose_argv(&[
            "exec",
            "-T",
            SERVICE_POSTGRES,
            "pg_dump",
            "-U",
            DB_ADMIN,
            "-Fc",
            SERVER_DB,
        ])
    }
    /// Drop, create, then `pg_restore` (fed the dump on stdin).
    pub fn restore_argvs(&self) -> [Vec<OsString>; 3] {
        [
            self.psql_argv("postgres", "-c", DROP_SQL),
            self.psql_argv("postgres", "-c", CREATE_SQL),
            self.compose_argv(&[
                "exec",
                "-T",
                SERVICE_POSTGRES,
                "pg_restore",
                "-U",
                DB_ADMIN,
                "-d",
                SERVER_DB,
            ]),
        ]
    }
    pub fn state_format_argv(&self) -> Vec<OsString> {
        self.psql_argv(SERVER_DB, "-tAc", STATE_FORMAT_SQL)
    }

    pub fn pull_argv(&self, image: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&["pull", "--quiet", "--", digest_ref(image)?]))
    }
    pub fn labels_argv(&self, image: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&[
            "image",
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            "--",
            digest_ref(image)?,
        ]))
    }
    pub fn image_list_argv(&self, repo: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&[
            "image",
            "ls",
            "--digests",
            "--no-trunc",
            "--format",
            "{{.Repository}}@{{.Digest}}",
            "--",
            repository(repo)?,
        ]))
    }
    pub fn image_rm_argv(&self, image: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&["image", "rm", "--", digest_ref(image)?]))
    }
    pub fn health_argv(&self, id: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&[
            "inspect",
            "--format",
            "{{.State.Health.Status}}",
            container_id(id)?,
        ]))
    }

    /// `<release_repo>:<version|channel>`, the tag validated as a Docker tag.
    pub fn release_ref(&self, selector: &ReleaseSelector) -> RuntimeResult<String> {
        let tag = match selector {
            ReleaseSelector::Version(v) => v.to_string(),
            ReleaseSelector::Channel => self.channel.clone(),
        };
        if !tag_re().is_match(&tag) {
            return Err(failed(format!("{tag:.140?} is not a valid release tag")));
        }
        Ok(format!("{}:{tag}", self.release_repo))
    }
    /// Only for refs from `release_ref`.
    pub fn release_pull_argv(&self, release_ref: &str) -> Vec<OsString> {
        os(&["pull", "--quiet", "--", release_ref])
    }
    /// Only for refs from `release_ref`; `name` from `fetch_container_name`.
    /// `none` is the (never run) command: the release image is `FROM scratch`.
    pub fn release_create_argv(
        &self,
        name: &str,
        release_ref: &str,
    ) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&[
            "create",
            "--name",
            fetch_container(name)?,
            release_ref,
            "none",
        ]))
    }
    pub fn cp_argv(&self, name: &str, path: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&[
            "cp",
            &format!("{}:{path}", fetch_container(name)?),
            "-",
        ]))
    }
    pub fn rm_argv(&self, name: &str) -> RuntimeResult<Vec<OsString>> {
        Ok(os(&["rm", "-f", fetch_container(name)?]))
    }

    // --------------------------------------------------------- execution

    /// Run `docker <argv>`; stdout goes to `sink` (at most `cap` bytes when
    /// given). Killed on timeout or when dropped.
    async fn exec_into<W>(
        &self,
        name: &str,
        argv: Vec<OsString>,
        timeout: Duration,
        stdin: std::process::Stdio,
        sink: &mut W,
        cap: Option<u64>,
    ) -> RuntimeResult<(std::process::ExitStatus, Vec<u8>, u64)>
    where
        W: tokio::io::AsyncWrite + Unpin + Send,
    {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new(&self.docker)
            .args(&argv)
            .stdin(stdin)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            // Own process group: a Ctrl-C or hangup at the terminal reaches
            // only aq-updater, never this child. Every command that runs
            // docker handles those signals: `run` finishes the step and
            // stops (SIGHUP is ignored), `install` and `restore-previous`
            // print a notice and finish the whole operation. So the outcome
            // of this child is always recorded; only SIGKILL cuts it short,
            // and then recovery resumes from the journal.
            .process_group(0)
            .spawn()
            .map_err(|e| failed(format!("{name}: cannot run {}: {e}", self.docker.display())))?;
        let stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let err_task = tokio::spawn(async move {
            let mut keep = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                keep.extend_from_slice(&buf[..n]);
                if keep.len() > STDERR_KEEP {
                    keep.drain(..keep.len() - STDERR_KEEP);
                }
            }
            keep
        });
        let run = async {
            let copied = match cap {
                Some(c) => tokio::io::copy(&mut stdout.take(c + 1), sink).await,
                None => {
                    let mut stdout = stdout;
                    tokio::io::copy(&mut stdout, sink).await
                }
            };
            let copied = match copied {
                Ok(n) if cap.is_some_and(|c| n > c) => {
                    let _ = child.start_kill();
                    Err(failed(format!(
                        "{name}: output larger than {} bytes",
                        cap.unwrap_or(0)
                    )))
                }
                Ok(n) => Ok(n),
                Err(e) => {
                    let _ = child.start_kill();
                    Err(failed(format!("{name}: cannot read its output: {e}")))
                }
            };
            let status = child
                .wait()
                .await
                .map_err(|e| failed(format!("{name}: cannot wait for docker: {e}")));
            (copied, status)
        };
        let (copied, status) = match tokio::time::timeout(timeout, run).await {
            Ok(r) => r,
            Err(_) => {
                return Err(failed(format!(
                    "{name} timed out after {} s",
                    timeout.as_secs()
                )));
            }
        };
        let stderr = err_task.await.unwrap_or_default();
        let n = copied?;
        Ok((status?, stderr, n))
    }

    async fn exec(
        &self,
        name: &str,
        argv: Vec<OsString>,
        timeout: Duration,
        stdin: std::process::Stdio,
    ) -> RuntimeResult<Ran> {
        let compose = argv.first().is_some_and(|a| a == "compose");
        let mut stdout = Vec::new();
        let (status, stderr, _) = self
            .exec_into(name, argv, timeout, stdin, &mut stdout, Some(MAX_CAPTURE))
            .await?;
        Ok(Ran {
            compose,
            success: status.success(),
            status,
            stdout,
            stderr,
        })
    }

    async fn run(
        &self,
        name: &str,
        argv: Vec<OsString>,
        timeout: Duration,
    ) -> RuntimeResult<Vec<u8>> {
        self.exec(name, argv, timeout, std::process::Stdio::null())
            .await?
            .ok(name)
    }

    /// Compose fails on a missing `--env-file`, and compose.yaml requires
    /// both of its variables: make sure it is complete first.
    fn compose_ready(&self) -> RuntimeResult<()> {
        self.ensure_release_env(None)
    }

    async fn compose(
        &self,
        name: &str,
        argv: Vec<OsString>,
        timeout: Duration,
    ) -> RuntimeResult<Vec<u8>> {
        self.compose_ready()?;
        self.run(name, argv, timeout).await
    }

    /// The server's health status, or why there is none.
    async fn server_health(&self, timeout: Duration) -> RuntimeResult<String> {
        let out = self
            .compose("docker compose ps", self.server_container_argv(), timeout)
            .await?;
        let text = String::from_utf8_lossy(&out);
        let Some(id) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
            return Err(failed("the server container is not running"));
        };
        let out = self
            .run("docker inspect", self.health_argv(id)?, timeout)
            .await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    async fn cp_file(&self, id: &str, name: &str) -> RuntimeResult<Vec<u8>> {
        let tar = self
            .run(
                "docker cp",
                self.cp_argv(id, &format!("/{name}"))?,
                CMD_TIMEOUT,
            )
            .await?;
        extract_tar_file(&tar, name).map_err(failed)
    }
}

#[async_trait]
impl Runtime for DockerCli {
    async fn fetch_release(&self, selector: ReleaseSelector) -> RuntimeResult<(Vec<u8>, Vec<u8>)> {
        let r = self.release_ref(&selector)?;
        self.run("docker pull", self.release_pull_argv(&r), PULL_TIMEOUT)
            .await?;
        let name = fetch_container_name();
        let created = self
            .run(
                "docker create",
                self.release_create_argv(&name, &r)?,
                CMD_TIMEOUT,
            )
            .await;
        let made = created.is_ok();
        let files = match created {
            Ok(_) => {
                async {
                    let manifest = self.cp_file(&name, "manifest.json").await?;
                    let bundle = self.cp_file(&name, "manifest.sigstore.json").await?;
                    Ok((manifest, bundle))
                }
                .await
            }
            Err(e) => Err(e),
        };
        // Always, by name: also after a failed or timed-out create, which may
        // still have left the container behind.
        if let Err(e) = self
            .run("docker rm", self.rm_argv(&name)?, CMD_TIMEOUT)
            .await
        {
            if made {
                tracing::warn!(container = %name, error = %e, "cannot remove the temporary release container");
            } else {
                tracing::debug!(container = %name, error = %e, "no temporary release container to remove");
            }
        }
        files
    }

    async fn pull(&self, image: &str) -> RuntimeResult<()> {
        self.run("docker pull", self.pull_argv(image)?, PULL_TIMEOUT)
            .await
            .map(drop)
    }

    async fn image_labels(&self, image: &str) -> RuntimeResult<HashMap<String, String>> {
        let out = self
            .run(
                "docker image inspect",
                self.labels_argv(image)?,
                CMD_TIMEOUT,
            )
            .await?;
        let labels: Option<HashMap<String, String>> =
            serde_json::from_slice(&out).map_err(|e| {
                failed(format!(
                    "docker image inspect: unexpected labels output: {e}"
                ))
            })?;
        Ok(labels.unwrap_or_default())
    }

    async fn free_bytes(&self, path: &Path) -> RuntimeResult<u64> {
        let st = nix::sys::statvfs::statvfs(path)
            .map_err(|e| failed(format!("statvfs {}: {e}", path.display())))?;
        #[allow(clippy::unnecessary_cast)]
        Ok((st.blocks_available() as u64).saturating_mul(st.fragment_size() as u64))
    }

    async fn backup(&self, dest: &Path) -> RuntimeResult<u64> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use tokio::io::AsyncWriteExt;
        // Before the dump: on a fresh host the server's database does not
        // exist until it is provisioned.
        self.compose(
            "docker compose run server-db-init",
            self.provision_database_argv(),
            CMD_TIMEOUT,
        )
        .await?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dest)
            .map_err(|e| failed(format!("cannot create backup {}: {e}", dest.display())))?;
        let result = async {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| failed(format!("cannot set the mode of {}: {e}", dest.display())))?;
            let mut out = tokio::fs::File::from_std(file);
            let (status, stderr, n) = self
                .exec_into(
                    "pg_dump",
                    self.backup_argv(),
                    BACKUP_TIMEOUT,
                    std::process::Stdio::null(),
                    &mut out,
                    None,
                )
                .await?;
            if !status.success() {
                return Err(failed(failure_text("pg_dump", &status, &stderr, true)));
            }
            if n == 0 {
                return Err(failed("pg_dump produced no output"));
            }
            let sync = |e: std::io::Error| failed(format!("cannot sync {}: {e}", dest.display()));
            out.flush().await.map_err(sync)?;
            out.sync_all().await.map_err(sync)?;
            fsync_parent(dest).map_err(sync)?;
            Ok(n)
        }
        .await;
        if result.is_err() {
            // This call created the file (create_new): remove the partial dump.
            let _ = std::fs::remove_file(dest);
        }
        result
    }

    async fn restore(&self, src: &Path) -> RuntimeResult<()> {
        let [drop_db, create_db, pg_restore] = self.restore_argvs();
        // Opened first: a missing dump must not cost the database.
        let dump = std::fs::File::open(src)
            .map_err(|e| failed(format!("cannot open backup {}: {e}", src.display())))?;
        self.compose("psql (drop database)", drop_db, CMD_TIMEOUT)
            .await?;
        self.compose("psql (create database)", create_db, CMD_TIMEOUT)
            .await?;
        self.compose_ready()?;
        self.exec("pg_restore", pg_restore, BACKUP_TIMEOUT, dump.into())
            .await?
            .ok("pg_restore")
            .map(drop)
    }

    async fn start_server(&self, image: &str) -> RuntimeResult<()> {
        write_release_env(&self.release_env, image, &self.spool)?;
        self.compose("docker compose up", self.start_server_argv(), CMD_TIMEOUT)
            .await
            .map(drop)
    }

    async fn select_release(&self, image: &str) -> RuntimeResult<()> {
        write_release_env(&self.release_env, image, &self.spool)
    }

    async fn stop_server(&self) -> RuntimeResult<()> {
        self.compose("docker compose stop", self.stop_server_argv(), CMD_TIMEOUT)
            .await
            .map(drop)
    }

    async fn wait_healthy(&self, deadline: Duration) -> RuntimeResult<()> {
        let started = tokio::time::Instant::now();
        let mut last;
        loop {
            let left = deadline.saturating_sub(started.elapsed());
            let per_call = CMD_TIMEOUT.min(left.max(Duration::from_secs(1)));
            match self.server_health(per_call).await {
                Ok(s) if s == "healthy" => return Ok(()),
                Ok(s) => last = format!("health {s:?}"),
                Err(RuntimeError::Failed(e)) => last = e,
                Err(e) => return Err(e),
            }
            if started.elapsed() + HEALTH_POLL > deadline {
                return Err(failed(format!(
                    "the server was not healthy within {} s (last: {last})",
                    deadline.as_secs()
                )));
            }
            tokio::time::sleep(HEALTH_POLL).await;
        }
    }

    async fn readiness(&self) -> RuntimeResult<ReadinessReport> {
        self.compose_ready()?;
        let name = "aq-server readiness";
        let ran = self
            .exec(
                name,
                self.readiness_argv(),
                CMD_TIMEOUT,
                std::process::Stdio::null(),
            )
            .await?;
        // A not-ready server still prints its report (and exits 1); the
        // engine judges the report.
        match serde_json::from_slice::<ReadinessReport>(&ran.stdout) {
            Ok(r) => Ok(r),
            Err(e) if ran.success => Err(failed(format!("{name}: unexpected output: {e}"))),
            Err(_) => Err(failed(failure_text(name, &ran.status, &ran.stderr, true))),
        }
    }

    async fn server_state_format(&self) -> RuntimeResult<Option<u32>> {
        self.compose_ready()?;
        let ran = self
            .exec(
                "psql (state format)",
                self.state_format_argv(),
                CMD_TIMEOUT,
                std::process::Stdio::null(),
            )
            .await?;
        parse_state_format(&ran.status, &ran.stdout, &ran.stderr)
    }

    async fn list_images(&self, repository: &str) -> RuntimeResult<Vec<String>> {
        let out = self
            .run(
                "docker image ls",
                self.image_list_argv(repository)?,
                CMD_TIMEOUT,
            )
            .await?;
        Ok(parse_image_list(&out))
    }

    async fn remove_image(&self, image: &str) -> RuntimeResult<()> {
        self.run("docker image rm", self.image_rm_argv(image)?, CMD_TIMEOUT)
            .await
            .map(drop)
    }
}

/// The one stderr line a failure of a command run through `docker compose`
/// may show in history and `status.json`: the first line with `ERROR:` (else
/// the first with `error`, any case), without control characters and at
/// most 200 characters. Lines containing `CONTEXT:` or `=` are never used:
/// PostgreSQL puts row data there, and Compose quotes env files.
pub fn sanitized_stderr(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let usable: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("CONTEXT:") && !l.contains('='))
        .collect();
    let line = usable
        .iter()
        .find(|l| l.contains("ERROR:"))
        .or_else(|| usable.iter().find(|l| l.to_lowercase().contains("error")))?;
    Some(line.chars().filter(|c| !c.is_control()).take(200).collect())
}

/// The error text for a failed command. Compose calls (everything run in
/// the containers, and anything that reads the env files) carry only the
/// sanitized line; their full stderr tail goes to the updater's own log.
fn failure_text(
    name: &str,
    status: &std::process::ExitStatus,
    stderr: &[u8],
    compose: bool,
) -> String {
    if !compose {
        return format!("{name} failed ({status}): {}", stderr_tail(stderr));
    }
    tracing::warn!(command = name, %status, stderr = ?stderr_tail(stderr), "command failed");
    match sanitized_stderr(stderr) {
        Some(line) => format!("{name} failed ({status}): {line}"),
        None => format!("{name} failed ({status})"),
    }
}

/// The last 2 KiB of a child's stderr, for error messages.
pub fn stderr_tail(stderr: &[u8]) -> String {
    const MAX: usize = 2048;
    let mut tail = &stderr[stderr.len().saturating_sub(MAX)..];
    // Do not start in the middle of a UTF-8 sequence.
    while let [first, rest @ ..] = tail
        && (first & 0xC0) == 0x80
    {
        tail = rest;
    }
    let mut s = String::from_utf8_lossy(tail).trim().to_string();
    while s.len() > MAX {
        let mut cut = s.len() - MAX;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ComposeConfig;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    const IMAGE: &str = "ghcr.io/electricjack/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000100";

    fn cli() -> DockerCli {
        DockerCli::new(
            ComposeConfig {
                project_dir: "/opt/agent-q".into(),
                files: vec![
                    "/opt/agent-q/compose.yaml".into(),
                    "/opt/agent-q/compose.prod.yaml".into(),
                ],
                env_files: vec!["/etc/agent-q/compose.env".into()],
                project_name: "agent-q".into(),
            },
            Path::new("/var/lib/agent-q/updater"),
            "ghcr.io/electricjack/agent-q-release".into(),
            "stable".into(),
        )
    }

    fn strs(v: &[OsString]) -> Vec<&str> {
        v.iter().map(|a| a.to_str().unwrap()).collect()
    }

    const PREFIX: [&str; 13] = [
        "compose",
        "--project-name",
        "agent-q",
        "--project-directory",
        "/opt/agent-q",
        "-f",
        "/opt/agent-q/compose.yaml",
        "-f",
        "/opt/agent-q/compose.prod.yaml",
        "--env-file",
        "/etc/agent-q/compose.env",
        "--env-file",
        "/var/lib/agent-q/updater/release.env",
    ];

    fn with_prefix(rest: &[&'static str]) -> Vec<&'static str> {
        PREFIX.iter().chain(rest).copied().collect()
    }

    #[test]
    fn compose_args_are_fixed_vectors() {
        let c = cli();
        assert_eq!(
            c.release_env_path(),
            Path::new("/var/lib/agent-q/updater/release.env")
        );
        assert_eq!(
            strs(&c.start_server_argv()),
            with_prefix(&["up", "-d", "--force-recreate", "server"])
        );
        assert_eq!(
            strs(&c.provision_database_argv()),
            with_prefix(&["run", "--rm", "-T", "server-db-init"])
        );
        assert_eq!(
            strs(&c.stop_server_argv()),
            with_prefix(&["stop", "server"])
        );
        assert_eq!(
            strs(&c.server_container_argv()),
            with_prefix(&["ps", "-q", "server"])
        );
        assert_eq!(
            strs(&c.readiness_argv()),
            with_prefix(&["exec", "-T", "server", "aq-server", "readiness"])
        );
        assert_eq!(
            strs(&c.backup_argv()),
            with_prefix(&[
                "exec",
                "-T",
                "postgres",
                "pg_dump",
                "-U",
                "agent_q",
                "-Fc",
                "aq_server"
            ])
        );
        let [drop, create, restore] = c.restore_argvs();
        assert_eq!(
            strs(&drop),
            with_prefix(&[
                "exec",
                "-T",
                "postgres",
                "psql",
                "-U",
                "agent_q",
                "-d",
                "postgres",
                "-c",
                "DROP DATABASE IF EXISTS aq_server WITH (FORCE)",
            ])
        );
        assert_eq!(
            strs(&create),
            with_prefix(&[
                "exec",
                "-T",
                "postgres",
                "psql",
                "-U",
                "agent_q",
                "-d",
                "postgres",
                "-c",
                "CREATE DATABASE aq_server OWNER aq_server",
            ])
        );
        assert_eq!(
            strs(&restore),
            with_prefix(&[
                "exec",
                "-T",
                "postgres",
                "pg_restore",
                "-U",
                "agent_q",
                "-d",
                "aq_server"
            ])
        );
        assert_eq!(
            strs(&c.state_format_argv()),
            with_prefix(&[
                "exec",
                "-T",
                "postgres",
                "psql",
                "-U",
                "agent_q",
                "-d",
                "aq_server",
                "-tAc",
                "SELECT value FROM meta WHERE key = 'state_format_version'",
            ])
        );
        // The server image never appears as an argument of a Compose call.
        for argv in [
            c.start_server_argv(),
            c.provision_database_argv(),
            c.stop_server_argv(),
            c.readiness_argv(),
            c.backup_argv(),
            c.state_format_argv(),
        ] {
            assert!(
                strs(&argv).iter().all(|a| !a.contains("sha256")),
                "{argv:?}"
            );
        }
        // Without extra env files only the release env is passed.
        let mut bare = cli();
        bare.compose.env_files.clear();
        let argv = bare.stop_server_argv();
        let argv = strs(&argv);
        assert_eq!(argv.iter().filter(|a| **a == "--env-file").count(), 1);
    }

    #[test]
    fn docker_argvs_take_only_validated_refs() {
        let c = cli();
        assert_eq!(
            strs(&c.pull_argv(IMAGE).unwrap()),
            ["pull", "--quiet", "--", IMAGE]
        );
        // A local registry with a port is a valid digest ref.
        let local = IMAGE.replace("ghcr.io/electricjack", "127.0.0.1:5000");
        assert_eq!(
            strs(&c.pull_argv(&local).unwrap()),
            ["pull", "--quiet", "--", local.as_str()]
        );
        assert_eq!(
            strs(&c.labels_argv(IMAGE).unwrap()),
            [
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                "--",
                IMAGE
            ]
        );
        for bad in [
            "ghcr.io/electricjack/agent-q-server:latest",
            "--help",
            "ghcr.io/x@sha256:00",
            &format!("{IMAGE}\nAQ_OTHER=1"),
        ] {
            assert!(c.pull_argv(bad).is_err(), "{bad}");
            assert!(c.labels_argv(bad).is_err(), "{bad}");
        }
        let v = ReleaseSelector::Version("1.2.3".parse().unwrap());
        assert_eq!(
            c.release_ref(&v).unwrap(),
            "ghcr.io/electricjack/agent-q-release:1.2.3"
        );
        assert_eq!(
            c.release_ref(&ReleaseSelector::Channel).unwrap(),
            "ghcr.io/electricjack/agent-q-release:stable"
        );
        // Build metadata is not a valid tag character.
        let v = ReleaseSelector::Version("1.2.3+x".parse().unwrap());
        assert!(c.release_ref(&v).is_err());
        let mut odd = cli();
        odd.channel = "-bad".into();
        assert!(odd.release_ref(&ReleaseSelector::Channel).is_err());

        let name = fetch_container_name();
        assert!(name.starts_with("aq-release-fetch-"), "{name}");
        assert_ne!(name, fetch_container_name());
        assert_eq!(
            strs(
                &c.release_create_argv(&name, "ghcr.io/electricjack/agent-q-release:1.2.3")
                    .unwrap()
            ),
            [
                "create",
                "--name",
                name.as_str(),
                "ghcr.io/electricjack/agent-q-release:1.2.3",
                "none"
            ]
        );
        assert_eq!(
            strs(&c.cp_argv(&name, "/manifest.json").unwrap()),
            ["cp", format!("{name}:/manifest.json").as_str(), "-"]
        );
        assert_eq!(
            strs(&c.rm_argv(&name).unwrap()),
            ["rm", "-f", name.as_str()]
        );
        for bad in ["0123456789ab", "aq-release-fetch-x", "--rm", ""] {
            assert!(c.release_create_argv(bad, "r:1").is_err(), "{bad}");
            assert!(c.cp_argv(bad, "/manifest.json").is_err(), "{bad}");
            assert!(c.rm_argv(bad).is_err(), "{bad}");
        }
        assert_eq!(
            strs(
                &c.image_list_argv("ghcr.io/electricjack/agent-q-server")
                    .unwrap()
            ),
            [
                "image",
                "ls",
                "--digests",
                "--no-trunc",
                "--format",
                "{{.Repository}}@{{.Digest}}",
                "--",
                "ghcr.io/electricjack/agent-q-server"
            ]
        );
        assert_eq!(
            strs(&c.image_rm_argv(IMAGE).unwrap()),
            ["image", "rm", "--", IMAGE]
        );
        for bad in [
            "",
            "--help",
            "ghcr.io/electricjack/agent-q-server:latest",
            IMAGE,
        ] {
            assert!(c.image_list_argv(bad).is_err(), "{bad}");
        }
        assert!(
            c.image_rm_argv("ghcr.io/electricjack/agent-q-server:latest")
                .is_err()
        );
        let listed = format!("{IMAGE}\nghcr.io/electricjack/agent-q-server@<none>\n{IMAGE}\n");
        assert_eq!(parse_image_list(listed.as_bytes()), [IMAGE]);
        let id = "0123456789ab";
        assert_eq!(
            strs(&c.health_argv(id).unwrap()),
            ["inspect", "--format", "{{.State.Health.Status}}", id]
        );
        for bad in [
            "",
            "abc",
            "0123456789AB",
            "0123456789ab:/etc/passwd",
            "--rm",
        ] {
            assert!(c.cp_argv(bad, "/manifest.json").is_err(), "{bad}");
            assert!(c.rm_argv(bad).is_err(), "{bad}");
            assert!(c.health_argv(bad).is_err(), "{bad}");
        }
    }

    const SPOOL: &str = "/var/lib/agent-q/updater/spool";

    fn body(image: &str) -> String {
        format!("AQ_SERVER_IMAGE={image}\nAQ_UPDATER_SPOOL={SPOOL}\n")
    }

    #[test]
    fn release_env_written_atomically_with_mode_0640() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("release.env");
        let spool = Path::new(SPOOL);
        std::fs::write(t.path().join("release.env.tmp"), "stale").unwrap();
        write_release_env(&path, IMAGE, spool).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640, "{mode:o}");
        let names: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![OsString::from("release.env")]);
        // Only digest refs are written: no tags, no injected lines.
        for bad in ["nginx:latest", &format!("{IMAGE}\nAQ_VAULT_DIR=/"), ""] {
            assert!(write_release_env(&path, bad, spool).is_err(), "{bad:?}");
        }
        // Only spool paths Compose reads back verbatim.
        for bad in [
            "rel/spool",
            "/a b",
            "/a$HOME",
            "/a#b",
            "/a\nAQ_VAULT_DIR=/",
            "/a\"b",
        ] {
            let e = write_release_env(&path, IMAGE, Path::new(bad)).unwrap_err();
            assert!(e.to_string().contains("spool path"), "{bad:?}: {e}");
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
    }

    #[test]
    fn no_release_placeholder_is_a_digest_ref_on_a_reserved_host() {
        assert!(aq_release::is_digest_ref(NO_RELEASE_IMAGE));
        assert!(NO_RELEASE_IMAGE.starts_with("no-release-installed.invalid/"));
        assert!(NO_RELEASE_IMAGE.ends_with(&format!("@sha256:{}", "0".repeat(64))));
    }

    #[test]
    fn ensure_release_env_bootstraps_a_fresh_host() {
        let t = tempfile::tempdir().unwrap();
        let spool = Path::new(SPOOL);
        // No release installed: the placeholder image and the spool, so
        // compose.yaml interpolates for postgres, psql and pg_dump.
        let fresh = t.path().join("fresh.env");
        ensure_release_env(&fresh, None, spool).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fresh).unwrap(),
            body(NO_RELEASE_IMAGE)
        );
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640, "{mode:o}");
        // A placeholder gives way to the journal's active release.
        ensure_release_env(&fresh, Some(IMAGE), spool).unwrap();
        assert_eq!(std::fs::read_to_string(&fresh).unwrap(), body(IMAGE));
        // Missing, with an active release.
        let active = t.path().join("active.env");
        ensure_release_env(&active, Some(IMAGE), spool).unwrap();
        assert_eq!(std::fs::read_to_string(&active).unwrap(), body(IMAGE));
        // An empty file (an older updater wrote these) is completed.
        let empty = t.path().join("empty.env");
        std::fs::write(&empty, "").unwrap();
        ensure_release_env(&empty, None, spool).unwrap();
        assert_eq!(
            std::fs::read_to_string(&empty).unwrap(),
            body(NO_RELEASE_IMAGE)
        );
    }

    #[test]
    fn ensure_release_env_keeps_the_image_and_fixes_the_spool() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        let spool = Path::new(SPOOL);
        let path = t.path().join("release.env");
        const OTHER: &str = "ghcr.io/electricjack/agent-q-server@sha256:0000000000000000000000000000000000000000000000000000000000000200";
        // The image last started is kept over the journal's; a missing or
        // different spool line is replaced.
        std::fs::write(&path, format!("AQ_SERVER_IMAGE={OTHER}\n")).unwrap();
        ensure_release_env(&path, Some(IMAGE), spool).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
        std::fs::write(
            &path,
            format!("AQ_SERVER_IMAGE={OTHER}\nAQ_UPDATER_SPOOL=/elsewhere\n"),
        )
        .unwrap();
        ensure_release_env(&path, None, spool).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
        // A complete file is left alone (same inode: not rewritten).
        let ino = std::fs::metadata(&path).unwrap().ino();
        ensure_release_env(&path, Some(IMAGE), spool).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(OTHER));
        // An image line that is not digest-pinned is not trusted.
        std::fs::write(&path, "AQ_SERVER_IMAGE=nginx:latest\n").unwrap();
        ensure_release_env(&path, Some(IMAGE), spool).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body(IMAGE));
    }

    #[test]
    fn docker_cli_writes_the_spool_of_its_state_dir() {
        let t = tempfile::tempdir().unwrap();
        let c = DockerCli::new(
            cli().compose.clone(),
            t.path(),
            "ghcr.io/electricjack/agent-q-release".into(),
            "stable".into(),
        );
        c.ensure_release_env(None).unwrap();
        assert_eq!(
            std::fs::read_to_string(c.release_env_path()).unwrap(),
            format!(
                "AQ_SERVER_IMAGE={NO_RELEASE_IMAGE}\nAQ_UPDATER_SPOOL={}\n",
                t.path().join("spool").display()
            )
        );
    }

    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            b.append_data(&mut h, name, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn extracts_the_named_file_from_a_docker_cp_tar() {
        let t = tar_of(&[("manifest.json", b"{\"a\":1}")]);
        assert_eq!(extract_tar_file(&t, "manifest.json").unwrap(), b"{\"a\":1}");
        // Missing or wrongly named entries fail.
        assert!(extract_tar_file(&t, "manifest.sigstore.json").is_err());
        assert!(extract_tar_file(b"not a tar", "manifest.json").is_err());
        // A symlink is not a file.
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        b.append_link(&mut h, "manifest.json", "/etc/passwd")
            .unwrap();
        let link = b.into_inner().unwrap();
        assert!(extract_tar_file(&link, "manifest.json").is_err());
        // Oversized content is refused.
        let big = vec![b'x'; MAX_RELEASE_FILE + 1];
        let t = tar_of(&[("manifest.json", &big)]);
        assert!(extract_tar_file(&t, "manifest.json").is_err());
    }

    #[test]
    fn missing_database_or_table_is_no_state_format() {
        use std::os::unix::process::ExitStatusExt;
        let ok = std::process::ExitStatus::from_raw(0);
        let bad = std::process::ExitStatus::from_raw(2 << 8);
        assert_eq!(parse_state_format(&ok, b"3\n", b"").unwrap(), Some(3));
        assert_eq!(parse_state_format(&ok, b"", b"").unwrap(), None);
        assert_eq!(
            parse_state_format(&bad, b"", b"psql: error: connection to server on socket \"/var/run/postgresql/.s.PGSQL.5432\" failed: FATAL:  database \"aq_server\" does not exist").unwrap(),
            None
        );
        assert_eq!(
            parse_state_format(
                &bad,
                b"",
                b"ERROR:  relation \"meta\" does not exist\nLINE 1: SELECT value FROM meta"
            )
            .unwrap(),
            None
        );
        assert!(parse_state_format(&bad, b"", b"service \"postgres\" is not running").is_err());
        assert!(parse_state_format(&ok, b"three", b"").is_err());
    }

    #[tokio::test]
    async fn failures_name_the_command_and_keep_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let fake = t.path().join("docker");
        crate::test_support::write_stub(&fake, "#!/bin/sh\necho \"boom: $1\" >&2\nexit 7\n");
        let c = cli().with_docker_binary(fake.clone());
        let e = c.pull(IMAGE).await.unwrap_err();
        let RuntimeError::Failed(m) = e else {
            panic!("{e:?}")
        };
        assert!(m.contains("docker pull") && m.contains("boom: pull"), "{m}");
        let e = c.pull("nginx:latest").await.unwrap_err();
        assert!(matches!(e, RuntimeError::Failed(_)), "{e:?}");

        // backup streams stdout into a new 0600 file and fsyncs it.
        crate::test_support::write_stub(&fake, "#!/bin/sh\nprintf 'PGDMP-data'\n");
        let dest = t.path().join("b.dump");
        let c = DockerCli {
            release_env: t.path().join("release.env"),
            ..cli().with_docker_binary(fake.clone())
        };
        assert_eq!(c.backup(&dest).await.unwrap(), 10);
        assert_eq!(std::fs::read(&dest).unwrap(), b"PGDMP-data");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // ...and never touches an existing file.
        assert!(c.backup(&dest).await.is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"PGDMP-data");
        // A failing dump removes the partial file it created.
        crate::test_support::write_stub(
            &fake,
            "#!/bin/sh\nprintf 'partial'\necho 'pg_dump: error: disk full' >&2\nexit 1\n",
        );
        let dest2 = t.path().join("c.dump");
        let e = c.backup(&dest2).await.unwrap_err();
        assert!(
            e.to_string().contains("pg_dump") && e.to_string().contains("disk full"),
            "{e}"
        );
        assert!(!dest2.exists());
    }

    #[tokio::test]
    async fn fetch_release_removes_the_container_even_on_error() {
        let t = tempfile::tempdir().unwrap();
        let fake = t.path().join("docker");
        let log = t.path().join("log");
        // Every call is logged; `create` and `cp` behave per case.
        let script = |create: &str, cp: &str| {
            format!(
                "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$1\" in\n  create) {create} ;;\n  cp) {cp} ;;\nesac\n",
                log = log.display()
            )
        };
        let cases = [
            // cp fails after a good create.
            (
                script("echo 0123456789abcdef", "echo 'no such file' >&2; exit 1"),
                "docker cp",
                true,
            ),
            // create fails: the container may still exist; remove it by name.
            (
                script("echo 'conflict' >&2; exit 1", "exit 1"),
                "docker create",
                false,
            ),
        ];
        for (body, failing, reaches_cp) in cases {
            let _ = std::fs::remove_file(&log);
            crate::test_support::write_stub(&fake, &body);
            let c = cli().with_docker_binary(fake.clone());
            let e = c.fetch_release(ReleaseSelector::Channel).await.unwrap_err();
            assert!(e.to_string().contains(failing), "{e}");
            let calls = std::fs::read_to_string(&log).unwrap();
            let calls: Vec<&str> = calls.lines().collect();
            assert_eq!(
                calls[0],
                "pull --quiet -- ghcr.io/electricjack/agent-q-release:stable"
            );
            let name = calls[1]
                .strip_prefix("create --name ")
                .and_then(|r| r.strip_suffix(" ghcr.io/electricjack/agent-q-release:stable none"))
                .unwrap_or_else(|| panic!("{calls:?}"));
            assert!(name.starts_with("aq-release-fetch-"), "{calls:?}");
            let mut want = vec![calls[0].to_string(), calls[1].to_string()];
            if reaches_cp {
                want.push(format!("cp {name}:/manifest.json -"));
            }
            want.push(format!("rm -f {name}"));
            assert_eq!(calls, want);
        }
    }

    #[test]
    fn compose_stderr_is_reduced_to_the_first_error_line() {
        let pg_restore = b"pg_restore: connecting to database for restore\n\
pg_restore: error: COPY failed for table \"users\": ERROR:  invalid input syntax for type uuid\n\
CONTEXT:  COPY users, line 3: \"42\talice@example.com\tsecret-hash\"\n";
        let got = sanitized_stderr(pg_restore).unwrap();
        assert_eq!(
            got,
            "pg_restore: error: COPY failed for table \"users\": ERROR:  invalid input syntax for type uuid"
        );
        // Lines with `=` (env files, key=value detail) and CONTEXT are never used.
        let env = b"failed to read /etc/agent-q/compose.env: line 2: error in PASSWORD=hunter2\n\
CONTEXT: error near row 7\n";
        assert_eq!(sanitized_stderr(env), None);
        // ERROR: wins over a later lowercase match, and the line is bounded.
        let long = format!("noise\nERROR:  {}\nanother error\n", "x".repeat(500));
        let got = sanitized_stderr(long.as_bytes()).unwrap();
        assert!(
            got.starts_with("ERROR:  xxx") && got.chars().count() == 200,
            "{got}"
        );
        // Control characters do not survive.
        assert_eq!(
            sanitized_stderr(b"psql: error: bad \x1b[31mthing\n").unwrap(),
            "psql: error: bad [31mthing"
        );
        assert_eq!(sanitized_stderr(b"nothing useful here\n"), None);
        assert_eq!(sanitized_stderr(b""), None);
    }

    #[tokio::test]
    async fn compose_failures_do_not_leak_stderr_into_the_error() {
        let t = tempfile::tempdir().unwrap();
        let fake = t.path().join("docker");
        crate::test_support::write_stub(
            &fake,
            "#!/bin/sh\necho 'pg_restore: error: could not execute query: ERROR:  duplicate key' >&2\n\
echo 'DETAIL:  Key (email)=(alice@example.com) already exists.' >&2\n\
echo 'CONTEXT:  COPY users, line 1: \"alice@example.com\"' >&2\nexit 1\n",
        );
        let c = DockerCli {
            release_env: t.path().join("release.env"),
            ..cli().with_docker_binary(fake.clone())
        };
        let dump = t.path().join("a.dump");
        std::fs::write(&dump, b"PGDMP").unwrap();
        for e in [
            c.restore(&dump).await.unwrap_err().to_string(),
            c.stop_server().await.unwrap_err().to_string(),
            c.server_state_format().await.unwrap_err().to_string(),
            c.backup(&t.path().join("b.dump"))
                .await
                .unwrap_err()
                .to_string(),
        ] {
            assert!(!e.contains("alice"), "{e}");
            assert!(e.contains("ERROR:  duplicate key"), "{e}");
            assert!(e.contains("failed (exit status: 1)"), "{e}");
        }
        // An env-file error: nothing from stderr reaches the error.
        crate::test_support::write_stub(
            &fake,
            "#!/bin/sh\necho 'failed to read compose.env: line 2: unexpected character in \"SECRET=hunter2\"' >&2\nexit 15\n",
        );
        let e = c.stop_server().await.unwrap_err().to_string();
        assert_eq!(e, "docker compose stop failed (exit status: 15)");
        // Plain docker calls (registry errors) keep their stderr tail.
        let e = c.pull(IMAGE).await.unwrap_err().to_string();
        assert!(e.contains("SECRET"), "{e}");
    }

    #[test]
    fn release_env_path_is_under_the_state_dir() {
        let c = cli();
        assert_eq!(
            c.release_env_path(),
            PathBuf::from("/var/lib/agent-q/updater/release.env")
        );
    }

    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_end() {
        let mut long = vec![b'a'; 5000];
        long.extend_from_slice(b"the end\n");
        let t = stderr_tail(&long);
        assert!(t.len() <= 2048);
        assert!(t.ends_with("the end"));
        // A multi-byte character cut at the boundary is dropped, not mangled.
        let mut s = "é".repeat(2000).into_bytes();
        s.push(b'!');
        let t = stderr_tail(&s);
        assert!(t.len() <= 2048 && t.ends_with('!') && !t.contains('\u{FFFD}'));
    }
}
