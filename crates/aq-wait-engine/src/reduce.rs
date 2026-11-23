use super::snapshot::outcome_digest;
use super::*;
/// Trusted authority, dependency graph and clock observed for this registration.
pub struct RegistrationContext<'a> {
    pub fence: &'a AuthorityFence,
    pub graph: &'a KnownGraph,
    pub now: u64,
}

impl Snapshot {
    pub fn begin(
        request: wire::Request,
        policy: ResolvedPolicy,
        ids: StableIds,
        context: RegistrationContext<'_>,
    ) -> Result<TransitionPlan> {
        let RegistrationContext { fence, graph, now } = context;
        request.validate()?;
        policy.validate()?;
        instant(now)?;
        let authority = fence.key()?;
        require(graph.scope == authority.scope)?;
        let wire::Command::RegisterWait(registration) = &request.body.command else {
            return Err(Error::Invalid);
        };
        require(
            !fence.task_cancelled
                && registration.scope == authority.scope
                && registration.binding == authority.binding
                && registration.policy == policy.reference,
        )?;
        registration.reject_self(Some(request.body.operation_id), None)?;
        for id in [ids.continuation_id, ids.outcome_id, ids.resume_intent_id] {
            wire::uuid(id)?;
        }
        let cycle = graph.assess(registration)?;
        let members = registration
            .predicate
            .members()
            .iter()
            .map(|v| MemberProgress::pending(v.member_id))
            .collect();
        let deadline = add(now, policy.body.maximum_age_ms)?;
        let mut next = Self {
            request,
            authority,
            ids,
            policy,
            created: now,
            deadline,
            logical_now: now,
            revision: 1,
            dirty: 0,
            completed_dirty: 0,
            work_generation: 0,
            inflight: None,
            next_due: now,
            retries: 0,
            phase: Phase::Installing,
            members,
            outcome: None,
            hold_reason: None,
            resume_allowed: true,
            cycle,
        };
        let actions = next.schedule()?;
        next.validate()?;
        Ok(TransitionPlan {
            expected_revision: None,
            next,
            actions,
            replay: false,
        })
    }
    pub fn replay_registration(
        &self,
        request: &wire::Request,
        fence: &AuthorityFence,
    ) -> Result<TransitionPlan> {
        self.validate()?;
        request.validate()?;
        if request != &self.request {
            return Err(Error::Conflict);
        }
        if fence.key()? != self.authority || fence.task_cancelled {
            return Err(Error::Stale);
        }
        Ok(TransitionPlan {
            expected_revision: Some(self.revision),
            next: self.clone(),
            actions: Vec::new(),
            replay: true,
        })
    }
    pub fn step(&self, input: Input, fence: &AuthorityFence, now: u64) -> Result<TransitionPlan> {
        self.validate()?;
        instant(now)?;
        let current = fence.key()?;
        if !self.authority.same_caller(&current) {
            return Err(Error::Stale);
        }
        let mut next = self.clone();
        next.logical_now = now.max(self.logical_now);
        let actions = next.apply_input(input, fence, &current)?;
        next.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        next.validate()?;
        Ok(TransitionPlan {
            expected_revision: Some(self.revision),
            next,
            actions,
            replay: false,
        })
    }
    fn apply_input(
        &mut self,
        input: Input,
        fence: &AuthorityFence,
        current: &AuthorityKey,
    ) -> Result<Vec<Action>> {
        if fence.task_cancelled {
            return self.cancel();
        }
        // Late source hints and replies cannot change the terminal outcome.
        if self.outcome.is_some() {
            return Ok(Vec::new());
        }
        if current.source_access != self.authority.source_access {
            self.hold("source access or project binding changed")?;
            return Ok(Vec::new());
        }
        if let Input::SourceBatch { ref ticket, .. } | Input::Reclaim { ref ticket } = input
            && self.inflight.as_ref() != Some(ticket)
        {
            return Err(Error::Stale);
        }

        if self.logical_now >= self.deadline {
            return Ok(vec![
                self.finish(wire::WaitOutcomeKind::DeadlineReached, true)?,
            ]);
        }
        match input {
            Input::Drive => self.schedule(),
            Input::Hint { member_id } => self.hint(member_id),
            Input::Reclaim { .. } => {
                self.inflight = None;
                self.retry()?;
                Ok(Vec::new())
            }
            Input::SourceBatch { ticket, replies } => self.receive(ticket, replies),
        }
    }
    fn cancel(&mut self) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        if self.outcome.is_none() {
            actions.push(self.finish(wire::WaitOutcomeKind::Cancelled, false)?);
        } else if self.resume_allowed {
            self.resume_allowed = false;
            actions.push(Action::RetireResume {
                intent_id: self.ids.resume_intent_id,
                wait: self.registration().wait_ref(),
            });
        }

