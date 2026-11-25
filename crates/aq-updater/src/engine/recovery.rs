//! Resolve an interruption from the durable update journal.

use super::readiness::{ensure_running, probe};
use super::{Engine, Recorded, Res, rt, step_name};
use crate::journal::InFlight;
use crate::runtime::Runtime;
use crate::verify::ReleaseVerifier;
use aq_release::{HistoryEntry, Outcome, Step};
use chrono::Utc;

impl<R: Runtime, V: ReleaseVerifier> Engine<R, V> {
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
        let recorded = self.recorded_outcome(cur.request_id);
        if matches!(recorded, Recorded::Nothing) {
            return self.recover_interruption(cur).await;
        }
        self.recover_recorded(cur, recorded)
    }

    fn recover_recorded(&mut self, cur: InFlight, recorded: Recorded) -> Res<Option<HistoryEntry>> {
        match recorded {
            Recorded::Nothing => Ok(None),
            Recorded::Entry(done) if done.outcome == Outcome::FailedNeedsOperator => {
                // Already waiting for the operator; make sure maintenance is on.
                if let Err(e) = rt(self.maintenance.enter())? {
                    tracing::warn!(error = %e, "cannot enter maintenance");
                }
                Ok(None)
            }
            Recorded::Entry(done) if done.outcome == Outcome::Unknown => {
                let detail = format!(
                    "history holds an outcome this updater does not understand for the update to {}; it cannot tell what happened. {}",
                    cur.target.version,
                    Self::operator_steps(&cur)
                );
                self.hold_for_operator(cur, detail).map(Some)
            }
            Recorded::Entry(_) => {
                // Outcome recorded, journal not yet cleared.
                if let Err(e) = rt(self.maintenance.leave())? {
                    tracing::warn!(error = %e, "cannot clear the maintenance marker");
                }
                self.clear_current()?;
                Ok(None)
            }
            Recorded::Unreadable(e) => {
                let detail = format!(
                    "history is unreadable ({e}), so whether the update to {} already finished cannot be told. {}",
                    cur.target.version,
                    Self::operator_steps(&cur)
                );
                self.hold_for_operator(cur, detail).map(Some)
            }
        }
    }

    async fn recover_interruption(&mut self, cur: InFlight) -> Res<Option<HistoryEntry>> {
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
}
