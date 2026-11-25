//! The boundary between the update engine and the host: Docker, Compose and
//! the database container. `Runtime` is the trait the engine drives;
//! `DockerCli` implements it with the `docker` CLI.

mod diagnostics;
mod process;
mod release_env;
mod release_image;

pub use diagnostics::{parse_state_format, sanitized_stderr, stderr_tail};
pub use release_env::{
    NO_RELEASE_IMAGE, RELEASE_ENV, ensure_release_env, is_env_safe_path, write_release_env,
};
pub use release_image::{extract_tar_file, fetch_container_name, parse_image_list};

use diagnostics::failure_text;
use process::Invocation;
use release_image::{container_id, digest_ref, fetch_container, repository, tag_re};

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

fn failed(m: impl Into<String>) -> RuntimeError {
    RuntimeError::Failed(m.into())
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
                    Invocation::new("pg_dump", self.backup_argv(), BACKUP_TIMEOUT),
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
        self.exec(Invocation::new("pg_restore", pg_restore, BACKUP_TIMEOUT).with_stdin(dump.into()))
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
            .exec(Invocation::new(name, self.readiness_argv(), CMD_TIMEOUT))
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
            .exec(Invocation::new(
                "psql (state format)",
                self.state_format_argv(),
                CMD_TIMEOUT,
            ))
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

#[cfg(test)]
mod tests;
