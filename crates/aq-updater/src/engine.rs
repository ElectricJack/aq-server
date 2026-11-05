//! Update state machine: install, bounded rollback, restore-previous, and
//! recovery from every persisted step.
//!
//! Rules the code below keeps:
//! - Each `Step` is persisted before the action it guards, so after a crash
//!   the journal alone says what may have happened.
//! - Before the replacement the active release is never stopped. Once the
//!   target has been started, a failure rolls back exactly once; a failed
//!   rollback becomes `FailedNeedsOperator` with maintenance left on.
//! - A terminal outcome is appended to history before `current` is cleared.
//!   `recover` looks for an outcome already recorded for the in-flight
//!   request, so a crash between the two never records it twice.
//! - Runtime errors marked `Interrupted` (and journal write failures) halt
//!   the engine at once with the journal as written; `recover` resumes.

use crate::config::UpdaterConfig;
use crate::journal::{InFlight, Journal, JournalError, JournalState};
use crate::runtime::{ReleaseSelector, Runtime, RuntimeError};
use crate::verify::ReleaseVerifier;
use aq_release::{
    HistoryEntry, Outcome, ReadinessReport, ReleaseManifest, ReleaseSummary, Step, UpdateRequest,
    UpdateState, UpdateStatus, check_upgrade,
};
use chrono::{DateTime, Utc};
use semver::Version;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Image label carrying the release's state format; checked after pull.
pub const STATE_FORMAT_LABEL: &str = "io.agent-q.state-format";
/// Detail of a `Rejected` entry for a request that arrived mid-update.
pub const UPDATE_IN_PROGRESS: &str = "update_in_progress";
/// History entries included in `UpdateStatus`.
pub const STATUS_HISTORY: usize = 20;

/// Toggles the server-visible maintenance marker. Both calls are idempotent.
pub trait MaintenanceSink {
    fn enter(&self) -> Result<(), RuntimeError>;
    fn leave(&self) -> Result<(), RuntimeError>;
}

#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub backup_dir: PathBuf,
    pub readiness_deadline: Duration,
    pub min_free_bytes: u64,
    pub backups_keep: usize,
    /// The `auth_mode` a healthy release must report (spec step 6).
    pub expected_auth_mode: String,
    /// Wait after entering maintenance and before the backup, so admin
    /// writes that passed the maintenance gate just before it closed can
    /// commit and land in the backup.
    pub maintenance_drain: Duration,
}

impl EngineSettings {
    pub fn from_config(cfg: &UpdaterConfig) -> Self {
        EngineSettings {
            backup_dir: cfg.backup_dir.clone(),
            readiness_deadline: Duration::from_secs(cfg.readiness_deadline_secs),
            min_free_bytes: cfg.min_free_bytes,
            backups_keep: cfg.backups_keep,
            expected_auth_mode: cfg.expected_auth_mode.clone(),
            maintenance_drain: Duration::from_secs(cfg.maintenance_drain_secs),
        }
    }
}

/// The engine stopped mid-operation without reaching an outcome. The journal
/// holds the last persisted step; `Engine::recover` resumes from it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "update halted: {0}; the journal keeps the last persisted step and recovery resumes from it"
)]
pub struct Halted(pub String);

impl From<JournalError> for Halted {
    fn from(e: JournalError) -> Self {
        Halted(format!("journal write failed: {e}"))
    }
}

type Res<T> = Result<T, Halted>;
/// Inner `Err` is a failed check, with a detail for history.
type Check<T = ()> = Res<Result<T, String>>;

/// Split a runtime result into "halt now" and "this check failed".
fn rt<T>(r: Result<T, RuntimeError>) -> Check<T> {
    match r {
        Ok(v) => Ok(Ok(v)),
        Err(RuntimeError::Failed(m)) => Ok(Err(m)),
        Err(RuntimeError::Interrupted(m)) => Err(Halted(m)),
    }
}

/// Fetch, verify and parse a release, read-only. Borrows only the runtime
/// and verifier so callers' futures stay `Send`.
async fn fetch_verified<R: Runtime, V: ReleaseVerifier>(
    runtime: &R,
    verifier: &V,
    selector: ReleaseSelector,
) -> Check<ReleaseManifest> {
    let (manifest, bundle) = match rt(runtime.fetch_release(selector).await)? {
        Ok(x) => x,
        Err(e) => return Ok(Err(format!("cannot fetch the release: {e}"))),
    };
    if let Err(e) = verifier.verify(&manifest, &bundle).await {
        return Ok(Err(format!("verification failed: {e}")));
    }
    Ok(ReleaseManifest::from_json(&manifest).map_err(|e| e.to_string()))
}

/// The updater state `status.json` and `aq-updater status` report for an
/// in-flight record, given the newest non-`Rejected` history entry for its
/// request (if any).
pub fn update_state(current: Option<&InFlight>, recorded: Option<&HistoryEntry>) -> UpdateState {
    match current {
        None => UpdateState::Idle,
        // Outside a running operation, an attempted rollback that is still
        // in flight means the rollback failed (or was cut short, which
        // recovery also turns into this state).
        Some(c) if c.rollback_attempted => UpdateState::FailedNeedsOperator {
            detail: match recorded {
                Some(e) if e.outcome == Outcome::FailedNeedsOperator => e.detail.clone(),
                _ => format!(
                    "the rollback after the failed update to {} did not complete",
                    c.target.version
                ),
            },
        },
        Some(c) => UpdateState::InProgress {
            request_id: c.request_id,
            target: Some(c.target.version.clone()),
            step: c.step,
            started_at: c.started_at,
        },
    }
}

/// Called with the new status after every journal write and history append.
pub type StatusObserver = Box<dyn Fn(&UpdateStatus) + Send>;

/// What history says about an in-flight request.
enum Recorded {
    Nothing,
    Entry(HistoryEntry),
    /// History could not be read: the outcome cannot be told.
    Unreadable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Install,
    RestorePrevious,
}

