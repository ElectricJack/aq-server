//! `internal` mode (spec 0012 §4, §6): claims internal tasks whose current
//! candidate is a logged-in agent and runs each once, non-interactively.
//!
//! Agent work (spec 0013 §4.2) runs the harness with tools in a workspace
//! holding the request's files until it writes its outcome ([`agent`]).
//!
//! Internal work takes no worker slot and is outside swarm admission: nothing
//! here reads swarm settings or admits a supervisor generation.
mod agent;
pub(crate) mod claude;
mod codex;

use crate::{
    config::protected_read,
    subscription::{AGENT_UID, Identity, file_digest, open_native, recover_login},
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

mod config;
mod directories;
pub(crate) mod process;
mod queue;

pub use config::{Harness, InternalConfig};
pub use directories::recover_run_dirs;
use directories::{Place, in_run_dirs};
use process::{Session, execute, execute_into, sweep_uid};
pub use queue::{serve, tick};

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
    if config.identity.drop_privileges {
        // Whatever an agent run of a host that stopped left running.
        sweep_uid(AGENT_UID);
    }
    recover_run_dirs(&config);
    // A statement that hangs ends instead of holding a run, its slot and
    // the Codex login.
    let options: sqlx::postgres::PgConnectOptions = config.database_url.parse()?;
    let options = options.options([("statement_timeout", "30000")]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(MAX_RUNS as u32 + 1)
        .idle_timeout(aq_machine_control::connections::SECONDARY_IDLE)
        .connect_with(options)
        .await?;
    let runner = format!("internal-host:{}", hostname()?);
    let config = std::sync::Arc::new(config);
    serve(config, pool, runner, || unchanged(path, &digest)).await
}

/// Where an agent run leaves its clipped, redacted transcript for
/// the queue; it stays empty for an answer run.
type TranscriptSlot = std::sync::Mutex<Option<serde_json::Value>>;

/// Runs `claimed` on the harness its candidate names, in `session`.
async fn run_candidate(
    config: &InternalConfig,
    claimed: &Claimed,
    session: &Session,
    transcript: &TranscriptSlot,
) -> Result<(String, Option<Usage>), Failure> {
    let harness = claimed.candidate.harness.as_deref();
    let identity = match claimed.request.agent {
        Some(_) => config.identity.agent(),
        None => config.identity,
    };
    let run = Run {
        claimed,
        config,
        identity,
        session,
        transcript,
    };
    match (harness, &config.codex, &config.claude) {
        _ if claimed.budget_ms() == 0 => Err(failure(Reason::DeadlineExceeded, DEADLINE_PASSED)),
        (Some("codex"), Some(h), _) => codex::codex(h, run).await,
        (Some("claude"), _, Some(h)) => claude::claude(h, run).await,
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

/// What a harness runner is given for one attempt. `identity` runs agent
/// work as the agent's own uid ([`Identity::agent`]).
#[derive(Clone, Copy)]
struct Run<'a> {
    claimed: &'a Claimed,
    config: &'a InternalConfig,
    identity: Identity,
    session: &'a Session,
    transcript: &'a TranscriptSlot,
}

impl Run<'_> {
    /// The secrets of every login this host holds, whichever harness the run
    /// used, read now: call it once the native has ended, so a refresh copied
    /// back meanwhile is included.
    fn secrets(&self) -> Vec<zeroize::Zeroizing<String>> {
        login_secrets(self.config)
    }

    /// Leaves an agent run's transcript for the queue.
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

/// `text` with each of `secrets` replaced.
fn scrub(mut text: String, secrets: &[zeroize::Zeroizing<String>]) -> String {
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret.as_str(), agent::REDACTED);
    }
    text
}

/// The secrets of the logins `config` names: the Claude setup token and
/// the Codex login's tokens, read now. A file that cannot be read gives none.
fn login_secrets(config: &InternalConfig) -> Vec<zeroize::Zeroizing<String>> {
    use zeroize::Zeroizing;
    let mut secrets: Vec<_> = config
        .claude
        .as_ref()
        .and_then(|h| {
            let token = protected_read(&h.secret_file, 65536)
                .ok()
                .map(Zeroizing::new)?;
            let token = aq_intelligence_config::material::normalize_claude_token(token).ok()?;
            std::str::from_utf8(&token)
                .ok()
                .map(|s| Zeroizing::new(s.to_owned()))
        })
        .into_iter()
        .collect();
    if let Some(login) = config.codex.as_ref().and_then(codex_login) {
        secrets.extend(
            ["id_token", "access_token", "refresh_token"]
                .iter()
                .filter_map(|key| {
                    login["tokens"][key]
                        .as_str()
                        .map(|s| Zeroizing::new(s.to_owned()))
                }),
        );
    }
    secrets
}

fn codex_login(harness: &Harness) -> Option<serde_json::Value> {
    let bytes = zeroize::Zeroizing::new(protected_read(&harness.secret_file, 65536).ok()?);
    serde_json::from_slice(&bytes).ok()
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
