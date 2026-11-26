use super::*;

impl Run {
    pub fn validate(&self) -> Result<()> {
        self.plan.validate()?;
        self.admitted.validate()?;
        self.last_clock.validate()?;
        ensure(
            self.admitted.machine_boot_id == self.last_clock.machine_boot_id
                && self.last_clock.boot_ms >= self.admitted.boot_ms
                && self.admitted.boot_ms.checked_add(self.plan.wall_time_ms)
                    == Some(self.original_deadline_boot_ms),
            "run clock/deadline",
        )?;
        let binding = self.plan.binding()?;
        self.validate_permit(&binding)?;
        self.validate_dispatch_permit()?;
        self.validate_host(&binding)?;
        self.validate_dispatch_state()?;
        self.validate_phase_evidence()?;
        self.validate_phase_clock()?;
        self.validate_stopping()?;
        self.validate_active()
    }
    fn validate_permit(&self, binding: &CentralBinding) -> Result<()> {
        if let Some(p) = &self.permit {
            p.validate()?;
            ensure(
                &p.binding == binding && self.permit_within_run(p),
                "run permit",
            )?;
        }
        Ok(())
    }
    fn validate_dispatch_permit(&self) -> Result<()> {
        if let Some(a) = &self.association {
            a.validate()?;
            ensure(
                a == &self.plan.dispatch(a.permit.clone())?
                    && a.permit.sequence == 1
                    && self.permit_within_run(&a.permit),
                "original dispatch association",
            )?;
            let p = self
                .permit
                .as_ref()
                .ok_or(Error::Invalid("dispatch without permit"))?;
            ensure(p.covers_original(&a.permit), "permit high watermark")?;
        }
        Ok(())
    }
    fn validate_host(&self, binding: &CentralBinding) -> Result<()> {
        if let Some(l) = &self.launch {
            l.validate()?;
            ensure(&l.binding == binding, "launch binding")?;
        }
        self.uncertain_report
            .iter()
            .chain(&self.guardian)
            .try_for_each(artifact)?;
        if let Some(h) = &self.host_observation {
            h.validate()?;
            ensure(self.host_matches(h), "host observation association")?;
        }
        if let Some(s) = &self.stop {
            s.validate()?;
            ensure(
                self.host_matches(&s.host) && self.guardian.as_ref() == Some(&s.host.guardian),
                "stop association",
            )?;
        }
        Ok(())
    }
    fn validate_dispatch_state(&self) -> Result<()> {
        match &self.dispatch {
            DispatchState::NotAttempted => {
                ensure(self.association.is_none(), "unattempted association")?
            }
            DispatchState::Resolved { evidence } => {
                evidence.validate()?;
                ensure(
                    self.association.as_ref() == Some(&evidence.association),
                    "handoff association",
                )?;
            }
            _ => ensure(self.association.is_some(), "dispatch intent absent")?,
        }
        Ok(())
    }
    fn validate_phase_evidence(&self) -> Result<()> {
        match self.phase {
            Phase::Prepared | Phase::CancelledBeforeStart => {
                ensure(self.has_no_start_evidence(), "prestart state")?
            }
            _ => ensure(
                self.permit.is_some() && self.launch.is_some() && self.association.is_some(),
                "started state",
            )?,
        }
        ensure(
            (self.phase == Phase::StoppedReported) == self.stop.is_some(),
            "stopped evidence",
        )?;
        Ok(())
    }
    fn has_no_start_evidence(&self) -> bool {
        matches!(
            (
                &self.permit,
                &self.launch,
                &self.guardian,
                &self.stop,
                &self.uncertain_report,
                &self.dispatch
            ),
            (None, None, None, None, None, DispatchState::NotAttempted)
        )
    }
    fn validate_phase_clock(&self) -> Result<()> {
        if self.phase == Phase::Prepared {
            ensure(
                self.cancellation.is_none()
                    && self.last_clock.boot_ms < self.original_deadline_boot_ms,
                "cancelled or expired prepared state",
            )?;
        }
        if self.phase == Phase::RunningReported {
            ensure(
                self.guardian.is_some() && self.host_observation.is_some(),
                "running reference",
            )?;
        }
        Ok(())
    }
    fn validate_stopping(&self) -> Result<()> {
        if matches!(
            self.phase,
            Phase::CancelledBeforeStart | Phase::StopRequested | Phase::StopUncertain
        ) {
            ensure(self.cancellation.is_some(), "stop cause")?;
        }
        if matches!(
            self.phase,
            Phase::StopRequested | Phase::StopUncertain | Phase::StoppedReported
        ) {
            ensure(
                !matches!(self.dispatch, DispatchState::IntentCommitted),
                "stopped input must be resolved or unknown",
            )?;
        }
        if self.phase == Phase::StopUncertain {
            ensure(self.uncertain_report.is_some(), "uncertain stop report")?;
        }
        Ok(())
    }
    fn validate_active(&self) -> Result<()> {
        if self.active() {
            ensure(
                self.cancellation.is_none()
                    && self.last_clock.boot_ms < self.original_deadline_boot_ms
                    && self.last_clock.boot_ms
                        < self.permit.as_ref().expect("started").expires_boot_ms,
                "active model expired/cancelled",
            )?;
        }
        Ok(())
    }
}

impl PermitData {
    fn covers_original(&self, original: &PermitData) -> bool {
        self.sequence >= original.sequence
            && self.issued_boot_ms >= original.issued_boot_ms
            && self.expires_boot_ms >= original.expires_boot_ms
            && (self.sequence != original.sequence || self == original)
    }
}
