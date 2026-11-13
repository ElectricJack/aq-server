//! `internal` mode (spec 0012 §4, §6): claims internal tasks whose current
//! candidate is a logged-in agent and runs each once, non-interactively.
//!
//! Internal work takes no worker slot and is outside swarm admission: nothing
//! here reads swarm settings or admits a supervisor generation.
mod codex;

use crate::{config::protected_read, subscription::Identity};
use anyhow::{Result, ensure};
use aq_internal_work::{CANDIDATE_TIMEOUT_MS, Reason, Way, store, validate};
use aq_session_protocol::Digest;
use serde::Deserialize;
use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The harnesses this build can run. `claude` is configured and validated,
/// but its runner does not exist yet, so its tasks are left queued.
const RUNNABLE: [&str; 1] = ["codex"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InternalConfig {
    pub database_url: String,
    /// Parent of the per-run Codex homes; root-owned, not group or world writable.
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
    /// The login file. Each copy in and back holds a `flock` on its directory,
    /// which needs to be writable only for a refresh to be written back.
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
    if config.claude.is_some() {
        eprintln!("internal work: the Claude runner is not available in this build");
    }
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

/// Claims at most one queued login task for a harness this host runs, runs
/// it, and records the answer or the candidate's failure. Returns whether it
/// claimed. One task at a time: the next claim waits for this run to end.
pub async fn tick(
    config: &InternalConfig,
    pool: &PgPool,
    runner: &str,
    now_ms: i64,
) -> Result<bool> {
    if config.codex.is_none() {
        return Ok(false);
    }
    let Some(claimed) = store::claim(pool, Way::Login, &RUNNABLE, runner, now_ms).await? else {
        return Ok(false);
    };
    let started = Instant::now();
    // The same per-candidate budget as the in-server API loop.
    let budget = (claimed.deadline_at_ms - now_ms).clamp(0, CANDIDATE_TIMEOUT_MS);
    let result = match (&config.codex, claimed.candidate.harness.as_deref()) {
        _ if budget == 0 => Err((
            Reason::DeadlineExceeded,
            "the task deadline passed before the run".into(),
        )),
        (Some(h), Some("codex")) => {
            let budget = Duration::from_millis(budget as u64);
            codex::codex(
                h,
                &config.homes,
                &config.scratch,
                &claimed,
                config.identity,
                budget,
            )
            .await
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
