use super::*;

pub(super) struct Installed {
    pub(super) turn: Config,
    pub(super) host: Host,
}

impl Service<'_> {
    pub(super) async fn reconcile(
        &self,
        ctl: &Control,
        cursor: &mut Poll,
        installed: Installed,
    ) -> Result<bool> {
        let Some(state) = installed.state(self, ctl).await? else {
            return Ok(false);
        };
        let (turn, host) = (&installed.turn, &installed.host);
        match state.run.phase() {
            Phase::StartCommitted | Phase::RunningReported => {
                cursor.failed_revision = None;
                host.enable_replies(ctl.actor, now()).await?;
                let p = state
                    .run
                    .permit()
                    .context("active supervisor permit missing")?;
                let boot = crate::clock()?.boot_ms;
                if boot >= p.issued_boot_ms + (p.expires_boot_ms - p.issued_boot_ms) / 2
                    && p.expires_boot_ms < state.run.original_deadline_boot_ms()
                {
                    host.renew(ctl.actor, p.sequence, turn.central.max_permit_ms)
                        .await?;
                }
                self.report("working", "Working on the conversation", Some(&state))
                    .await?;
                return Ok(false);
            }
            Phase::StopRequested => {
                self.report(
                    "stopping",
                    "Stopping supervisor; retaining work",
                    Some(&state),
                )
                .await?;
                return Ok(false);
            }
            Phase::StopUncertain => {
                self.report(
                    "uncertain",
                    "Stop is unconfirmed; inspect the retained supervisor",
                    Some(&state),
                )
                .await?;
                return Ok(false);
            }
            Phase::StoppedReported | Phase::CancelledBeforeStart => {
                clean_homes(self.config)?;
                archive_turn(self.config, turn)?;
                // A stopped generation with outstanding mail is checked by
                // the reservation owner below, including after a restart.
                cursor.failed_revision = None;
            }
            Phase::Prepared => {
                self.report(
                    "uncertain",
                    "Prepared supervisor needs reconciliation",
                    Some(&state),
                )
                .await?;
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl Installed {
    async fn state(&self, service: &Service<'_>, ctl: &Control) -> Result<Option<Snapshot>> {
        let Installed { turn, host } = self;
        let pool = service.pool;
        ensure!(
            turn.central.subject.actor_id() == ctl.actor,
            "supervisor actor changed"
        );
        let current = aq_supervisor_control::authority::sessions(pool)
            .inspect(ctl.actor)
            .await?;
        let successor = current.as_ref().is_some_and(|s| {
            s.run.plan().generation < turn.central.generation
                && matches!(
                    s.run.phase(),
                    Phase::StoppedReported | Phase::CancelledBeforeStart
                )
        });
        let state = if current.is_none() || successor {
            if !ctl.enabled {
                return Ok(None);
            }
            if let Some(reason) =
                crate::config::swarm_wait(pool, turn.swarm_profile.as_ref()).await?
            {
                service.report("waiting", swarm_stage(reason), None).await?;
                return Ok(None);
            }
            host.start(
                crate::Startup {
                    central: turn.central.clone(),
                    host: turn.host.clone(),
                    input: protected_read(&turn.startup_file, 1048576)?,
                },
                turn.central.max_permit_ms,
            )
            .await?
        } else {
            let existing = current.context("central state missing")?;
            ensure!(
                existing.run.plan().generation == turn.central.generation,
                "current turn does not match retained generation"
            );
            if !ctl.enabled && existing.run.phase() != Phase::StoppedReported {
                host.stop(ctl.actor).await?
            } else {
                host.reconcile(ctl.actor).await?
            }
        };
        Ok(Some(state))
    }
}

#[cfg(test)]
#[path = "reconcile/qualification.rs"]
mod qualification;