fn step_name(s: Step) -> &'static str {
    match s {
        Step::Requested => "requested",
        Step::Validated => "validated",
        Step::Pulled => "pulled",
        Step::Maintenance => "maintenance",
        Step::BackedUp => "backed_up",
        Step::Replacing => "replacing",
        Step::Probing => "probing",
        Step::Active => "active",
        Step::RollingBack => "rolling_back",
        Step::Unknown => "unknown",
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

/// A readiness report passes only if it describes exactly `m`, the database
/// is reachable, and authentication is configured as `auth_mode`.
pub fn check_ready(
    r: &ReadinessReport,
    m: &ReleaseManifest,
    auth_mode: &str,
) -> Result<(), String> {
    let mut bad = Vec::new();
    if r.version != m.version {
        bad.push(format!("version {} (expected {})", r.version, m.version));
    }
    if r.source_revision != m.source_revision {
        bad.push(format!(
            "source_revision {:?} (expected {:?})",
            r.source_revision, m.source_revision
        ));
    }
    if r.state_format != m.state_format {
        bad.push(format!(
            "state_format {} (expected {})",
            r.state_format, m.state_format
        ));
    }
    if r.database != "ok" {
        bad.push(format!("database {:?} (expected \"ok\")", r.database));
    }
    if (r.protocol_min, r.protocol_max) != (m.protocol.min, m.protocol.max) {
        bad.push(format!(
            "protocol {}..{} (expected {}..{})",
            r.protocol_min, r.protocol_max, m.protocol.min, m.protocol.max
        ));
    }
    if r.auth_mode != auth_mode {
        bad.push(format!(
            "auth_mode {:?} (expected {auth_mode:?})",
            r.auth_mode
        ));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("readiness mismatch: {}", bad.join(", ")))
    }
}

/// `*.dump` files in `dir`, newest first. Names start with a UTC timestamp,
/// so name order is time order.
fn dumps(dir: &Path) -> Vec<PathBuf> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %e, "cannot list backups");
            }
            return Vec::new();
        }
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "dump"))
        .collect();
    v.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    v
}

/// The updater will not start a backup (DEP8): the backup filesystem (the
/// root filesystem in the standard layout) has less free space than the
/// newest backup plus 10%.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "not enough free space to start the backup in {}: {free} bytes free, {need} needed (the newest backup, {newest} bytes, plus 10%); free some space and retry",
    dir.display()
)]
pub struct InsufficientBackupSpace {
    pub dir: PathBuf,
    pub free: u64,
    pub newest: u64,
    pub need: u64,
}

/// Free bytes a backup needs: the newest backup's size plus 10%, rounded up.
pub fn backup_space_needed(newest: u64) -> u64 {
    newest.saturating_add(newest.div_ceil(10))
}

/// Refuse to start a backup in `dir` when `free` is below the newest
/// backup's size (`newest`) plus 10%.
pub fn check_backup_space(
    dir: &Path,
    free: u64,
    newest: u64,
) -> Result<(), InsufficientBackupSpace> {
    let need = backup_space_needed(newest);
    if free < need {
        return Err(InsufficientBackupSpace {
            dir: dir.to_path_buf(),
            free,
            newest,
            need,
        });
    }
    Ok(())
}

/// Repository part of a digest reference (`host/path@sha256:…` → `host/path`).
pub fn image_repository(image: &str) -> Option<&str> {
    image.split_once('@').map(|(repo, _)| repo)
}

/// Images to remove (DEP7c): every image in `present` that is not in `keep`
/// (the current and previous release's images). `present` lists only the
/// repositories of the kept images, so nothing else is selected.
pub fn images_to_prune(present: &[String], keep: &[String]) -> Vec<String> {
    let mut out: Vec<String> = present
        .iter()
        .filter(|p| !keep.contains(p))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Remove release images other than `keep`, in the repositories of `keep`.
/// Best effort: failures are logged and never change an update's outcome.
async fn prune_release_images<R: Runtime>(runtime: &R, keep: Vec<String>) {
    let mut repos: Vec<&str> = keep.iter().filter_map(|i| image_repository(i)).collect();
    repos.sort_unstable();
    repos.dedup();
    for repo in repos {
        let present = match runtime.list_images(repo).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(repository = repo, error = %e, "cannot list images to prune");
                continue;
            }
        };
        for image in images_to_prune(&present, &keep) {
            match runtime.remove_image(&image).await {
                Ok(()) => tracing::info!(image = %image, "pruned old release image"),
                Err(e) => tracing::warn!(image = %image, error = %e, "cannot prune image"),
            }
        }
    }
}

fn last_backup_size(dir: &Path) -> u64 {
    dumps(dir)
        .first()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or(0, |m| m.len())
}

// The async helpers below borrow only the runtime, never the engine, so the
// engine's futures stay `Send` without requiring `Sync` of its parts.

/// Backups hold the whole database: owner-only.
fn create_backup_dir(dir: &Path) -> Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

async fn take_backup<R: Runtime>(runtime: &R, dir: &Path, dest: &Path) -> Check<u64> {
    if let Err(e) = create_backup_dir(dir) {
        return Ok(Err(e));
    }
    rt(runtime.backup(dest).await)
}

/// Start the target; with `restore_first`, stop the server and restore that
/// backup before starting (restore-previous across state formats).
async fn replace<R: Runtime>(
    runtime: &R,
    target: &ReleaseManifest,
    restore_first: Option<&Path>,
) -> Check {
    if let Some(src) = restore_first {
        if let Err(e) = rt(runtime.stop_server().await)? {
            return Ok(Err(format!("stop_server: {e}")));
        }
        if let Err(e) = rt(runtime.restore(src).await)? {
            return Ok(Err(format!("restore of {}: {e}", src.display())));
        }
    }
    Ok(rt(runtime.start_server(&target.images.server).await)?
        .map_err(|e| format!("start_server: {e}")))
}

