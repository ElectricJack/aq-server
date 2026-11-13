//! `internal` mode (spec 0012 §4, §6): claims internal tasks whose current
//! candidate is a logged-in agent and runs each once, non-interactively.
//!
//! Agent work (spec 0013 §4.2) runs the harness with tools in a workspace
//! holding the request's files until it writes its outcome ([`agent`]).
//!
//! Internal work takes no worker slot and is outside swarm admission: nothing
//! here reads swarm settings or admits a supervisor generation.
mod agent;
mod claude;
mod codex;

use crate::{
    config::protected_read,
    subscription::{Identity, file_digest, open_native, recover_login},
};
use anyhow::{Result, ensure};
use aq_internal_work::{
    Claimed, DEADLINE_PASSED, MIN_CANDIDATE_BUDGET_MS, Reason, Usage, store, validate,
};
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
    /// The native's file when its digest last matched.
    #[serde(skip)]
    verified: std::sync::Mutex<Option<Stamp>>,
    /// How many of the next runs panic once the native has run (tests only).
    #[cfg(feature = "test-support")]
    #[serde(skip)]
    pub(crate) panics: std::sync::atomic::AtomicUsize,
}

/// What shows a change to a file's content without reading it: its device,
/// inode, size, mtime and ctime. Every write moves the ctime, and no one can
/// set it back.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    fn of(m: &std::fs::Metadata) -> Self {
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }
}

impl Harness {
    /// The native, opened for exec through its descriptor and checked
    /// against `native_digest`. It is hashed when the config is loaded and
    /// again only when its [`Stamp`] differs from the last match.
    fn verified_native(&self, identity: Identity) -> Result<std::fs::File> {
        let executable = open_native(&self.native, identity)?;
        let stamp = Stamp::of(&executable.metadata()?);
        let verified = || self.verified.lock().unwrap_or_else(|p| p.into_inner());
        // Not held while hashing, so another run is not blocked meanwhile.
        let known = *verified();
        if known != Some(stamp) {
            let matches = file_digest(&executable)? == self.native_digest.as_str();
            *verified() = matches.then_some(stamp);
            ensure!(matches, "native digest mismatch");
        }
        Ok(executable)
    }
}

impl InternalConfig {
    /// Reads `path` with the checks `Config::load` applies: an absolute,
    /// canonical, owner-only regular file of at most 64 KiB, no unknown
    /// fields, absolute installed paths, and no login file under a directory
    /// the native child is given. Returns the file's digest too. Each native
    /// is hashed here once; a native that does not match fails its runs.
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
        for h in harnesses.iter().copied().flatten() {
            if h.verified_native(config.identity).is_err() {
                eprintln!("internal work: a native executable was refused");
            }
        }
        Ok((config, Digest::of(&bytes)))
    }

    /// Runs the native as the current user, without the privilege drop or
    /// chown, so tests that are not root can drive a fake native.
    #[cfg(feature = "test-support")]
    pub fn for_tests(mut self) -> Self {
        self.identity = Identity::current();
        self
    }

    /// Makes the next `runs` Codex runs panic once the native has run, before
    /// anything is copied back.
    #[cfg(feature = "test-support")]
    pub fn panic_in_codex_runs(self, runs: usize) -> Self {
        if let Some(codex) = &self.codex {
            codex
                .panics
                .store(runs, std::sync::atomic::Ordering::SeqCst);
        }
        self
    }
}

/// How many login tasks one host runs at once, so a long run does not keep
/// the next task waiting. Bounded by memory: the `internal-work` service has
/// 1 GiB, and one Claude Code native takes a few hundred MB.
pub const MAX_RUNS: usize = 2;

/// How long a native has to exit after SIGTERM at its stop before SIGKILL:
/// Codex may be writing a refreshed login.
const TERM_GRACE_MS: u64 = 1_000;

