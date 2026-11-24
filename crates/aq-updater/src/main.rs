//! `aq-updater`: the host update service and the operator's SSH commands.
//!
//! Exit codes: 0 success; 1 failure (refused, rolled back, invalid
//! signature, lock held, corrupt journal); 2 usage or config error; 3 the
//! engine halted mid-update (the journal keeps the step; the next start
//! recovers from it).

use aq_release::{
    HistoryEntry, Outcome, ReleaseManifest, ReleaseSummary, UpdateRequest, UpdateState,
};
use aq_updater::config::UpdaterConfig;
use aq_updater::engine::{Engine, EngineSettings, Halted, update_state};
use aq_updater::journal::{Journal, JournalError, LockError, lock, read_history};
use aq_updater::runtime::DockerCli;
use aq_updater::spool::{self, SpoolMaintenance};
use aq_updater::verify::{self, ReleaseVerifier, VerifyError};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_HALTED: u8 = 3;
/// How often `run` looks for update requests.
const REQUEST_POLL: Duration = Duration::from_secs(2);

/// Agent Q host updater: installs verified releases into the Compose project.
#[derive(Parser)]
#[command(name = "aq-updater", version)]
struct Cli {
    /// Path to the updater config.
    #[arg(long, global = true, default_value = "/etc/agent-q/updater.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the update service: recover, then install requests from the spool
    /// and poll the channel until SIGTERM.
    Run {
        /// Recover, install pending requests, then exit.
        #[arg(long)]
        once: bool,
    },
    /// Show update state, releases and recent history (no lock, no writes).
    Status,
    /// Install a release version now (takes the update lock).
    Install { version: semver::Version },
    /// Bring back the previous release, or retry bringing back the active
    /// release after a failed rollback.
    RestorePrevious {
        /// Allow restoring the database backup (loses changes made since it).
        #[arg(long)]
        restore_backup: bool,
    },
    /// Verify a release manifest against its signature bundle: exit 0 if
    /// valid, 1 if not, 2 on usage or config errors.
    VerifyManifest { manifest: PathBuf, bundle: PathBuf },
    /// Test only: write an ed25519 key pair (secret.hex 0600, public.hex).
    #[cfg(feature = "test-verifier")]
    #[command(hide = true)]
    TestKeygen { dir: PathBuf },
    /// Test only: print the test-key bundle for a manifest.
    #[cfg(feature = "test-verifier")]
    #[command(hide = true)]
    TestSign {
        #[arg(long)]
        key: PathBuf,
        manifest: PathBuf,
    },
}

/// A command's failure: what to print and the exit code.
struct Exit(u8, String);

type CmdResult = Result<ExitCode, Exit>;