async fn probe<R: Runtime>(runtime: &R, cfg: &EngineSettings, m: &ReleaseManifest) -> Check {
    if let Err(e) = rt(runtime.wait_healthy(cfg.readiness_deadline).await)? {
        return Ok(Err(format!("wait_healthy: {e}")));
    }
    match rt(runtime.readiness().await)? {
        Ok(report) => Ok(check_ready(&report, m, &cfg.expected_auth_mode)),
        Err(e) => Ok(Err(format!("readiness: {e}"))),
    }
}

async fn ensure_running<R: Runtime>(
    runtime: &R,
    cfg: &EngineSettings,
    m: &ReleaseManifest,
) -> Check {
    if let Err(e) = rt(runtime.start_server(&m.images.server).await)? {
        return Ok(Err(format!("start_server: {e}")));
    }
    probe(runtime, cfg, m).await
}

/// Stop, restore the in-flight backup if needed, start `active`, probe.
async fn bring_back<R: Runtime>(
    runtime: &R,
    cfg: &EngineSettings,
    cur: &InFlight,
    active: &ReleaseManifest,
) -> Check {
    if cur.needs_restore_on_rollback {
        let Some(backup) = &cur.backup else {
            return Ok(Err("no pre-update backup is recorded to restore".into()));
        };
        // Before anything stops: if the restore fails, a manual
        // `docker compose up` must start `active`, not the failed target.
        if let Err(e) = rt(runtime.select_release(&active.images.server).await)? {
            return Ok(Err(format!("select_release: {e}")));
        }
        if let Err(e) = rt(runtime.stop_server().await)? {
            return Ok(Err(format!("stop_server: {e}")));
        }
        if let Err(e) = rt(runtime.restore(backup).await)? {
            return Ok(Err(format!("restore of {}: {e}", backup.display())));
        }
    }
    ensure_running(runtime, cfg, active).await
}