/// Runs the claim loop for the config at `path`. Returns once the file is
/// removed or its content changes and the runs in progress have ended, so a
/// supervisor can restart it with the new config.
pub async fn run(path: &Path) -> Result<()> {
    unsafe {
        libc::umask(0o077);
    }
    let (config, digest) = InternalConfig::load(path)?;
    recover_run_dirs(&config);
    // A statement that hangs ends instead of holding a run, its slot and
    // the Codex login.
    let options: sqlx::postgres::PgConnectOptions = config.database_url.parse()?;
    let options = options.options([("statement_timeout", "30000")]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(MAX_RUNS as u32 + 1)
        .connect_with(options)
        .await?;
    let runner = format!("internal-host:{}", hostname()?);
    let config = std::sync::Arc::new(config);
    serve(config, pool, runner, || unchanged(path, &digest)).await
}

/// At start, before anything runs (one host owns both directories):
/// removes the run directories, named by a UUID, that a host which stopped
/// mid-run left under `homes` and `scratch`. Anything else there is left
/// alone. A Codex login such a run refreshed is first offered back to the
/// login file ([`recover_login`]); a home whose newer login could not be
/// copied back, or that holds a login while Codex is not configured, is kept.
pub fn recover_run_dirs(config: &InternalConfig) {
    for parent in [&config.homes, &config.scratch] {
        if private_parent(parent, config.identity).is_err() {
            continue;
        }
        for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            if !is_run_dir(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let login = path.join("auth.json");
            if parent == &config.homes && std::fs::symlink_metadata(&login).is_ok() {
                // Without Codex configured there is no login file to check it
                // against, so it is kept too.
                let checked = config.codex.as_ref().is_some_and(|codex| {
                    recover_login(&codex.secret_file, &login, config.identity)
                });
                if !checked {
                    eprintln!("internal work: kept a run home holding a login not copied back");
                    continue;
                }
            }
            if std::fs::remove_dir_all(&path).is_err() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Whether `name` is a run directory's: a UUID in its hyphenated form.
fn is_run_dir(name: &std::ffi::OsStr) -> bool {
    let name = name.to_str().unwrap_or_default();
    uuid::Uuid::try_parse(name).is_ok_and(|id| id.hyphenated().to_string() == name)
}

/// The claim loop: claims while fewer than [`MAX_RUNS`] runs are in
/// progress and runs each claimed task in its own task; looks again when a
/// run ends or after the poll interval.
///
/// At most one Codex run holds the Codex login at a time, decided when
/// claiming: while one does, only Claude tasks are claimed (Claude's token is
/// never refreshed, so its runs can share it). The Codex run keeps the login
/// until it has ended, its refresh copied back included, so the next Codex
/// run, even the same task's fallback, copies in the newest login.
///
/// At most one agent run is in progress at a time, so one of the
/// [`MAX_RUNS`] slots always serves answer tasks: while one runs, only answer
/// tasks are claimed. A Codex agent run holds the Codex login like any Codex
/// run, for its whole duration.
///
/// Once `keep_going` returns false, or an error, it claims nothing more and
/// returns that when the runs in progress have ended.
pub async fn serve(
    config: std::sync::Arc<InternalConfig>,
    pool: PgPool,
    runner: String,
    mut keep_going: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let codex_login = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let agent_slot = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let mut runs = tokio::task::JoinSet::new();
    loop {
        let mut claimed = false;
        let codex = codex_login.clone().try_acquire_owned().ok();
        if codex.is_none() && config.codex.is_some() {
            pass_busy_codex(&pool, &runner).await;
        }
        if runs.len() < MAX_RUNS {
            let agent = agent_slot.clone().try_acquire_owned().ok();
            let free = (codex.is_some(), agent.is_some());
            match claim(&config, &pool, &runner, now_ms()?, free).await {
                Ok(Some(job)) => {
                    let is_codex = job.claimed.candidate.harness.as_deref() == Some("codex");
                    let login = codex.filter(|_| is_codex);
                    let agent = agent.filter(|_| job.claimed.request.agent.is_some());
                    let (config, pool) = (config.clone(), pool.clone());
                    runs.spawn(async move {
                        let ended = finish(job, &config, &pool).await;
                        drop((login, agent));
                        ended
                    });
                    claimed = true;
                }
                Ok(None) => {}
                Err(_) => eprintln!("internal work: claim failed"),
            }
        }
        let keep = keep_going();
        if !matches!(keep, Ok(true)) {
            while let Some(ended) = runs.join_next().await {
                report_end(ended);
            }
            return keep.map(drop);
        }
        if !claimed {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(config.poll_ms)) => {}
                Some(ended) = runs.join_next(), if !runs.is_empty() => report_end(ended),
            }
        }
        while let Some(ended) = runs.try_join_next() {
            report_end(ended);
        }
    }
}

/// The detail of a Codex candidate passed over while the login is busy.
const LOGIN_BUSY: &str = "codex login busy";

/// While the Codex login is busy: passes over the Codex candidate of each
/// queued task that cannot wait for it, so that its next candidate runs. A
/// task cannot wait when it has a later candidate and is blocking or has
/// less than twice [`MIN_CANDIDATE_BUDGET_MS`] left. A task whose last
/// candidate is Codex waits: passing it over would only fail it.
async fn pass_busy_codex(pool: &PgPool, runner: &str) {
    loop {
        let Ok(now) = now_ms() else { return };
        let short = 2 * MIN_CANDIDATE_BUDGET_MS;
        let passable = match store::claim_passable(pool, "codex", runner, now, short).await {
            Ok(Some(passable)) => passable,
            Ok(None) => return,
            Err(_) => {
                eprintln!("internal work: claim failed");
                return;
            }
        };
        let (id, index) = (passable.id, passable.candidate_index);
        if store::pass_candidate(pool, id, index, LOGIN_BUSY, now)
            .await
            .is_err()
        {
            eprintln!("internal work: passing over a busy login failed");
            return;
        }
    }
}

/// How long recording a run's result may take. Past it the host logs, gives
/// the record up (the sweep moves the task on) and frees the login and the
/// slot the run held.
const RECORD_TIMEOUT: Duration = Duration::from_secs(30);

async fn record(write: impl Future<Output = aq_internal_work::Result<()>>) -> Result<()> {
    match tokio::time::timeout(RECORD_TIMEOUT, write).await {
        Ok(written) => Ok(written?),
        Err(_) => {
            eprintln!("internal work: recording a run timed out");
            Ok(())
        }
    }
}

fn report_end(ended: std::result::Result<Result<()>, tokio::task::JoinError>) {
    if !matches!(ended, Ok(Ok(()))) {
        eprintln!("internal work: run failed");
    }
}

/// A login task this host claimed at `now_ms`, [`Instant`] `started`.
struct Job {
    claimed: Claimed,
    now_ms: i64,
    started: Instant,
}

/// Claims at most one queued login task for a harness this host has
/// configured, runs it, and records the answer or the candidate's failure.
/// Returns whether it claimed; it returns when the run has ended.
///
/// `now_ms` is the claim time. The run, its preparation included, is stopped
/// at [`Claimed::stop_at_ms`], counted from the claim exactly as the sweep
/// counts: the native's session gets SIGTERM and the failure is recorded at
/// once, while the run gets [`TERM_GRACE_MS`] to end before SIGKILL and then
/// reaps the native, copies a refreshed login back and removes its
/// directories.
pub async fn tick(
    config: &InternalConfig,
    pool: &PgPool,
    runner: &str,
    now_ms: i64,
) -> Result<bool> {
    let Some(job) = claim(config, pool, runner, now_ms, (true, true)).await? else {
        return Ok(false);
    };
    finish(job, config, pool).await?;
    Ok(true)
}

/// Claims the next queued login task for a harness this host has
/// configured; for Codex only when `codex` (its login is free), and agent
/// work only when `agents` (no agent runs here now).
async fn claim(
    config: &InternalConfig,
    pool: &PgPool,
    runner: &str,
    now_ms: i64,
    (codex, agents): (bool, bool),
) -> Result<Option<Job>> {
    let codex = config.codex.as_ref().filter(|_| codex);
    let configured: Vec<&str> = [("codex", codex), ("claude", config.claude.as_ref())]
        .into_iter()
        .filter_map(|(name, harness)| harness.map(|_| name))
        .collect();
    if configured.is_empty() {
        return Ok(None);
    }
    // At `now_ms`: the claim records it as `claimed_at_ms`.
    let started = Instant::now();
    let claimed = store::claim_login(pool, &configured, agents, runner, now_ms).await?;
    Ok(claimed.map(|claimed| Job {
        claimed,
        now_ms,
        started,
    }))
}

/// Runs a claimed task and records the answer or the candidate's failure.
async fn finish(job: Job, config: &InternalConfig, pool: &PgPool) -> Result<()> {
    let Job {
        claimed,
        now_ms,
        started,
    } = job;
    let finished = || {
        let elapsed = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        now_ms.saturating_add(elapsed)
    };
    let stop_after = u64::try_from(claimed.stop_at_ms() - now_ms).unwrap_or(0);
    let stop_at = started + Duration::from_millis(stop_after);
    let session = Session::new(stop_at);
    let transcript = Transcript::default();
    let run = run_candidate(config, &claimed, &session, &transcript);
    tokio::pin!(run);
    let ran = tokio::select! {
        biased;
        result = &mut run => Some(result),
        () = tokio::time::sleep_until(stop_at.into()) => None,
    };
    let (id, index) = (claimed.id, claimed.candidate_index);
    let Some(result) = ran else {
        session.terminate();
        let harness = claimed.candidate.harness.as_deref().unwrap_or("native");
        let detail = claimed.unanswered(harness);
        let reason = Reason::DeadlineExceeded;
        let report = record(store::fail_candidate(
            pool,
            id,
            index,
            reason,
            &detail,
            finished(),
        ));
        // The run's outcome is not used; it still has to reap the native,
        // copy a refreshed login back and remove its directories.
        let end = async {
            let grace = Duration::from_millis(TERM_GRACE_MS);
            if tokio::time::timeout(grace, &mut run).await.is_err() {
                session.kill();
                let _ = run.await;
            }
        };
        // The report is polled first and does not wait for the run to end.
        let (reported, ()) = tokio::join!(report, end);
        keep_transcript(pool, &claimed, &transcript).await;
        return reported;
    };
    keep_transcript(pool, &claimed, &transcript).await;
    let request = &claimed.request;
    let checked = result.and_then(|(raw, usage)| {
        validate::answer(&request.answer, &raw, request.max_output_bytes)
            .map(|answer| (answer, usage))
            .map_err(|detail| (Reason::InvalidOutput, detail))
    });
    let finished = finished();
    match checked {
        Ok((answer, usage)) => {
            record(store::complete(
                pool,
                id,
                index,
                &answer,
                usage.as_ref(),
                finished,
            ))
            .await
        }
        Err((reason, detail)) => {
            record(store::fail_candidate(
                pool, id, index, reason, &detail, finished,
            ))
            .await
        }
    }
}

/// Where an agent run leaves its clipped, redacted transcript for
/// [`keep_transcript`]; it stays empty for an answer run.
type Transcript = std::sync::Mutex<Option<serde_json::Value>>;

/// Stores the transcript an agent run left, once the run has ended (also
/// after a timeout, when its native was killed).
async fn keep_transcript(pool: &PgPool, claimed: &Claimed, transcript: &Transcript) {
    let events = transcript
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(events) = events else { return };
    let (id, index) = (claimed.id, claimed.candidate_index);
    if record(store::save_transcript(pool, id, index, &events))
        .await
        .is_err()
    {
        eprintln!("internal work: a transcript was not recorded");
    }
}

/// Runs `claimed` on the harness its candidate names, in `session`.
async fn run_candidate(
    config: &InternalConfig,
    claimed: &Claimed,
    session: &Session,
    transcript: &Transcript,
) -> Result<(String, Option<Usage>), Failure> {
    let (homes, scratch, identity) = (&config.homes, &config.scratch, config.identity);
    let harness = claimed.candidate.harness.as_deref();
    let run = Run {
        claimed,
        identity,
        session,
        transcript,
    };
    match (harness, &config.codex, &config.claude) {
        _ if claimed.budget_ms() == 0 => Err(failure(Reason::DeadlineExceeded, DEADLINE_PASSED)),
        (Some("codex"), Some(h), _) => codex::codex(h, homes, scratch, run).await,
        (Some("claude"), _, Some(h)) => claude::claude(h, homes, scratch, run).await,
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

/// What a harness runner is given for one attempt.
#[derive(Clone, Copy)]
struct Run<'a> {
    claimed: &'a Claimed,
    identity: Identity,
    session: &'a Session,
    transcript: &'a Transcript,
}

impl Run<'_> {
    /// Leaves an agent run's transcript for [`keep_transcript`].
    fn keep(&self, events: Option<serde_json::Value>) {
        *self
            .transcript
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = events;
    }
}

fn failure(reason: Reason, detail: &str) -> Failure {
    (reason, detail.into())
}

/// Checks the run directories' parents and the native of `h`, makes a fresh
/// private home under `homes` and an empty working directory under `scratch`
/// (owned by the native's user), writes agent work's files into the
/// directory, and runs `attempt` with the verified executable, the home and
/// the directory. Both are removed afterwards, whatever the outcome.
async fn in_run_dirs<T>(
    h: &Harness,
    homes: &Path,
    scratch: &Path,
    run: Run<'_>,
    attempt: impl AsyncFnOnce(&std::fs::File, &Path, &Path) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let identity = run.identity;
    for parent in [homes, scratch] {
        private_parent(parent, identity)
            .map_err(|_| failure(Reason::ProviderError, "internal work directories rejected"))?;
    }
    let executable = h
        .verified_native(identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "native executable refused"))?;
    let name = uuid::Uuid::new_v4().to_string();
    let (home, cwd) = (homes.join(&name), scratch.join(&name));
    let made = run_dirs(&home, &cwd, identity)
        .map_err(|_| failure(Reason::ProviderError, "native run could not be prepared"));
    // Removed however this ends, a panic included.
    let _dirs = RunDirs([&home, &cwd]);
    let work = run.claimed.request.agent.as_ref();
    let made = made.and_then(|()| match work {
        Some(work) => agent::materialize(&cwd, work, identity).map_err(|_| {
            failure(
                Reason::ProviderError,
                "agent workspace could not be prepared",
            )
        }),
        None => Ok(()),
    });
    match made {
        Ok(()) => attempt(&executable, &home, &cwd).await,
        Err(failure) => Err(failure),
    }
}

/// A run's home and working directory, removed on drop.
struct RunDirs<'a>([&'a Path; 2]);

