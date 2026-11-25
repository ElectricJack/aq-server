use super::*;

impl Snapshot {
    pub fn ids(&self) -> &StableIds {
        &self.ids
    }
    pub fn policy(&self) -> &ResolvedPolicy {
        &self.policy
    }
    pub fn inflight(&self) -> Option<&WorkTicket> {
        self.inflight.as_ref()
    }
    pub fn hold_reason(&self) -> Option<&wire::Text> {
        self.hold_reason.as_ref()
    }
    pub fn cycle(&self) -> &wire::CycleAssessment {
        &self.cycle
    }
    /// Original committed install boundaries, present only once every member
    /// is installed; aligned with the predicate's members.
    pub fn boundaries(&self) -> Option<Vec<wire::SourceBoundary>> {
        self.members.iter().map(|m| m.original.clone()).collect()
    }
    /// Latest observation per member, with its last check and verification times.
    pub fn member_states(&self) -> Vec<MemberFreshness> {
        self.members
            .iter()
            .map(|m| MemberFreshness {
                state: m.last.clone(),
                unavailable_reason: m.unavailable_reason.clone(),
                last_checked_at_ms: m.last_checked_at,
                last_verified_at_ms: m.last_verified_at,
            })
            .collect()
    }
    pub fn phase(&self) -> &Phase {
        &self.phase
    }
    pub fn outcome(&self) -> Option<&wire::WaitOutcome> {
        self.outcome.as_ref()
    }
    pub fn original_created_at_ms(&self) -> u64 {
        self.created
    }
    pub fn deadline_ms(&self) -> u64 {
        self.deadline
    }
    pub fn next_due_ms(&self) -> u64 {
        self.next_due
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Capacity);
        }
        let value: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        value.validate()?;
        Ok(value)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.request.validate()?;
        self.policy.validate()?;
        instant(self.created)?;
        instant(self.logical_now)?;
        instant(self.next_due)?;
        let wire::Command::RegisterWait(registration) = &self.request.body.command else {
            return Err(Error::Invalid);
        };
        self.validate_owner(registration)?;
        for id in [
            self.ids.continuation_id,
            self.ids.outcome_id,
            self.ids.resume_intent_id,
        ] {
            wire::uuid(id)?;
        }
        require(self.members.len() == registration.predicate.members().len())?;
        for (member, progress) in registration.predicate.members().iter().zip(&self.members) {
            progress.validate(member, self.created, self.logical_now)?;
        }
        self.validate_phase()?;
        if let Some(ticket) = &self.inflight {
            self.validate_ticket(registration, ticket)?;
        }
        require((self.phase == Phase::Terminal) == self.outcome.is_some())?;
        if let Some(outcome) = &self.outcome {
            self.validate_outcome(registration, outcome)?;
        }
        if serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }
    fn validate_owner(&self, registration: &wire::WaitRegistration) -> Result<()> {
        require(
            self.authority.scope == registration.scope
                && self.authority.binding == registration.binding
                && self.authority.scope.project_id == self.authority.binding.project_id
                && registration.policy == self.policy.reference,
        )?;
        self.validate_clock_and_counters()
    }
    fn validate_clock_and_counters(&self) -> Result<()> {
        require(
            self.logical_now >= self.created
                && self.deadline == add(self.created, self.policy.body.maximum_age_ms)?
                && self.revision > 0
                && self.completed_dirty <= self.dirty
                && self.work_generation <= u64::from(self.policy.body.max_work_attempts)
                && self.retries <= self.policy.body.max_retries,
        )?;
        Ok(())
    }
    fn validate_phase(&self) -> Result<()> {
        if self.phase == Phase::Pending {
            require(self.members.iter().all(|m| m.original.is_some()))?;
        }
        if self.phase == Phase::Installing {
            require(self.members.iter().any(|m| m.original.is_none()))?;
        }
        if self.phase == Phase::RecoveryHeld {
            require(self.hold_reason.is_some() && self.inflight.is_none())?;
        }
        if let Some(reason) = &self.hold_reason {
            reason.validate()?;
        }
        match &self.cycle {
            wire::CycleAssessment::NoKnownCycle { revision } => wire::artifact(revision)?,
            wire::CycleAssessment::ExternalUnknown { reason } => reason.validate()?,
        }
        Ok(())
    }
    fn validate_ticket(
        &self,
        registration: &wire::WaitRegistration,
        ticket: &WorkTicket,
    ) -> Result<()> {
        require(
            ticket.wait == registration.wait_ref()
                && ticket.generation == self.work_generation
                && ticket.generation > 0
                && ticket.dirty_through <= self.dirty
                && self.outcome.is_none()
                && self.phase != Phase::RecoveryHeld,
        )?;
        self.validate_work_phase(&ticket.kind)
    }
    fn validate_work_phase(&self, kind: &WorkKind) -> Result<()> {
        require(match kind {
            WorkKind::Install => {
                self.phase == Phase::Installing && self.members.iter().any(|m| m.original.is_none())
            }
            WorkKind::Reobserve => {
                self.phase == Phase::Pending && self.members.iter().all(|m| m.original.is_some())
            }
        })?;
        Ok(())
    }
    fn validate_outcome(
        &self,
        registration: &wire::WaitRegistration,
        outcome: &wire::WaitOutcome,
    ) -> Result<()> {
        outcome.validate_for(&registration.predicate)?;
        require(
            outcome.outcome.outcome_id == self.ids.outcome_id
                && outcome.outcome.wait == registration.wait_ref()
                && outcome.outcome.digest == outcome_digest(outcome)?
                && outcome.recorded_at_ms >= self.created
                && outcome.recorded_at_ms <= self.logical_now,
        )?;
        match outcome.kind {
            wire::WaitOutcomeKind::Satisfied | wire::WaitOutcomeKind::SourceFailed => {
                require(
                    self.members.iter().all(|m| m.original.is_some())
                        && outcome.recorded_at_ms < self.deadline,
                )?;
            }
            wire::WaitOutcomeKind::DeadlineReached => {
                require(outcome.recorded_at_ms >= self.deadline)?
            }
            wire::WaitOutcomeKind::Cancelled => require(!self.resume_allowed)?,
            wire::WaitOutcomeKind::SourceUnavailable | wire::WaitOutcomeKind::ManualOverride => {
                return Err(Error::Invalid);
            }
        }
        require(
            outcome.members
                == self
                    .members
                    .iter()
                    .map(|m| m.last.clone())
                    .collect::<Vec<_>>(),
        )?;
        Ok(())
    }
}
pub(super) fn outcome_digest(outcome: &wire::WaitOutcome) -> Result<wire::Digest> {
    let mut value = serde_json::to_value(outcome).map_err(|_| Error::Invalid)?;
    value["outcome"]
        .as_object_mut()
        .ok_or(Error::Invalid)?
        .remove("digest");
    value.sort_all_objects();
    Ok(wire::Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Error::Invalid)?,
    ))
}