pub struct Engine<R: Runtime, V: ReleaseVerifier> {
    journal: Journal,
    runtime: R,
    verifier: V,
    maintenance: Box<dyn MaintenanceSink + Send>,
    settings: EngineSettings,
    available: Option<ReleaseSummary>,
    observer: Option<StatusObserver>,
}

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
    pub fn new(
        journal: Journal,
        runtime: R,
        verifier: V,
        maintenance: Box<dyn MaintenanceSink + Send>,
        settings: EngineSettings,
    ) -> Self {
        Engine {
            journal,
            runtime,
            verifier,
            maintenance,
            settings,
            available: None,
            observer: None,
        }
    }

    /// Publish status on every step: `observer` is called after each
    /// successful journal write and history append.
    pub fn set_observer(&mut self, observer: StatusObserver) {
        self.observer = Some(observer);
    }

    fn notify(&self) {
        if let Some(f) = &self.observer {
            f(&self.status());
        }
    }

    /// Show a request as in progress while its release is fetched and
    /// verified, before `Requested` is persisted (nothing is journaled yet,
    /// so a crash here leaves nothing to recover). If install then refuses
    /// without persisting, the history append publishes the real (idle)
    /// state again.
    fn notify_taken(&self, req: &UpdateRequest, started_at: DateTime<Utc>) {
        if let Some(f) = &self.observer {
            let mut s = self.status();
            s.state = UpdateState::InProgress {
                request_id: req.id,
                target: req.version.clone(),
                step: Step::Requested,
                started_at,
            };
            f(&s);
        }
    }

    pub fn journal_state(&self) -> &JournalState {
        self.journal.state()
    }

    /// The newest compatible release seen on the channel, for `status()`.
    pub fn set_available(&mut self, available: Option<ReleaseSummary>) {
        if self.available != available {
            self.available = available;
            self.notify();
        }
    }

    /// Restore `available` from an earlier run (the last status.json),
    /// keeping it only if it is still newer than the active release.
    pub fn seed_available(&mut self, available: Option<ReleaseSummary>) {
        let active = self.active_version();
        let newer = available.filter(|a| active.as_ref().is_none_or(|v| a.version > *v));
        self.set_available(newer);
    }

    /// `target` is now active: an `available` release it covers (same or
    /// older version) is no longer an update.
    fn installed(&mut self, target: &Version) {
        if self
            .available
            .as_ref()
            .is_some_and(|a| a.version <= *target)
        {
            self.available = None;
        }
    }

    /// Fetch and verify the channel's current release (read-only).
    pub async fn fetch_channel(&mut self) -> Result<ReleaseManifest, String> {
        fetch_verified(&self.runtime, &self.verifier, ReleaseSelector::Channel)
            .await
            .map_err(|h| h.0)?
    }

    /// Poll the channel: `available` becomes its release when that is newer
    /// than the active one and `check_upgrade` accepts it (against the
    /// active release's state format), and `None` otherwise. A fetch or
    /// verification failure leaves `available` unchanged and is returned.
    pub async fn refresh_available(&mut self) -> Result<Option<ReleaseSummary>, String> {
        let target = self.fetch_channel().await?;
        let active = self.journal.state().active.clone();
        let found = active.as_ref().map(|a| a.state_format);
        let available = match check_upgrade(active.as_ref(), found, &target) {
            Ok(_) => Some(summary(&target)),
            Err(refusal) => {
                tracing::debug!(version = %target.version, %refusal, "channel release not offered");
                None
            }
        };
        self.set_available(available.clone());
        Ok(available)
    }

    /// Make the maintenance marker match the journal: on while anything is
    /// in flight (including `FailedNeedsOperator`), off when idle.
    pub fn sync_maintenance(&self) -> Result<(), RuntimeError> {
        if self.journal.state().current.is_some() {
            self.maintenance.enter()
        } else {
            self.maintenance.leave()
        }
    }

    /// Record a request that is not acted on (malformed, or a repeat that
    /// arrived while an update ran) as `Rejected`.
    pub fn reject(
        &mut self,
        request_id: Option<Uuid>,
        to: Option<Version>,
        detail: String,
    ) -> Res<HistoryEntry> {
        let now = Utc::now();
        self.log_only(HistoryEntry {
            request_id,
            from: self.active_version(),
            to,
            outcome: Outcome::Rejected,
            detail,
            started_at: now,
            finished_at: now,
        })
    }

    pub fn status(&self) -> UpdateStatus {
        let s = self.journal.state();
        let history = self
            .journal
            .recent_history(STATUS_HISTORY)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "cannot read update history");
                Vec::new()
            });
        let recorded = match &s.current {
            Some(c) if c.rollback_attempted => match self.recorded_outcome(c.request_id) {
                Recorded::Entry(e) => Some(e),
                Recorded::Nothing | Recorded::Unreadable(_) => None,
            },
            _ => None,
        };
        let state = update_state(s.current.as_ref(), recorded.as_ref());
        UpdateStatus {
            updater_version: env!("CARGO_PKG_VERSION").to_string(),
            state,
            active: s.active.as_ref().map(summary),
            previous: s.previous.as_ref().map(summary),
            available: self.available.clone(),
            history,
            updated_at: Utc::now(),
        }
    }

    // ---------------------------------------------------------------- install

    pub async fn install(&mut self, req: UpdateRequest) -> Res<HistoryEntry> {
        let started_at = Utc::now();
        let from = self.active_version();
        let entry = |outcome, detail: String| HistoryEntry {
            request_id: Some(req.id),
            from: from.clone(),
            to: req.version.clone(),
            outcome,
            detail,
            started_at,
            finished_at: Utc::now(),
        };
        if self.journal.state().current.is_some() {
            return self.log_only(entry(Outcome::Rejected, UPDATE_IN_PROGRESS.into()));
        }
        self.notify_taken(&req, started_at);

        // Fetch, verify and parse: read-only, nothing to recover yet.
        let selector = match &req.version {
            Some(v) => ReleaseSelector::Version(v.clone()),
            None => ReleaseSelector::Channel,
        };
        let target = match fetch_verified(&self.runtime, &self.verifier, selector).await? {
            Ok(m) => m,
            Err(e) => return self.log_only(entry(Outcome::Refused, e)),
        };
        if let Some(v) = &req.version
            && *v != target.version
        {
            return self.log_only(entry(
                Outcome::Refused,
                format!(
                    "requested {v} but the fetched manifest is for {}",
                    target.version
                ),
            ));
        }

        let mut cur = InFlight {
            request_id: req.id,
            target,
            step: Step::Requested,
            backup: None,
            needs_restore_on_rollback: false,
            started_at,
            rollback_attempted: false,
        };
        self.persist(&cur)?;
        let found = match rt(self.runtime.server_state_format().await)? {
            Ok(f) => f,
            Err(e) => {
                let detail = format!("cannot read the installed state format: {e}");
                return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
            }
        };
        match check_upgrade(self.journal.state().active.as_ref(), found, &cur.target) {
            Ok(plan) => cur.needs_restore_on_rollback = plan.needs_restore_on_rollback,
            Err(refusal) => {
                let detail = refusal.to_string();
                return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
            }
        }
        self.forward(cur, Mode::Install).await
    }

    // -------------------------------------------------------- restore-previous

    /// Make `previous` active again. Same state format: swap images. Different
    /// formats: needs `allow_restore`, then restores `active_backup` (losing
    /// changes made since it was taken). In `FailedNeedsOperator`, retries
    /// bringing back the active release once instead.
    pub async fn restore_previous(&mut self, allow_restore: bool) -> Res<HistoryEntry> {
        let started_at = Utc::now();
        let s = self.journal.state().clone();
        let from = self.active_version();
        if let Some(cur) = s.current {
            if cur.rollback_attempted {
                return self.retry_active(cur, allow_restore, started_at).await;
            }
            return self.log_only(HistoryEntry {
                request_id: None,
                from,
                to: None,
                outcome: Outcome::Rejected,
                detail: UPDATE_IN_PROGRESS.into(),
                started_at,
                finished_at: Utc::now(),
            });
        }
        let refuse = |to: Option<Version>, detail: String| HistoryEntry {
            request_id: None,
            from: from.clone(),
            to,
            outcome: Outcome::Refused,
            detail,
            started_at,
            finished_at: Utc::now(),
        };
        let (Some(active), Some(prev)) = (s.active, s.previous) else {
            return self.log_only(refuse(
                None,
                "no previous release is recorded; nothing to restore".into(),
            ));
        };
        let to = Some(prev.version.clone());
        let formats_differ = prev.state_format != active.state_format;
        if formats_differ {
            let Some(backup) = &s.active_backup else {
                return self.log_only(refuse(
                    to,
                    format!(
                        "{} uses state format {} but {} uses {}, and no backup of {}'s data is recorded; it cannot be restored automatically",
                        prev.version, prev.state_format, active.version, active.state_format, prev.version
                    ),
                ));
            };
            if !matches!(backup.try_exists(), Ok(true)) {
                return self.log_only(refuse(
                    to,
                    format!(
                        "{} uses state format {} but {} uses {}, and the backup of {}'s data ({}) is missing; it cannot be restored automatically",
                        prev.version,
                        prev.state_format,
                        active.version,
                        active.state_format,
                        prev.version,
                        backup.display()
                    ),
                ));
            }
            if !allow_restore {
                return self.log_only(refuse(
                    to,
                    format!(
                        "{} uses state format {} but {} uses {}, so {} cannot read the current data. Re-run with --restore-backup to restore {}, the backup taken before {} was installed; all changes made since that backup will be lost",
                        prev.version,
                        prev.state_format,
                        active.version,
                        active.state_format,
                        prev.version,
                        backup.display(),
                        active.version
                    ),
                ));
            }
        } else {
            // No restore: `prev` runs on the data as it is now, so it must be
            // able to read it (as `install` checks). With a restore the data
            // is replaced by the backup `prev` last ran on.
            match rt(self.runtime.server_state_format().await)? {
                Ok(Some(found)) if !prev.readable_state_formats.contains(&found) => {
                    return self.log_only(refuse(
                        to,
                        format!(
                            "installed data uses state format {found}, which {} reads only as {:?}; it cannot be restored without a compatible backup",
                            prev.version, prev.readable_state_formats
                        ),
                    ));
                }
                Ok(_) => {}
                Err(e) => {
                    return self.log_only(refuse(
                        to,
                        format!("cannot read the installed state format: {e}"),
                    ));
                }
            }
        }
        let cur = InFlight {
            request_id: Uuid::new_v4(),
            target: prev,
            step: Step::Requested,
            backup: None,
            needs_restore_on_rollback: formats_differ,
            started_at,
            rollback_attempted: false,
        };
        self.persist(&cur)?;
        self.forward(cur, Mode::RestorePrevious).await
    }

    /// Operator-initiated second attempt after `FailedNeedsOperator`: bring
    /// back the active release, restoring the in-flight backup if the failed
    /// target may have changed the data.
    async fn retry_active(
        &mut self,
        mut cur: InFlight,
        allow_restore: bool,
        started_at: DateTime<Utc>,
    ) -> Res<HistoryEntry> {
        match cur.step {
            // At `Active` the swap is done: `active` is the target, and the
            // in-flight backup holds the other release's data. Never restore
            // it (ruling 23).
            Step::Active => cur.needs_restore_on_rollback = false,
            // Before `Replacing` the target never ran, so the data is the
            // active release's own, and at `BackedUp` the dump may be
            // partial. Nothing to restore.
            Step::Requested
            | Step::Validated
            | Step::Pulled
            | Step::Maintenance
            | Step::BackedUp => cur.needs_restore_on_rollback = false,
            // The target may have run: `--restore-backup` restores the
            // complete pre-update backup even when the state formats say
            // it is not needed (a release that changed the data without
            // changing its state format, for one).
            Step::Replacing | Step::Probing | Step::RollingBack | Step::Unknown => {
                if allow_restore && cur.backup.is_some() {
                    cur.needs_restore_on_rollback = true;
                }
            }
        }
        let refuse = |detail: String| HistoryEntry {
            // No request id: this must not read as the in-flight outcome.
            request_id: None,
            from: self.active_version(),
            to: self.active_version(),
            outcome: Outcome::Refused,
            detail,
            started_at,
            finished_at: Utc::now(),
        };
        let Some(active) = self.journal.state().active.clone() else {
            let e = refuse("no active release is recorded to restore".into());
            return self.log_only(e);
        };
        if cur.needs_restore_on_rollback
            && !cur
                .backup
                .as_ref()
                .is_some_and(|b| matches!(b.try_exists(), Ok(true)))
        {
            let e = refuse(format!(
                "bringing back {} needs the pre-update backup {}, which is missing; restore the database by hand",
                active.version,
                cur.backup
                    .as_ref()
                    .map_or("<none recorded>".into(), |b| b.display().to_string())
            ));
            return self.log_only(e);
        }
        if cur.needs_restore_on_rollback && !allow_restore {
            let backup = cur
                .backup
                .as_ref()
                .map_or("<none recorded>".into(), |b| b.display().to_string());
            let e = refuse(format!(
                "the failed update to {} may have changed the data, so bringing back {} needs the backup {backup}. Re-run with --restore-backup to restore it; all changes made since that backup will be lost",
                cur.target.version, active.version
            ));
            return self.log_only(e);
        }
        match bring_back(&self.runtime, &self.settings, &cur, &active).await? {
            Ok(()) => {
                let detail = format!(
                    "operator restore: {} is running again after the failed update to {}",
                    active.version, cur.target.version
                );
                self.conclude(self.op_entry(&cur, Outcome::RolledBack, detail), true)
            }
            Err(e) => {
                let detail = format!(
                    "operator restore of {} failed: {e}. {}",
                    active.version,
                    Self::operator_steps(&cur)
                );
                self.needs_operator(&cur, detail)
            }
        }
    }

    // ---------------------------------------------------------------- recover

    /// Resolve whatever the journal says was in flight. Call once at startup,
    /// before taking requests. `None`: nothing to do.
    pub async fn recover(&mut self) -> Res<Option<HistoryEntry>> {
        let Some(cur) = self.journal.state().current.clone() else {
            // Idle means maintenance is off; clear a marker a crash left.
            if let Err(e) = rt(self.maintenance.leave())? {
                tracing::warn!(error = %e, "cannot clear the maintenance marker");
            }
            return Ok(None);
        };
        if cur.step == Step::Active {
            return self.finish_active(cur);
        }
        match self.recorded_outcome(cur.request_id) {
            Recorded::Nothing => {}
            Recorded::Entry(done) if done.outcome == Outcome::FailedNeedsOperator => {
                // Already waiting for the operator; make sure maintenance is on.
                if let Err(e) = rt(self.maintenance.enter())? {
                    tracing::warn!(error = %e, "cannot enter maintenance");
                }
                return Ok(None);
            }
            Recorded::Entry(done) if done.outcome == Outcome::Unknown => {
                let detail = format!(
                    "history holds an outcome this updater does not understand for the update to {}; it cannot tell what happened. {}",
                    cur.target.version,
                    Self::operator_steps(&cur)
                );
                return Ok(Some(self.hold_for_operator(cur, detail)?));
            }
            Recorded::Entry(_) => {
                // Outcome recorded, journal not yet cleared.
                if let Err(e) = rt(self.maintenance.leave())? {
                    tracing::warn!(error = %e, "cannot clear the maintenance marker");
                }
                self.clear_current()?;
                return Ok(None);
            }
            Recorded::Unreadable(e) => {
                let detail = format!(
                    "history is unreadable ({e}), so whether the update to {} already finished cannot be told. {}",
                    cur.target.version,
                    Self::operator_steps(&cur)
                );
                return Ok(Some(self.hold_for_operator(cur, detail)?));
            }
        }
        let why = format!("interrupted at step {}", step_name(cur.step));
        let entry = match (cur.step, cur.rollback_attempted) {
            (Step::Requested | Step::Validated | Step::Pulled, _) => {
                self.abort_recovered(cur, why, false).await?
            }
            (Step::Maintenance | Step::BackedUp, _) => {
                if cur.step == Step::BackedUp {
                    self.remove_backup(cur.backup.as_deref());
                }
                self.abort_recovered(cur, why, true).await?
            }
            (Step::Replacing | Step::Probing | Step::RollingBack, false) => {
                self.fail_forward(cur, why).await?
            }
            // Handled before the history lookup; kept total without a panic.
            (Step::Active, _) => return self.finish_active(cur),
            (Step::Replacing | Step::Probing | Step::RollingBack, true) | (Step::Unknown, _) => {
                let detail = format!(
                    "{why} during the rollback from {}; the rollback is not retried automatically. {}",
                    cur.target.version,
                    Self::operator_steps(&cur)
                );
                self.needs_operator(&cur, detail)?
            }
        };
        Ok(Some(entry))
    }

    /// `Active` is persisted together with the swap, so the journal alone
    /// proves the target is installed: finish as `Succeeded` whatever
    /// history says. Only a known, already-recorded outcome is not appended
    /// again; an unknown or unreadable one is not trusted, and failing to
    /// append the line does not stop the finish.
    fn finish_active(&mut self, cur: InFlight) -> Res<Option<HistoryEntry>> {
        let recorded = match self.recorded_outcome(cur.request_id) {
            Recorded::Entry(e) => {
                !matches!(e.outcome, Outcome::Unknown | Outcome::FailedNeedsOperator)
            }
            Recorded::Nothing | Recorded::Unreadable(_) => false,
        };
        self.installed(&cur.target.version);
        let entry = if recorded {
            None
        } else {
            let s = self.journal.state();
            let entry = HistoryEntry {
                request_id: Some(cur.request_id),
                from: s.previous.as_ref().map(|m| m.version.clone()),
                to: Some(cur.target.version.clone()),
                outcome: Outcome::Succeeded,
                detail: format!(
                    "{} is active (finished by recovery: interrupted at step active)",
                    cur.target.version
                ),
                started_at: cur.started_at,
                finished_at: Utc::now(),
            };
            if let Err(e) = self.append(entry.clone()) {
                tracing::warn!(error = %e, "cannot record the finished update in history");
            }
            Some(entry)
        };
        if let Err(e) = rt(self.maintenance.leave())? {
            tracing::warn!(error = %e, "cannot leave maintenance");
        }
        self.clear_current()?;
        Ok(entry)
    }

    /// Abandon an update that never replaced the active release, then make
    /// sure the active release is up.
    async fn abort_recovered(
        &mut self,
        cur: InFlight,
        why: String,
        leave: bool,
    ) -> Res<HistoryEntry> {
        let mut detail = format!("{why}; update to {} abandoned", cur.target.version);
        if let Some(active) = self.journal.state().active.clone() {
            // The active release was never replaced before `Replacing`: if it
            // still passes its probe, leave it running rather than recreate
            // it. Only a failed probe recreates it.
            let checked = match probe(&self.runtime, &self.settings, &active).await? {
                Ok(()) => Ok(()),
                Err(first) => {
                    tracing::warn!(error = %first, version = %active.version, "active release failed its probe during recovery; recreating it");
                    ensure_running(&self.runtime, &self.settings, &active).await?
                }
            };
            match checked {
                Ok(()) => detail.push_str(&format!("; {} is running", active.version)),
                Err(e) => detail.push_str(&format!(
                    "; warning: {} did not pass its readiness check: {e}",
                    active.version
                )),
            }
        }
        self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), leave)
    }

    // --------------------------------------------------------- forward steps

    /// Steps `Validated` through `Active`. `cur` is persisted at `Requested`.
    async fn forward(&mut self, mut cur: InFlight, mode: Mode) -> Res<HistoryEntry> {
        // Validated: disk space for the backup.
        self.set_step(&mut cur, Step::Validated)?;
        let dir = self.settings.backup_dir.clone();
        // statvfs needs the directory to exist (first run).
        if let Err(e) = create_backup_dir(&dir) {
            return self.conclude(self.op_entry(&cur, Outcome::Refused, e), false);
        }
        let need = self
            .settings
            .min_free_bytes
            .max(last_backup_size(&dir).saturating_mul(3));
        match rt(self.runtime.free_bytes(&dir).await)? {
            Ok(free) if free >= need => {}
            Ok(free) => {
                let detail = format!(
                    "not enough free space in {}: {free} bytes free, {need} needed (the larger of min_free_bytes and 3x the last backup); free some space and retry",
                    dir.display()
                );
                return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
            }
            Err(e) => {
                let detail = format!("cannot check free space in {}: {e}", dir.display());
                return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
            }
        }

        // Pulled: download before anything stops.
        self.set_step(&mut cur, Step::Pulled)?;
        let image = cur.target.images.server.clone();
        if let Err(e) = rt(self.runtime.pull(&image).await)? {
            let detail = format!("pull of {image} failed: {e}");
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), false);
        }
        let want = cur.target.state_format.to_string();
        match rt(self.runtime.image_labels(&image).await)? {
            Ok(labels) if labels.get(STATE_FORMAT_LABEL) == Some(&want) => {}
            Ok(labels) => {
                let detail = format!(
                    "image {image} has label {STATE_FORMAT_LABEL}={:?} but the manifest says {want}",
                    labels.get(STATE_FORMAT_LABEL)
                );
                return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), false);
            }
            Err(e) => {
                let detail = format!("cannot read labels of {image}: {e}");
                return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), false);
            }
        }

        // Free-space precheck (DEP8), after the pull has used its space and
        // before anything stops: the dump needs the newest backup's size
        // plus 10%.
        match rt(self.runtime.free_bytes(&dir).await)? {
            Ok(free) => {
                if let Err(e) = check_backup_space(&dir, free, last_backup_size(&dir)) {
                    return self
                        .conclude(self.op_entry(&cur, Outcome::Refused, e.to_string()), false);
                }
            }
            Err(e) => {
                let detail = format!("cannot check free space in {}: {e}", dir.display());
                return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
            }
        }

        // Maintenance.
        self.set_step(&mut cur, Step::Maintenance)?;
        if let Err(e) = rt(self.maintenance.enter())? {
            let detail = format!("cannot enter maintenance: {e}");
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), true);
        }
        // Drain: a mutation that passed the gate just before the marker
        // appeared may still be committing; let it finish before the dump.
        if !self.settings.maintenance_drain.is_zero() {
            tokio::time::sleep(self.settings.maintenance_drain).await;
        }

        // BackedUp: the path is persisted before the dump starts.
        let from = self
            .active_version()
            .map_or_else(|| "none".to_string(), |v| v.to_string());
        let dest = dir.join(format!(
            "{}-{from}-{}.dump",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            cur.target.version
        ));
        // Checked before the path is journaled: the in-flight backup (which
        // failure and recovery delete) is always a file this attempt made.
        if dest.exists() {
            let detail = format!(
                "backup failed, update abandoned before replacement: {} already exists",
                dest.display()
            );
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), true);
        }
        cur.backup = Some(dest.clone());
        self.set_step(&mut cur, Step::BackedUp)?;
        if let Err(e) = take_backup(&self.runtime, &dir, &dest).await? {
            self.remove_backup(Some(&dest));
            let detail = format!(
                "backup failed, update abandoned before replacement{}: {e}",
                self.active_version()
                    .map_or(String::new(), |v| format!(" ({v} left running)"))
            );
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), true);
        }
        self.prune_backups(&dest);

        // Replacing.
        self.set_step(&mut cur, Step::Replacing)?;
        let restore_first = match mode {
            Mode::RestorePrevious if cur.needs_restore_on_rollback => {
                match self.journal.state().active_backup.clone() {
                    Some(b) => Some(b),
                    None => {
                        let check = "no backup recorded to restore".to_string();
                        return self.fail_forward(cur, check).await;
                    }
                }
            }
            _ => None,
        };
        if let Err(check) = replace(&self.runtime, &cur.target, restore_first.as_deref()).await? {
            return self.fail_forward(cur, check).await;
        }

        // Probing.
        self.set_step(&mut cur, Step::Probing)?;
        if let Err(check) = probe(&self.runtime, &self.settings, &cur.target).await? {
            return self.fail_forward(cur, check).await;
        }

        // Active: swap, persisted with the step so recovery can finish.
        let mut s = self.journal.state().clone();
        let old = s.active.take();
        s.previous = old.clone();
        s.active = Some(cur.target.clone());
        s.active_backup = match mode {
            Mode::Install => cur.backup.clone(),
            // Ruling 17: nothing recorded to go forward with again.
            Mode::RestorePrevious => None,
        };
        cur.step = Step::Active;
        s.current = Some(cur.clone());
        self.installed(&cur.target.version);
        self.record(s)?;
        let detail = match mode {
            Mode::Install => format!("{} is active", cur.target.version),
            Mode::RestorePrevious if cur.needs_restore_on_rollback => format!(
                "restored previous release {} and its data backup",
                cur.target.version
            ),
            Mode::RestorePrevious => format!("restored previous release {}", cur.target.version),
        };
        let entry = HistoryEntry {
            request_id: Some(cur.request_id),
            from: old.map(|m| m.version),
            to: Some(cur.target.version.clone()),
            outcome: Outcome::Succeeded,
            detail,
            started_at: cur.started_at,
            finished_at: Utc::now(),
        };
        let out = self.conclude(entry, true)?;
        // Image prune (DEP7c): keep only the current and previous release.
        prune_release_images(&self.runtime, self.release_images()).await;
        Ok(out)
    }

    /// Images of the active and previous release: what image pruning keeps.
    fn release_images(&self) -> Vec<String> {
        let s = self.journal.state();
        s.active
            .iter()
            .chain(s.previous.iter())
            .flat_map(|m| [m.images.server.clone(), m.images.worker.clone()])
            .collect()
    }

    // ----------------------------------------------------- failure, rollback

    /// The target was (possibly) started and failed `check`.
    async fn fail_forward(&mut self, cur: InFlight, check: String) -> Res<HistoryEntry> {
        let Some(active) = self.journal.state().active.clone() else {
            // First install: nothing to roll back to.
            let mut detail = format!(
                "first install of {} failed: {check}; server stopped",
                cur.target.version
            );
            if let Err(e) = rt(self.runtime.stop_server().await)? {
                detail.push_str(&format!(" (stop_server also failed: {e})"));
            }
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), true);
        };
        self.rollback(cur, active, check).await
    }

    /// The single rollback attempt.
    async fn rollback(
        &mut self,
        mut cur: InFlight,
        active: ReleaseManifest,
        check: String,
    ) -> Res<HistoryEntry> {
        cur.step = Step::RollingBack;
        cur.rollback_attempted = true;
        self.persist(&cur)?;
        match bring_back(&self.runtime, &self.settings, &cur, &active).await? {
            Ok(()) => {
                let restored = if cur.needs_restore_on_rollback {
                    " and its data restored from the pre-update backup"
                } else {
                    ""
                };
                let detail = format!(
                    "{} failed ({check}); rolled back to {}{restored}",
                    cur.target.version, active.version
                );
                self.conclude(self.op_entry(&cur, Outcome::RolledBack, detail), true)
            }
            Err(e) => {
                let detail = format!(
                    "{} failed ({check}); the rollback to {} also failed ({e}). {}",
                    cur.target.version,
                    active.version,
                    Self::operator_steps(&cur)
                );
                self.needs_operator(&cur, detail)
            }
        }
    }

    fn operator_steps(cur: &InFlight) -> String {
        // As `retry_active` decides: a restore only once the target may
        // have run, and never at `Active`.
        let may_restore = matches!(
            cur.step,
            Step::Replacing | Step::Probing | Step::RollingBack | Step::Unknown
        );
        let restore = if cur.needs_restore_on_rollback && may_restore {
            format!(
                " --restore-backup (restores {})",
                cur.backup
                    .as_ref()
                    .map_or("<no backup recorded>".into(), |b| b.display().to_string())
            )
        } else {
            String::new()
        };
        format!(
            "Maintenance stays on and nothing further is tried automatically. Inspect the server and database (docker compose logs server), then run `aq-updater restore-previous{restore}` to retry bringing back the active release"
        )
    }

    // ------------------------------------------------------- journal helpers

    fn active_version(&self) -> Option<Version> {
        self.journal
            .state()
            .active
            .as_ref()
            .map(|m| m.version.clone())
    }

    fn op_entry(&self, cur: &InFlight, outcome: Outcome, detail: String) -> HistoryEntry {
        HistoryEntry {
            request_id: Some(cur.request_id),
            from: self.active_version(),
            to: Some(cur.target.version.clone()),
            outcome,
            detail,
            started_at: cur.started_at,
            finished_at: Utc::now(),
        }
    }

    /// Journal write plus status notification.
    fn record(&mut self, s: JournalState) -> Result<(), JournalError> {
        self.journal.record(s)?;
        self.notify();
        Ok(())
    }

    /// History append plus status notification.
    fn append(&self, entry: HistoryEntry) -> Result<(), JournalError> {
        self.journal.append_history(entry)?;
        self.notify();
        Ok(())
    }

    fn persist(&mut self, cur: &InFlight) -> Res<()> {
        let mut s = self.journal.state().clone();
        s.current = Some(cur.clone());
        self.record(s)?;
        Ok(())
    }

    fn set_step(&mut self, cur: &mut InFlight, step: Step) -> Res<()> {
        cur.step = step;
        self.persist(cur)
    }

    fn clear_current(&mut self) -> Res<()> {
        let mut s = self.journal.state().clone();
        s.current = None;
        self.record(s)?;
        Ok(())
    }

    /// A history entry with no in-flight state behind it.
    fn log_only(&mut self, entry: HistoryEntry) -> Res<HistoryEntry> {
        self.append(entry.clone())?;
        Ok(entry)
    }

    /// Terminal outcome: record history, optionally leave maintenance, then
    /// clear `current`. History comes first: until the outcome is recorded a
    /// crash must leave maintenance on (recovery may decide
    /// `FailedNeedsOperator`); once it is recorded, recovery only finishes
    /// the leave and the clear.
    fn conclude(&mut self, entry: HistoryEntry, leave: bool) -> Res<HistoryEntry> {
        self.append(entry.clone())?;
        if leave && let Err(e) = rt(self.maintenance.leave())? {
            tracing::warn!(error = %e, "cannot leave maintenance; recovery at the next start clears it");
        }
        self.clear_current()?;
        Ok(entry)
    }

    /// Record `FailedNeedsOperator`: maintenance is (re-)entered and
    /// `current` is kept.
    fn needs_operator(&mut self, cur: &InFlight, mut detail: String) -> Res<HistoryEntry> {
        if let Err(e) = rt(self.maintenance.enter())? {
            tracing::warn!(error = %e, "cannot enter maintenance");
            detail.push_str(&format!(" Warning: could not enter maintenance: {e}"));
        }
        let entry = self.op_entry(cur, Outcome::FailedNeedsOperator, detail);
        self.append(entry.clone())?;
        Ok(entry)
    }

    /// "Cannot tell what happened": stop automatic action for this update
    /// (marked as if its rollback had been attempted, so status shows it and
    /// `restore_previous` can retry) and wait for the operator.
    fn hold_for_operator(&mut self, mut cur: InFlight, detail: String) -> Res<HistoryEntry> {
        cur.rollback_attempted = true;
        self.persist(&cur)?;
        self.needs_operator(&cur, detail)
    }

    /// The newest non-`Rejected` history entry for `request_id`.
    fn recorded_outcome(&self, request_id: Uuid) -> Recorded {
        match self.journal.recent_history(usize::MAX) {
            Ok(h) => h
                .into_iter()
                .find(|e| e.request_id == Some(request_id) && e.outcome != Outcome::Rejected)
                .map_or(Recorded::Nothing, Recorded::Entry),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read update history");
                Recorded::Unreadable(e.to_string())
            }
        }
    }

    // -------------------------------------------------------- backup files

    fn remove_backup(&self, path: Option<&Path>) {
        let Some(p) = path else { return };
        if self.journal.state().active_backup.as_deref() == Some(p) {
            return;
        }
        match std::fs::remove_file(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "cannot remove partial backup")
            }
        }
    }

    /// Keep the newest `backups_keep` dumps; never delete `active_backup` or
    /// the in-flight backup.
    fn prune_backups(&self, in_flight: &Path) {
        let protected = self.journal.state().active_backup.as_deref();
        for p in dumps(&self.settings.backup_dir)
            .into_iter()
            .skip(self.settings.backups_keep)
        {
            if p == in_flight || Some(p.as_path()) == protected {
                continue;
            }
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::warn!(path = %p.display(), error = %e, "cannot prune backup");
            }
        }
    }
}