impl Drop for RunDirs<'_> {
    fn drop(&mut self) {
        for dir in self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
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

/// The process session a native leads, so that the runner can stop the run
/// from outside it. No native starts once the session was stopped or its
/// stop time has passed.
struct Session {
    state: std::sync::Mutex<SessionState>,
    stop_at: Instant,
}

enum SessionState {
    NotStarted,
    Running(i32),
    Stopped,
}

impl Session {
    fn new(stop_at: Instant) -> Self {
        Session {
            state: std::sync::Mutex::new(SessionState::NotStarted),
            stop_at,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spawn(
        &self,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        let mut state = self.state();
        if matches!(*state, SessionState::Stopped) || Instant::now() >= self.stop_at {
            *state = SessionState::Stopped;
            return Err(std::io::Error::other("the run was stopped"));
        }
        let child = command.spawn()?;
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        *state = pid.map_or(SessionState::Stopped, SessionState::Running);
        Ok(child)
    }

    /// Asks every process in the session to end (SIGTERM), and keeps a
    /// native that has not started from starting.
    fn terminate(&self) {
        let mut state = self.state();
        match *state {
            SessionState::Running(pid) => unsafe {
                libc::kill(-pid, libc::SIGTERM);
            },
            _ => *state = SessionState::Stopped,
        }
    }

    /// Kills every process left in the session (SIGKILL), and keeps a
    /// native that has not started from starting.
    fn kill(&self) {
        let mut state = self.state();
        if let SessionState::Running(pid) = *state {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        *state = SessionState::Stopped;
    }
}

/// A run dropped before its end (an aborted task, a panic) kills the whole
/// session, not only the leader `kill_on_drop` reaches.
impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawns `command` in `session`, writes `prompt` to its stdin, reads its
/// stdout and waits for it to exit. Once it has exited, whatever is left of
/// its session is killed and stdout read for at most [`DRAIN_MS`] more, so a
/// grandchild holding the pipe cannot hold up the result. [`Session::terminate`]
/// and [`Session::kill`] end the run the same way.
async fn execute(
    command: std::process::Command,
    prompt: String,
    session: &Session,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut bytes = Vec::new();
    let status = execute_into(command, prompt, session, &mut |chunk| {
        // Past the limit the rest is drained unread.
        let room = STDOUT_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    })
    .await?;
    Ok((status, bytes))
}

/// [`execute`], handing each piece of stdout to `out` as it arrives instead
/// of collecting it, so an agent's long event stream is read whole.
async fn execute_into(
    command: std::process::Command,
    prompt: String,
    session: &Session,
    out: &mut (dyn FnMut(&[u8]) + Send),
) -> std::io::Result<std::process::ExitStatus> {
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
    let mut chunk = vec![0u8; 64 * 1024];
    let (mut written, mut status, mut drain_until) = (false, None, None);
    while status.is_none() || stdout.is_some() {
        let drained =
            tokio::time::sleep_until(drain_until.unwrap_or_else(tokio::time::Instant::now));
        tokio::select! {
            () = &mut write, if !written => written = true,
            read = read_some(&mut stdout, &mut chunk), if stdout.is_some() => match read {
                Ok(0) | Err(_) => stdout = None,
                Ok(n) => out(&chunk[..n]),
            },
            exited = child.wait(), if status.is_none() => {
                session.kill();
                status = Some(exited?);
                let drain = Duration::from_millis(DRAIN_MS);
                drain_until = Some(tokio::time::Instant::now() + drain);
            },
            () = drained, if drain_until.is_some() => break,
        }
    }
    Ok(status.expect("the loop ends only after the native has exited"))
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
