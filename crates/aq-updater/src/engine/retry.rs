//! Bring the active release back after a failed rollback.

use super::readiness::ensure_running;
use super::{Check, Engine, EngineSettings, Res, rt};
use crate::journal::InFlight;
use crate::runtime::Runtime;
use crate::verify::ReleaseVerifier;
use aq_release::{HistoryEntry, Outcome, ReleaseManifest, Step};
use chrono::{DateTime, Utc};

/// Stop, restore the in-flight backup if needed, start `active`, probe.
pub(super) async fn bring_back<R: Runtime>(
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

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
    /// Operator-initiated second attempt after `FailedNeedsOperator`: bring
    /// back the active release, restoring the in-flight backup if the failed
    /// target may have changed the data.
    pub(super) async fn retry_active(
        &mut self,
        mut cur: InFlight,
        allow_restore: bool,
        started_at: DateTime<Utc>,
    ) -> Res<HistoryEntry> {
        choose_retry_backup(&mut cur, allow_restore);
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
        if let Err(detail) = retry_backup_ready(&cur, &active, allow_restore) {
            return self.log_only(refuse(detail));
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
}

fn choose_retry_backup(cur: &mut InFlight, allow_restore: bool) {
    match cur.step {
        // At `Active` the swap is done: `active` is the target, and the
        // in-flight backup holds the other release's data. Never restore
        // it (ruling 23).
        Step::Active => cur.needs_restore_on_rollback = false,
        // Before `Replacing` the target never ran, so the data is the
        // active release's own, and at `BackedUp` the dump may be
        // partial. Nothing to restore.
        Step::Requested | Step::Validated | Step::Pulled | Step::Maintenance | Step::BackedUp => {
            cur.needs_restore_on_rollback = false
        }
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
}

fn retry_backup_ready(
    cur: &InFlight,
    active: &ReleaseManifest,
    allow_restore: bool,
) -> Result<(), String> {
    if !cur.needs_restore_on_rollback {
        return Ok(());
    }
    let backup = cur.backup.as_ref();
    let display = backup.map_or_else(|| "<none recorded>".into(), |b| b.display().to_string());
    if !backup.is_some_and(|b| matches!(b.try_exists(), Ok(true))) {
        return Err(format!(
            "bringing back {} needs the pre-update backup {display}, which is missing; restore the database by hand",
            active.version
        ));
    }
    if !allow_restore {
        return Err(format!(
            "the failed update to {} may have changed the data, so bringing back {} needs the backup {display}. Re-run with --restore-backup to restore it; all changes made since that backup will be lost",
            cur.target.version, active.version
        ));
    }
    Ok(())
}