fn usage(m: impl std::fmt::Display) -> Exit {
    Exit(EXIT_USAGE, format!("aq-updater: {m}"))
}
fn failure(m: impl std::fmt::Display) -> Exit {
    Exit(EXIT_FAILED, format!("aq-updater: {m}"))
}
fn halted(h: Halted) -> Exit {
    tracing::error!(error = %h, "update halted; restart to recover");
    Exit(EXIT_HALTED, format!("aq-updater: {h}"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("aq-updater: cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILED);
        }
    };
    match rt.block_on(dispatch(cli)) {
        Ok(code) => code,
        Err(Exit(code, msg)) => {
            eprintln!("{msg}");
            ExitCode::from(code)
        }
    }
}

async fn dispatch(cli: Cli) -> CmdResult {
    match cli.command {
        #[cfg(feature = "test-verifier")]
        Command::TestKeygen { dir } => test_signing::keygen(&dir),
        #[cfg(feature = "test-verifier")]
        Command::TestSign { key, manifest } => test_signing::sign(&key, &manifest),
        command => {
            let cfg = UpdaterConfig::load(&cli.config).map_err(usage)?;
            match command {
                Command::Run { once } => run(&cfg, once).await,
                Command::Status => status(&cfg),
                Command::Install { version } => install(&cfg, version).await,
                Command::RestorePrevious { restore_backup } => {
                    restore_previous(&cfg, restore_backup).await
                }
                Command::VerifyManifest { manifest, bundle } => {
                    verify_manifest(&cfg, &manifest, &bundle).await
                }
                #[cfg(feature = "test-verifier")]
                Command::TestKeygen { .. } | Command::TestSign { .. } => unreachable!(),
            }
        }
    }
}

// ------------------------------------------------------------------- engine

type RealEngine = Engine<DockerCli, Box<dyn ReleaseVerifier>>;

/// Lock, open the journal, prepare the spool, and build the engine with
/// status published on every change.
fn open_engine(cfg: &UpdaterConfig) -> Result<RealEngine, Exit> {
    let lock = lock(&cfg.state_dir).map_err(|e| match e {
        LockError::AlreadyLocked(_) => failure(format!(
            "{e}. The update service holds the lock while it runs: stop it first (systemctl stop aq-updater), or request the update through the API"
        )),
        LockError::Io { .. } => failure(e),
    })?;
    let journal = Journal::open(lock).map_err(failure)?;
    spool::ensure_dirs(&cfg.state_dir)
        .map_err(|e| failure(format!("cannot prepare the spool: {e}")))?;
    let runtime = DockerCli::from_config(cfg);
    let active = journal
        .state()
        .active
        .as_ref()
        .map(|m| m.images.server.clone());
    // Before any Compose call: compose.yaml needs both release.env variables
    // even on a fresh host (postgres, psql, pg_dump before the first install).
    runtime
        .ensure_release_env(active.as_deref())
        .map_err(failure)?;
    let verifier = verify::from_config(&cfg.verifier).map_err(usage)?;
    let status_dir = spool::status_dir(&cfg.state_dir);
    let mut engine = Engine::new(
        journal,
        runtime,
        verifier,
        Box::new(SpoolMaintenance::new(&status_dir)),
        EngineSettings::from_config(cfg),
    );
    // Keep the `available` the last run found until the next poll, if it is
    // still newer than the active release.
    if let Some(prev) = spool::read_status(&status_dir) {
        engine.seed_available(prev.available);
    }
    engine.set_observer(Box::new(move |s| {
        if let Err(e) = spool::publish_status(&status_dir, s) {
            tracing::warn!(error = %e, "cannot publish status.json");
        }
    }));
    Ok(engine)
}

fn publish(engine: &RealEngine, cfg: &UpdaterConfig) {
    if let Err(e) = spool::publish_status(&spool::status_dir(&cfg.state_dir), &engine.status()) {
        tracing::warn!(error = %e, "cannot publish status.json");
    }
}

/// `recover()`, then publish status and make the maintenance marker match
/// the journal.
async fn start(engine: &mut RealEngine, cfg: &UpdaterConfig) -> Result<(), Exit> {
    let recovered = engine.recover().await;
    publish(engine, cfg);
    match recovered {
        Ok(Some(e)) => {
            tracing::info!(outcome = ?e.outcome, detail = ?e.detail, "recovered an interrupted update")
        }
        Ok(None) => {}
        Err(h) => return Err(halted(h)),
    }
    if let Err(e) = engine.sync_maintenance() {
        tracing::warn!(error = %e, "cannot set the maintenance marker");
    }
    Ok(())
}

/// A shutdown flag set on the first SIGTERM or SIGINT. The handlers are
/// installed at once, so a signal is never fatal; the work in progress (an
/// install included) finishes and nothing new starts. SIGHUP (a `run --once`
/// whose SSH session dropped) is ignored: the service keeps running.
fn shutdown_signal() -> Result<tokio::sync::watch::Receiver<bool>, Exit> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| failure(format!("cannot handle SIGTERM: {e}")))?;
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| failure(format!("cannot handle SIGINT: {e}")))?;
    let mut sighup =
        signal(SignalKind::hangup()).map_err(|e| failure(format!("cannot handle SIGHUP: {e}")))?;
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = sigterm.recv() => break,
                _ = sigint.recv() => break,
                _ = sighup.recv() => tracing::info!("SIGHUP ignored"),
            }
        }
        tracing::info!("shutdown requested; finishing the current step, starting nothing new");
        let _ = tx.send(true);
        // Keep the sender (and the handlers) alive: later signals are ignored.
        loop {
            tokio::select! {
                _ = sigterm.recv() => {}
                _ = sigint.recv() => {}
                _ = sighup.recv() => {}
            }
        }
    });
    Ok(rx)
}

