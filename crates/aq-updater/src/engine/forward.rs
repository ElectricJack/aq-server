//! Persist and execute the steps of an update.

use super::backup::{initial_space_check, post_pull_space_check};
use super::images::{prune_release_images, pull_release};
use super::readiness::probe;
use super::{Check, Engine, Mode, Res, rt};
use crate::journal::InFlight;
use crate::runtime::Runtime;
use crate::verify::ReleaseVerifier;
use aq_release::{HistoryEntry, Outcome, ReleaseManifest, Step};
use chrono::Utc;
use std::path::Path;

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

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
    /// Steps `Validated` through `Active`. `cur` is persisted at `Requested`.
    pub(super) async fn forward(&mut self, mut cur: InFlight, mode: Mode) -> Res<HistoryEntry> {
        self.set_step(&mut cur, Step::Validated)?;
        let dir = self.settings.backup_dir.clone();
        if let Err(detail) =
            initial_space_check(&self.runtime, &dir, self.settings.min_free_bytes).await?
        {
            return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
        }

        // Download before anything stops.
        self.set_step(&mut cur, Step::Pulled)?;
        if let Err(detail) = pull_release(&self.runtime, &cur.target).await? {
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), false);
        }
        if let Err(detail) = post_pull_space_check(&self.runtime, &dir).await? {
            return self.conclude(self.op_entry(&cur, Outcome::Refused, detail), false);
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

        if let Err(detail) = self.back_up_update(&mut cur, &dir).await? {
            return self.conclude(self.op_entry(&cur, Outcome::Aborted, detail), true);
        }

        if let Err(check) = self.replace_and_probe(&mut cur, mode).await? {
            return self.fail_forward(cur, check).await;
        }

        let out = self.activate_release(cur, mode)?;
        // Image prune (DEP7c): keep only the current and previous release.
        prune_release_images(&self.runtime, self.release_images()).await;
        Ok(out)
    }

    async fn replace_and_probe(&mut self, cur: &mut InFlight, mode: Mode) -> Check {
        self.set_step(cur, Step::Replacing)?;
        let restore_first = if mode == Mode::RestorePrevious && cur.needs_restore_on_rollback {
            let Some(backup) = self.journal.state().active_backup.clone() else {
                return Ok(Err("no backup recorded to restore".to_string()));
            };
            Some(backup)
        } else {
            None
        };
        if let Err(check) = replace(&self.runtime, &cur.target, restore_first.as_deref()).await? {
            return Ok(Err(check));
        }
        self.set_step(cur, Step::Probing)?;
        probe(&self.runtime, &self.settings, &cur.target).await
    }

    fn activate_release(&mut self, mut cur: InFlight, mode: Mode) -> Res<HistoryEntry> {
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
        self.conclude(entry, true)
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
}
