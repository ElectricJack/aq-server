//! `internal` mode (spec 0012 §4, §6): claims internal tasks whose current
//! candidate is a logged-in agent and runs each once, non-interactively.
//!
//! Internal work takes no worker slot and is outside swarm admission: nothing
//! here reads swarm settings or admits a supervisor generation.
mod claude;
mod codex;

use crate::{
    config::protected_read,
    subscription::{Identity, verify_native},
};
use anyhow::{Result, ensure};
use aq_internal_work::{CANDIDATE_TIMEOUT_MS, Reason, Way, store, validate};
use aq_session_protocol::Digest;
use serde::Deserialize;
use sqlx::PgPool;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalConfig {
    pub database_url: String,
    /// Parent of the per-run native homes; root-owned, not group or world writable.
    pub homes: PathBuf,
    /// Parent of the per-run empty working directories; same ownership as `homes`.
    pub scratch: PathBuf,
    pub poll_ms: u64,
    pub codex: Option<Harness>,
    pub claude: Option<Harness>,
    #[serde(skip)]
    identity: Identity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    pub native: PathBuf,
    pub native_digest: Digest,
    /// The login file. Codex: an `auth.json`; each copy in and back holds a
    /// `flock` on its directory, which needs to be writable only for a
    /// refresh to be written back. Claude: the one-line setup token, read
    /// only and passed to the native in its environment.
    pub secret_file: PathBuf,
}

impl InternalConfig {
    /// Reads `path` with the checks `Config::load` applies: an absolute,
    /// canonical, owner-only regular file of at most 64 KiB, no unknown
    /// fields, absolute installed paths, and no login file under a directory
    /// the native child is given. Returns the file's digest too.
    pub fn load(path: &Path) -> Result<(Self, Digest)> {
        let bytes = protected_read(path, 65536)?;
        let config: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            (1..=60_000).contains(&config.poll_ms),
            "poll_ms must be between 1 and 60000"
        );
        let harnesses = [&config.codex, &config.claude];
        let mut paths = vec![&config.homes, &config.scratch];
        for h in harnesses.iter().copied().flatten() {
            paths.extend([&h.native, &h.secret_file]);
        }
        ensure!(
            paths.iter().all(|p| p.is_absolute()),
            "absolute installed paths required"
        );
        ensure!(
            harnesses.iter().copied().flatten().all(|h| {
                !h.secret_file.starts_with(&config.homes)
                    && !h.secret_file.starts_with(&config.scratch)
            }),
            "login files overlap the native directories"
        );
        Ok((config, Digest::of(&bytes)))
    }

    /// Runs the native as the current user, without the privilege drop or
    /// chown, so tests that are not root can drive a fake native.
    #[cfg(feature = "test-support")]
    pub fn for_tests(mut self) -> Self {
        self.identity = Identity::current();
        self
    }
}

/// Runs the claim loop for the config at `path`. Returns once the file is
/// removed or its content changes, so a supervisor can restart it with the
/// new config.
pub async fn run(path: &Path) -> Result<()> {
    unsafe {
        libc::umask(0o077);
    }
    let (config, digest) = InternalConfig::load(path)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&config.database_url)
        .await?;
    let runner = format!("internal-host:{}", hostname()?);
    loop {
        let claimed = match tick(&config, &pool, &runner, now_ms()?).await {
            Ok(claimed) => claimed,
            Err(_) => {
                eprintln!("internal work: tick failed");
                false
            }
        };
        if !unchanged(path, &digest)? {
            return Ok(());
        }
        if !claimed {
            tokio::time::sleep(Duration::from_millis(config.poll_ms)).await;
        }
    }
}

/// Claims at most one queued login task for a harness this host has
/// configured, runs it, and records the answer or the candidate's failure.
/// Returns whether it claimed. One task at a time: the next claim waits for
/// this run to end.
pub async fn tick(
    config: &InternalConfig,
    pool: &PgPool,
    runner: &str,
    now_ms: i64,
) -> Result<bool> {
    let configured: Vec<&str> = [("codex", &config.codex), ("claude", &config.claude)]
        .into_iter()
        .filter_map(|(name, harness)| harness.as_ref().map(|_| name))
        .collect();
    if configured.is_empty() {
        return Ok(false);
    }
    let Some(claimed) = store::claim(pool, Way::Login, &configured, runner, now_ms).await? else {
        return Ok(false);
    };
    let started = Instant::now();
    // The same per-candidate budget as the in-server API loop.
    let budget = (claimed.deadline_at_ms - now_ms).clamp(0, CANDIDATE_TIMEOUT_MS);
    let (homes, scratch, identity) = (&config.homes, &config.scratch, config.identity);
    let duration = Duration::from_millis(budget as u64);
    let result = match (
        claimed.candidate.harness.as_deref(),
        &config.codex,
        &config.claude,
    ) {
        _ if budget == 0 => Err((
            Reason::DeadlineExceeded,
            "the task deadline passed before the run".into(),
        )),
        (Some("codex"), Some(h), _) => {
            codex::codex(h, homes, scratch, &claimed, identity, duration).await
        }
        (Some("claude"), _, Some(h)) => {
            claude::claude(h, homes, scratch, &claimed, identity, duration).await
        }
        _ => Err((
            Reason::ProviderUnavailable,
            "no runner for this harness on this host".into(),
        )),
    };
    let request = &claimed.request;
    let checked = result.and_then(|(raw, usage)| {
        validate::answer(&request.answer, &raw, request.max_output_bytes)
            .map(|answer| (answer, usage))
            .map_err(|detail| (Reason::InvalidOutput, detail))
    });
    let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let finished = now_ms.saturating_add(elapsed);
    let (id, index) = (claimed.id, claimed.candidate_index);
    match checked {
        Ok((answer, usage)) => {
            store::complete(pool, id, index, &answer, usage.as_ref(), finished).await
        }
        Err((reason, detail)) => {
            store::fail_candidate(pool, id, index, reason, &detail, finished).await
        }
    }?;
    Ok(true)
}