/// Printed (once) when a manual `install` or `restore-previous` is signalled.
const INTERRUPT_NOTICE: &str = "aq-updater: interrupt received; finishing the current operation so recovery stays consistent (send SIGKILL to abort; the journal will recover on next start)";

/// For `install` and `restore-previous`: from here on SIGINT, SIGTERM and
/// SIGHUP never stop the process. The docker commands run in their own
/// process group, so a signal that killed aq-updater would leave a
/// `pg_restore` or `compose up` running with nobody to record its outcome.
/// Instead the first signal prints `INTERRUPT_NOTICE` and the operation
/// runs to its end; later ones are ignored the same way.
fn hold_through_interrupts() -> Result<(), Exit> {
    use tokio::signal::unix::{SignalKind, signal};
    let handle = |kind: SignalKind, name: &str| {
        signal(kind).map_err(|e| failure(format!("cannot handle {name}: {e}")))
    };
    let mut int = handle(SignalKind::interrupt(), "SIGINT")?;
    let mut term = handle(SignalKind::terminate(), "SIGTERM")?;
    let mut hup = handle(SignalKind::hangup(), "SIGHUP")?;
    tokio::spawn(async move {
        let mut told = false;
        loop {
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            if !told {
                told = true;
                // The terminal may be gone (SIGHUP): never panic on the write.
                use std::io::Write;
                let _ = writeln!(std::io::stderr(), "{INTERRUPT_NOTICE}");
            }
        }
    });
    Ok(())
}

async fn run(cfg: &UpdaterConfig, once: bool) -> CmdResult {
    let mut stop = shutdown_signal()?;
    let mut engine = open_engine(cfg)?;
    start(&mut engine, cfg).await?;
    let requests = spool::requests_dir(&cfg.state_dir);
    if once {
        let r = spool::process_requests(&mut engine, &requests, &stop).await;
        publish(&engine, cfg);
        r.map_err(halted)?;
        return Ok(ExitCode::SUCCESS);
    }
    tracing::info!(state_dir = %cfg.state_dir.display(), "aq-updater running");
    let mut requests_tick = tokio::time::interval(REQUEST_POLL);
    requests_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut poll_tick = tokio::time::interval(Duration::from_secs(cfg.poll_interval_secs));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !*stop.borrow() {
        // Only the waits race the shutdown flag; a chosen branch runs to
        // completion. `process_requests` also checks the flag before each
        // install, so no new install starts after a signal.
        tokio::select! {
            biased;
            _ = stop.changed() => {}
            _ = requests_tick.tick() => {
                let r = spool::process_requests(&mut engine, &requests, &stop).await;
                if let Err(h) = r {
                    publish(&engine, cfg);
                    return Err(halted(h));
                }
            }
            _ = poll_tick.tick() => {
                match engine.refresh_available().await {
                    Ok(a) => tracing::debug!(available = ?a.map(|s| s.version), "channel polled"),
                    Err(e) => tracing::warn!(error = %e, "channel poll failed; available unchanged"),
                }
                publish(&engine, cfg);
            }
        }
    }
    tracing::info!("aq-updater stopping");
    publish(&engine, cfg);
    Ok(ExitCode::SUCCESS)
}

/// The outcome line. Its write may fail when the SSH session that started
/// the command is gone; the outcome is in history either way.
fn print_entry(e: &HistoryEntry) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "{}", entry_line(e));
}

async fn install(cfg: &UpdaterConfig, version: semver::Version) -> CmdResult {
    hold_through_interrupts()?;
    let mut engine = open_engine(cfg)?;
    start(&mut engine, cfg).await?;
    let req = UpdateRequest {
        id: uuid::Uuid::new_v4(),
        version: Some(version),
        requested_by: "aq-updater install (cli)".into(),
        requested_at: Utc::now(),
    };
    let entry = engine.install(req).await;
    publish(&engine, cfg);
    let entry = entry.map_err(halted)?;
    print_entry(&entry);
    Ok(outcome_code(&entry, &[Outcome::Succeeded]))
}

