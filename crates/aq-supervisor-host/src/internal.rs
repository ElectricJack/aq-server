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
use aq_internal_work::{Claimed, Reason, Usage, Way, store, validate};
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
///
/// `now_ms` is the claim time. The run, its preparation included, is stopped
/// at [`Claimed::stop_at_ms`], counted from the claim exactly as the sweep
/// counts: the native's session is killed and the failure recorded first,
/// and only then are the run directories removed and a login copied back.
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
    // At `now_ms`: the claim records it as `claimed_at_ms`.
    let started = Instant::now();
    let Some(claimed) = store::claim(pool, Way::Login, &configured, runner, now_ms).await? else {
        return Ok(false);
    };
    let finished = || {
        let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        now_ms.saturating_add(elapsed)
    };
    let stop_after = u64::try_from(claimed.stop_at_ms() - now_ms).unwrap_or(0);
    let session = Session::default();
    let run = run_candidate(config, &claimed, &session);
    tokio::pin!(run);
    let ran = tokio::select! {
        biased;
        result = &mut run => Some(result),
        () = tokio::time::sleep_until((started + Duration::from_millis(stop_after)).into()) => None,
    };
    let (id, index) = (claimed.id, claimed.candidate_index);
    let Some(result) = ran else {
        session.stop();
        let harness = claimed.candidate.harness.as_deref().unwrap_or("native");
        let detail = claimed.unanswered(harness);
        let reason = Reason::DeadlineExceeded;
        let reported = store::fail_candidate(pool, id, index, reason, &detail, finished()).await;
        // The killed run still reaps the native, copies a refreshed login
        // back and removes its directories; its outcome is not used.
        let _ = run.await;
        reported?;
        return Ok(true);
    };
    let request = &claimed.request;
    let checked = result.and_then(|(raw, usage)| {
        validate::answer(&request.answer, &raw, request.max_output_bytes)
            .map(|answer| (answer, usage))
            .map_err(|detail| (Reason::InvalidOutput, detail))
    });
    let finished = finished();
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

/// Runs `claimed` on the harness its candidate names, in `session`.
async fn run_candidate(
    config: &InternalConfig,
    claimed: &Claimed,
    session: &Session,
) -> Result<(String, Option<Usage>), Failure> {
    let (homes, scratch, identity) = (&config.homes, &config.scratch, config.identity);
    let harness = claimed.candidate.harness.as_deref();
    match (harness, &config.codex, &config.claude) {
        _ if claimed.budget_ms() == 0 => Err(failure(
            Reason::DeadlineExceeded,
            "the task deadline passed before the run",
        )),
        (Some("codex"), Some(h), _) => {
            codex::codex(h, homes, scratch, claimed, identity, session).await
        }
        (Some("claude"), _, Some(h)) => {
            claude::claude(h, homes, scratch, claimed, identity, session).await
        }
        _ => Err(failure(
            Reason::ProviderUnavailable,
            "no runner for this harness on this host",
        )),
    }
}

/// Longest failure `detail`, in bytes; it is stored in the task row.
const DETAIL_BYTES: usize = 500;
/// The stdout read from a native; the rest is drained unread.
const STDOUT_BYTES: usize = 8 * 1024 * 1024;

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

/// How long a native's stdout may stay open after the native has exited and
/// its session was killed: a grandchild that left the session can hold the
/// pipe no longer than this.
const DRAIN_MS: u64 = 500;

/// The process session a native leads, so that [`tick`] can stop the run
/// from outside it. A stop before the spawn keeps the native from starting.
#[derive(Default)]
struct Session(std::sync::Mutex<SessionState>);

#[derive(Default)]
enum SessionState {
    #[default]
    NotStarted,
    Running(i32),
    Stopped,
}

impl Session {
    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spawn(
        &self,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        let mut state = self.state();
        if matches!(*state, SessionState::Stopped) {
            return Err(std::io::Error::other("the run was stopped"));
        }
        let child = command.spawn()?;
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        *state = pid.map_or(SessionState::Stopped, SessionState::Running);
        Ok(child)
    }

    /// Kills every process left in the session, and keeps a native that has
    /// not started from starting.
    fn stop(&self) {
        let mut state = self.state();
        if let SessionState::Running(pid) = *state {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        *state = SessionState::Stopped;
    }
}

/// Spawns `command` in `session`, writes `prompt` to its stdin, reads its
/// stdout and waits for it to exit. Once it has exited, whatever is left of
/// its session is killed and stdout read for at most [`DRAIN_MS`] more, so a
/// grandchild holding the pipe cannot hold up the result. [`Session::stop`]
/// ends the run the same way.
async fn execute(
    command: std::process::Command,
    prompt: String,
    session: &Session,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = session.spawn(&mut command)?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let write = async move {
        if let Some(mut stdin) = stdin {
            // A child that exits without reading is judged by its status.
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
    };
    tokio::pin!(write);
    let (mut bytes, mut chunk) = (Vec::new(), vec![0u8; 64 * 1024]);
    let (mut written, mut status, mut drain_until) = (false, None, None);
    while status.is_none() || stdout.is_some() {
        let drained =
            tokio::time::sleep_until(drain_until.unwrap_or_else(tokio::time::Instant::now));
        tokio::select! {
            () = &mut write, if !written => written = true,
            read = read_some(&mut stdout, &mut chunk), if stdout.is_some() => match read {
                Ok(0) | Err(_) => stdout = None,
                Ok(n) => {
                    // Past the limit the rest is drained unread.
                    let room = STDOUT_BYTES.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&chunk[..n.min(room)]);
                }
            },
            exited = child.wait(), if status.is_none() => {
                session.stop();
                status = Some(exited?);
                let drain = Duration::from_millis(DRAIN_MS);
                drain_until = Some(tokio::time::Instant::now() + drain);
            },
            () = drained, if drain_until.is_some() => break,
        }
    }
    let status = status.expect("the loop ends only after the native has exited");
    Ok((status, bytes))
}

/// The next bytes of `out`; none once it is closed.
async fn read_some(
    out: &mut Option<tokio::process::ChildStdout>,
    chunk: &mut [u8],
) -> std::io::Result<usize> {
    match out {
        Some(out) => out.read(chunk).await,
        None => Ok(0),
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
