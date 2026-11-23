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

mod backup;
mod forward;
mod images;
mod readiness;
mod recovery;
mod restore;
mod retry;

pub use backup::{InsufficientBackupSpace, backup_space_needed, check_backup_space};
pub use images::{image_repository, images_to_prune};
pub use readiness::check_ready;

use retry::bring_back;

use crate::config::UpdaterConfig;
use crate::journal::{InFlight, Journal, JournalError, JournalState};
use crate::runtime::{ReleaseSelector, Runtime, RuntimeError};
use crate::verify::ReleaseVerifier;
use aq_release::{
    HistoryEntry, Outcome, ReleaseManifest, ReleaseSummary, Step, UpdateRequest, UpdateState,
    UpdateStatus, check_upgrade,
};
use chrono::{DateTime, Utc};
use semver::Version;
use std::path::PathBuf;
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
}