/// Longest failure `detail`, in bytes; it is stored in the task row.
const DETAIL_BYTES: usize = 500;
/// The stdout read from a native; the rest is drained unread.
const STDOUT_BYTES: u64 = 8 * 1024 * 1024;

type Failure = (Reason, String);

fn failure(reason: Reason, detail: &str) -> Failure {
    (reason, detail.into())
}

/// Checks the run directories' parents and the native of `h`, makes a fresh
/// private home under `homes` and an empty working directory under `scratch`
/// (owned by the native's user), and runs `attempt` with the verified
/// executable, the home and the directory. Both are removed afterwards,
/// whatever the outcome.
async fn in_run_dirs<T>(
    h: &Harness,
    homes: &Path,
    scratch: &Path,
    identity: Identity,
    attempt: impl AsyncFnOnce(&std::fs::File, &Path, &Path) -> Result<T, Failure>,
) -> Result<T, Failure> {
    for parent in [homes, scratch] {
        private_parent(parent, identity)
            .map_err(|_| failure(Reason::ProviderError, "internal work directories rejected"))?;
    }
    let executable = verify_native(&h.native, &h.native_digest, identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "native executable refused"))?;
    let name = uuid::Uuid::new_v4().to_string();
    let (home, cwd) = (homes.join(&name), scratch.join(&name));
    let made = run_dirs(&home, &cwd, identity)
        .map_err(|_| failure(Reason::ProviderError, "native run could not be prepared"));
    let result = match made {
        Ok(()) => attempt(&executable, &home, &cwd).await,
        Err(failure) => Err(failure),
    };
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);
    result
}

/// Creates `home` and `cwd` owner-only; `cwd` belongs to the native's user.
fn run_dirs(home: &Path, cwd: &Path, identity: Identity) -> std::io::Result<()> {
    for dir in [home, cwd] {
        std::fs::create_dir(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        std::os::unix::fs::chown(cwd, id, id)?;
    }
    Ok(())
}

/// `dir` is a canonical directory owned by the executable owner and not
/// writable by group or others, so nobody else can plant a run directory.
fn private_parent(dir: &Path, identity: Identity) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    anyhow::ensure!(
        meta.is_dir()
            && meta.uid() == identity.executable_owner
            && meta.mode() & 0o022 == 0
            && dir.canonicalize()? == dir,
        "internal work directory ownership"
    );
    Ok(())
}

enum Exit {
    Finished(std::process::ExitStatus, Vec<u8>),
    TimedOut,
}

/// Spawns `command`, writes `prompt` to its stdin, reads its stdout and waits
/// for it, all within `budget`. At the budget the child's whole session (it
/// leads one) is killed and reaped.
async fn execute(
    command: std::process::Command,
    prompt: String,
    budget: Duration,
) -> std::io::Result<Exit> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let run = async {
        let write = async move {
            if let Some(mut stdin) = stdin {
                // A child that exits without reading is judged by its status.
                let _ = stdin.write_all(prompt.as_bytes()).await;
            }
        };
        let read = async {
            let mut bytes = Vec::new();
            if let Some(out) = stdout.as_mut() {
                let _ = (&mut *out).take(STDOUT_BYTES).read_to_end(&mut bytes).await;
                let _ = tokio::io::copy(out, &mut tokio::io::sink()).await;
            }
            bytes
        };
        let ((), bytes, status) = tokio::join!(write, read, child.wait());
        status.map(|status| Exit::Finished(status, bytes))
    };
    match tokio::time::timeout(budget, run).await {
        Ok(finished) => finished,
        Err(_) => {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.wait().await;
            Ok(Exit::TimedOut)
        }
    }
}

/// An authentication or usage-limit failure: the login cannot serve now.
/// Codex and Claude Code wordings both.
fn unavailable(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "usage limit",
        "usage_limit",
        "rate limit",
        "limit reached",
        "hit your limit",
        "quota",
        "unauthorized",
        "401",
        "authenticat",
        "log in",
        "login",
        "sign in",
        "token expired",
        "token_expired",
        "refresh token",
        "oauth token",
        "invalid api key",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// `text` cut to at most `limit` bytes at a character boundary.
fn clip(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Whether the config at `path` still has `digest`. A removed file is a change.
fn unchanged(path: &Path, digest: &Digest) -> Result<bool> {
    match protected_read(path, 65536) {
        Ok(bytes) => Ok(Digest::of(&bytes) == *digest),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn now_ms() -> Result<i64> {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    Ok(i64::try_from(since.as_millis())?)
}

fn hostname() -> Result<String> {
    let mut name = [0u8; 256];
    ensure!(
        unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } == 0,
        "hostname unavailable"
    );
    let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
    Ok(String::from_utf8_lossy(&name[..end]).into_owned())
}