async fn restore_previous(cfg: &UpdaterConfig, allow_restore: bool) -> CmdResult {
    hold_through_interrupts()?;
    let mut engine = open_engine(cfg)?;
    start(&mut engine, cfg).await?;
    let entry = engine.restore_previous(allow_restore).await;
    publish(&engine, cfg);
    let entry = entry.map_err(halted)?;
    print_entry(&entry);
    // `RolledBack` is the success of a retry after a failed rollback.
    Ok(outcome_code(
        &entry,
        &[Outcome::Succeeded, Outcome::RolledBack],
    ))
}

fn outcome_code(e: &HistoryEntry, ok: &[Outcome]) -> ExitCode {
    if ok.contains(&e.outcome) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_FAILED)
    }
}

async fn verify_manifest(cfg: &UpdaterConfig, manifest: &Path, bundle: &Path) -> CmdResult {
    let read =
        |p: &Path| std::fs::read(p).map_err(|e| usage(format!("cannot read {}: {e}", p.display())));
    let (m, b) = (read(manifest)?, read(bundle)?);
    let verifier = verify::from_config(&cfg.verifier).map_err(usage)?;
    match verifier.verify(&m, &b).await {
        Ok(()) => {}
        Err(e @ VerifyError::Rejected(_)) => return Err(failure(e)),
        Err(e @ VerifyError::Unavailable(_)) => return Err(usage(e)),
    }
    let parsed = ReleaseManifest::from_json(&m).map_err(failure)?;
    println!(
        "verified: release {} ({})",
        parsed.version, parsed.source_revision
    );
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------- status

fn state_name(s: &UpdateState) -> &'static str {
    match s {
        UpdateState::Idle => "idle",
        UpdateState::InProgress { .. } => "in_progress",
        UpdateState::FailedNeedsOperator { .. } => "failed_needs_operator",
        UpdateState::Unknown => "unknown",
    }
}

fn outcome_name(o: Outcome) -> &'static str {
    match o {
        Outcome::Succeeded => "succeeded",
        Outcome::RolledBack => "rolled_back",
        Outcome::Refused => "refused",
        Outcome::Rejected => "rejected",
        Outcome::Aborted => "aborted",
        Outcome::FailedNeedsOperator => "failed_needs_operator",
        Outcome::Unknown => "unknown",
    }
}

fn release_line(s: Option<&ReleaseSummary>) -> String {
    match s {
        None => "none".into(),
        Some(s) => format!(
            "{} (revision {}, state format {}, {})",
            s.version, s.source_revision, s.state_format, s.server_image
        ),
    }
}

fn summary(m: &ReleaseManifest) -> ReleaseSummary {
    ReleaseSummary {
        version: m.version.clone(),
        source_revision: m.source_revision.clone(),
        server_image: m.images.server.clone(),
        state_format: m.state_format,
    }
}

