//! Operator-initiated release and backup restoration.

use super::{Check, Engine, Mode, Res, UPDATE_IN_PROGRESS, rt};
use crate::journal::InFlight;
use crate::runtime::Runtime;
use crate::verify::ReleaseVerifier;
use aq_release::{HistoryEntry, Outcome, ReleaseManifest, Step};
use chrono::Utc;
use semver::Version;
use std::path::Path;
use uuid::Uuid;

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
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
        let formats_differ = prev.state_format != active.state_format;
        let eligible = if formats_differ {
            previous_backup_ready(&active, &prev, s.active_backup.as_deref(), allow_restore)
        } else {
            // Without restoring a backup, the previous release has to read the current data.
            previous_can_read(&self.runtime, &prev).await?
        };
        if let Err(detail) = eligible {
            return self.log_only(refuse(Some(prev.version.clone()), detail));
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
}

fn previous_backup_ready(
    active: &ReleaseManifest,
    previous: &ReleaseManifest,
    backup: Option<&Path>,
    allow_restore: bool,
) -> Result<(), String> {
    let Some(backup) = backup else {
        return Err(format!(
            "{} uses state format {} but {} uses {}, and no backup of {}'s data is recorded; it cannot be restored automatically",
            previous.version,
            previous.state_format,
            active.version,
            active.state_format,
            previous.version
        ));
    };
    if !matches!(backup.try_exists(), Ok(true)) {
        return Err(format!(
            "{} uses state format {} but {} uses {}, and the backup of {}'s data ({}) is missing; it cannot be restored automatically",
            previous.version,
            previous.state_format,
            active.version,
            active.state_format,
            previous.version,
            backup.display()
        ));
    }
    if !allow_restore {
        return Err(format!(
            "{} uses state format {} but {} uses {}, so {} cannot read the current data. Re-run with --restore-backup to restore {}, the backup taken before {} was installed; all changes made since that backup will be lost",
            previous.version,
            previous.state_format,
            active.version,
            active.state_format,
            previous.version,
            backup.display(),
            active.version
        ));
    }
    Ok(())
}

async fn previous_can_read<R: Runtime>(runtime: &R, previous: &ReleaseManifest) -> Check {
    Ok(match rt(runtime.server_state_format().await)? {
        Ok(Some(found)) if !previous.readable_state_formats.contains(&found) => Err(format!(
            "installed data uses state format {found}, which {} reads only as {:?}; it cannot be restored without a compatible backup",
            previous.version, previous.readable_state_formats
        )),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("cannot read the installed state format: {e}")),
    })
}