        Ok(actions)
    }
    fn hint(&mut self, member_id: Uuid) -> Result<Vec<Action>> {
        require(
            self.registration()
                .predicate
                .members()
                .iter()
                .any(|v| v.member_id == member_id),
        )?;
        self.dirty = self.dirty.checked_add(1).ok_or(Error::Overflow)?;
        if self.retries == 0 {
            self.next_due = self.logical_now;
        }
        self.schedule()
    }
    pub fn registration(&self) -> &wire::WaitRegistration {
        match &self.request.body.command {
            wire::Command::RegisterWait(v) => v,
            _ => unreachable!("validated registration"),
        }
    }
    pub(super) fn schedule(&mut self) -> Result<Vec<Action>> {
        let ready = self.phase != Phase::RecoveryHeld && self.logical_now >= self.next_due;
        let idle = self.inflight.is_none() && self.outcome.is_none();
        if !ready || !idle {
            return Ok(Vec::new());
        }
        if self.work_generation >= u64::from(self.policy.body.max_work_attempts) {
            self.hold("source work budget exhausted")?;
            return Ok(Vec::new());
        }
        self.work_generation = self.work_generation.checked_add(1).ok_or(Error::Overflow)?;
        let kind = if self.members.iter().all(|m| m.original.is_some()) {
            WorkKind::Reobserve
        } else {
            WorkKind::Install
        };
        let ticket = WorkTicket {
            wait: self.registration().wait_ref(),
            generation: self.work_generation,
            dirty_through: self.dirty,
            kind: kind.clone(),
        };
        let sources = self.source_requests(&kind);
        self.inflight = Some(ticket.clone());
        Ok(vec![match kind {
            WorkKind::Install => Action::Install { ticket, sources },
            WorkKind::Reobserve => Action::Reobserve { ticket, sources },
        }])
    }
    pub(super) fn retry(&mut self) -> Result<()> {
        self.retries = self.retries.checked_add(1).ok_or(Error::Overflow)?;
        if self.retries >= self.policy.body.max_retries {
            self.hold("source retry budget exhausted")?;
            return Ok(());
        }
        let base = self.policy.body.retry_base_ms;
        let delay = base
            .saturating_mul(1u64 << self.retries.saturating_sub(1).min(16))
            .min(self.policy.body.retry_cap_ms);
        let seed = u64::from_str_radix(&self.request.body_digest.as_str()[..4], 16)
            .map_err(|_| Error::Invalid)?;
        let jitter = seed % (base / 4 + 1);
        self.next_due = add(
            self.logical_now,
            delay
                .saturating_add(jitter)
                .min(self.policy.body.retry_cap_ms),
        )?;
        Ok(())
    }
    fn hold(&mut self, reason: &str) -> Result<()> {
        self.phase = Phase::RecoveryHeld;
        self.inflight = None;
        self.hold_reason = Some(wire::Text::new(reason)?);
        Ok(())
    }
    pub(super) fn finish(&mut self, kind: wire::WaitOutcomeKind, resume: bool) -> Result<Action> {
        let members: Vec<_> = self.members.iter().map(|m| m.last.clone()).collect();
        let selected = if kind == wire::WaitOutcomeKind::Satisfied
            && matches!(self.registration().predicate, wire::Predicate::Any { .. })
        {
            members
                .iter()
                .find(|m| m.state == wire::MemberState::Satisfied)
                .map(|m| m.member_id)
        } else {
            None
        };
        let mut outcome = wire::WaitOutcome {
            outcome: wire::OutcomeRef {
                outcome_id: self.ids.outcome_id,
                wait: self.registration().wait_ref(),
                revision: 1,
                digest: wire::Digest::of(b"placeholder never returned"),
            },
            kind,
            members,
            selected_member_id: selected,
            recorded_at_ms: self.logical_now,
            unresolved_predicate: None,
        };
        outcome.outcome.digest = outcome_digest(&outcome)?;
        outcome.validate_for(&self.registration().predicate)?;
        self.resume_allowed = resume;
        self.phase = Phase::Terminal;
        self.inflight = None;
        self.outcome = Some(outcome.clone());
        let resume = resume.then(|| ResumeIntent {
            intent_id: self.ids.resume_intent_id,
            continuation: wire::ContinuationRef {
                continuation_id: self.ids.continuation_id,
                generation: self.registration().continuation_generation,
                wait: self.registration().wait_ref(),
            },
            outcome: outcome.outcome.clone(),
        });
        Ok(Action::PersistOutcome {
            outcome: Box::new(outcome),
            resume,
        })
    }
}

impl AuthorityKey {
    fn same_caller(&self, other: &Self) -> bool {
        self.principal == other.principal
            && self.binding == other.binding
            && self.scope == other.scope
    }
}