/// History details can quote request content; never let them drive the
/// operator's terminal.
fn printable(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn entry_line(e: &HistoryEntry) -> String {
    let v = |v: &Option<semver::Version>| v.as_ref().map_or("-".to_string(), |v| v.to_string());
    format!(
        "{} {} {} -> {}: {}",
        e.finished_at.format("%Y-%m-%d %H:%M:%SZ"),
        outcome_name(e.outcome),
        v(&e.from),
        v(&e.to),
        printable(&e.detail)
    )
}

fn status_state_line(state: &UpdateState) -> String {
    match state {
        UpdateState::InProgress {
            request_id,
            target,
            step,
            started_at,
        } => format!(
            "in_progress: {} at step {} (request {request_id}, started {})",
            target
                .as_ref()
                .map_or("the channel release".to_string(), |v| v.to_string()),
            serde_json::to_value(step)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            started_at.format("%Y-%m-%d %H:%M:%SZ")
        ),
        other => state_name(other).to_string(),
    }
}

fn append_operator_instructions(out: &mut String, state: &UpdateState) {
    if let UpdateState::FailedNeedsOperator { detail } = state {
        let detail = printable(detail);
        let cmd = if detail.contains("--restore-backup") {
            "aq-updater restore-previous --restore-backup"
        } else {
            "aq-updater restore-previous"
        };
        out.push_str(&format!(
            "\nThe update failed and so did the rollback. Maintenance stays on and nothing further is tried automatically.\n\
             detail: {detail}\n\n\
             Next steps:\n\
             \x20 1. Inspect the server and database: docker compose logs server\n\
             \x20 2. Stop the update service so the command below can take the lock: systemctl stop aq-updater\n\
             \x20 3. Retry bringing back the active release:\n\
             \x20      {cmd}\n\
             \x20 4. Start the update service again: systemctl start aq-updater\n"
        ));
    }
}

/// Lock-free and read-only: the journal, history and the last status.json.
fn status(cfg: &UpdaterConfig) -> CmdResult {
    let state_dir = &cfg.state_dir;
    let journal = match Journal::read(state_dir) {
        Ok(j) => j,
        Err(e @ JournalError::Corrupt { .. }) => {
            let JournalError::Corrupt { raw, .. } = &e else {
                unreachable!()
            };
            println!("{e}\n\n--- raw journal.json ---\n{raw}\n--- end ---");
            return Ok(ExitCode::from(EXIT_FAILED));
        }
        Err(e) => {
            println!("{e}");
            return Ok(ExitCode::from(EXIT_FAILED));
        }
    };
    let history = read_history(state_dir, 10).map_err(failure)?;
    let recorded = match &journal.current {
        Some(c) if c.rollback_attempted => read_history(state_dir, usize::MAX)
            .map_err(failure)?
            .into_iter()
            .find(|e| e.request_id == Some(c.request_id) && e.outcome != Outcome::Rejected),
        _ => None,
    };
    let state = update_state(journal.current.as_ref(), recorded.as_ref());
    let available = spool::read_status(&spool::status_dir(state_dir)).and_then(|s| s.available);

    let mut out = String::new();
    let mut line = |label: &str, value: String| out.push_str(&format!("{label:<11}{value}\n"));
    line("state:", status_state_line(&state));
    line(
        "active:",
        release_line(journal.active.as_ref().map(summary).as_ref()),
    );
    line(
        "previous:",
        release_line(journal.previous.as_ref().map(summary).as_ref()),
    );
    line("available:", release_line(available.as_ref()));
    append_operator_instructions(&mut out, &state);
    out.push_str("\nhistory (newest first):\n");
    if history.is_empty() {
        out.push_str("  (none)\n");
    }
    for e in &history {
        out.push_str(&format!("  {}\n", entry_line(e)));
    }
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------ test signing

#[cfg(feature = "test-verifier")]
mod test_signing {
    use super::{CmdResult, failure, usage};
    use ed25519_dalek::{Signer, SigningKey};
    use std::io::{Read, Write};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::process::ExitCode;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex32(s: &str) -> Option<[u8; 32]> {
        let s = s.trim();
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(out)
    }

    fn write_new(path: &Path, mode: u32, body: &str) -> Result<(), super::Exit> {
        let err = |e: std::io::Error| failure(format!("cannot write {}: {e}", path.display()));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)
            .map_err(err)?;
        f.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(err)?;
        f.write_all(body.as_bytes()).map_err(err)?;
        f.sync_all().map_err(err)
    }

    pub fn keygen(dir: &Path) -> CmdResult {
        std::fs::create_dir_all(dir)
            .map_err(|e| failure(format!("cannot create {}: {e}", dir.display())))?;
        let mut seed = [0u8; 32];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut seed))
            .map_err(|e| failure(format!("cannot read /dev/urandom: {e}")))?;
        let sk = SigningKey::from_bytes(&seed);
        let (secret, public) = (dir.join("secret.hex"), dir.join("public.hex"));
        write_new(&secret, 0o600, &format!("{}\n", hex(&seed)))?;
        write_new(
            &public,
            0o644,
            &format!("{}\n", hex(sk.verifying_key().as_bytes())),
        )?;
        println!("{}", public.display());
        Ok(ExitCode::SUCCESS)
    }

    pub fn sign(key: &Path, manifest: &Path) -> CmdResult {
        let text = std::fs::read_to_string(key)
            .map_err(|e| usage(format!("cannot read {}: {e}", key.display())))?;
        let seed = unhex32(&text).ok_or_else(|| usage("the key must be 64 hex characters"))?;
        let bytes = std::fs::read(manifest)
            .map_err(|e| usage(format!("cannot read {}: {e}", manifest.display())))?;
        let sig = SigningKey::from_bytes(&seed).sign(&bytes);
        println!("{}", hex(&sig.to_bytes()));
        Ok(ExitCode::SUCCESS)
    }
}
