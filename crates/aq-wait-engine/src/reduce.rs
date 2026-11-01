use super::*;
impl Snapshot {
    pub fn begin(
        request: wire::Request,
        fence: &AuthorityFence,
        policy: ResolvedPolicy,
        ids: StableIds,
        graph: &KnownGraph,
        now: u64,
    ) -> Result<TransitionPlan> {
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
        let owner = DependencyKey::Task {
            project: registration.binding.project_id,
            task: registration.binding.task_id,
        };
        let dependencies: Vec<_> = registration
            .predicate
            .members()
            .iter()
            .filter_map(|member| match member.condition {
                wire::Condition::Task { task_id, .. } => Some(DependencyKey::Task {
                    project: registration.binding.project_id,
                    task: task_id,
                }),
                wire::Condition::Dependency {
                    depends_on_task_id, ..
                } => Some(DependencyKey::Task {
                    project: registration.binding.project_id,
                    task: depends_on_task_id,
                }),
                wire::Condition::WaitOutcome { ref wait } => {
                    Some(DependencyKey::Wait { wait: wait.wait_id })
                }
                _ => None,
            })
            .collect();
        let cycle = graph.check(&owner, &dependencies)?;
        let members = registration
            .predicate
            .members()
            .iter()
            .map(|v| MemberProgress {
                original: None,
                current: None,
                original_observation: None,
                current_observation: None,
                original_snapshot: None,
                current_snapshot: None,
                incarnation: None,
                last: wire::MemberOutcome {
                    member_id: v.member_id,
                    state: wire::MemberState::Pending,
                    evidence: None,
                },
                unavailable_reason: None,
                last_checked_at: None,
                last_verified_at: None,
            })
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
        if current.principal != self.authority.principal
            || current.binding != self.authority.binding
            || current.scope != self.authority.scope
        {
            return Err(Error::Stale);
        }
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.logical_now = now.max(self.logical_now);
        if fence.task_cancelled {
            if next.outcome.is_none() {
                actions.push(next.finish(wire::WaitOutcomeKind::Cancelled, false)?);
            } else if next.resume_allowed {
                next.resume_allowed = false;
                actions.push(Action::RetireResume {
                    intent_id: next.ids.resume_intent_id,
                    wait: next.registration().wait_ref(),
                });
            }
        } else if next.outcome.is_some() {
            // Terminal source identity/outcome never changes on late hints/replies.
        } else if current.source_access != self.authority.source_access {
            next.hold("source access or project binding changed")?;
        } else {
            if let Input::SourceBatch { ref ticket, .. } | Input::Reclaim { ref ticket } = input
                && next.inflight.as_ref() != Some(ticket)
            {
                return Err(Error::Stale);
            }
            if next.logical_now >= next.deadline {
                actions.push(next.finish(wire::WaitOutcomeKind::DeadlineReached, true)?);
            } else {
                match input {
                    Input::Drive => actions.extend(next.schedule()?),
                    Input::Hint { member_id } => {
                        require(
                            next.registration()
                                .predicate
                                .members()
                                .iter()
                                .any(|v| v.member_id == member_id),
                        )?;
                        next.dirty = next.dirty.checked_add(1).ok_or(Error::Overflow)?;
                        if next.retries == 0 {
                            next.next_due = next.logical_now;
                        }
                        actions.extend(next.schedule()?);
                    }
                    Input::Reclaim { .. } => {
                        next.inflight = None;
                        next.retry()?;
                    }
                    Input::SourceBatch { ticket, replies } => {
                        actions.extend(next.receive(ticket, replies)?)
                    }
                }
            }
        }
        next.revision = self.revision.checked_add(1).ok_or(Error::Overflow)?;
        next.validate()?;
        Ok(TransitionPlan {
            expected_revision: Some(self.revision),
            next,
            actions,
            replay: false,
        })
    }
    pub(crate) fn registration(&self) -> &wire::WaitRegistration {
        match &self.request.body.command {
            wire::Command::RegisterWait(v) => v,
            _ => unreachable!("validated registration"),
        }
    }
    fn source_requests(&self, kind: &WorkKind) -> Vec<SourceRequest> {
        self.registration()
            .predicate
            .members()
            .iter()
            .zip(&self.members)
            .filter(|(_, p)| *kind != WorkKind::Install || p.original.is_none())
            .map(|(m, p)| SourceRequest {
                subscription: SubscriptionId {
                    wait: self.registration().wait_ref(),
                    member_id: m.member_id,
                },
                scope: self.authority.scope.clone(),
                binding: self.authority.binding.clone(),
                principal: self.authority.principal.clone(),
                source_access: self.authority.source_access.clone(),
                member: (*m).clone(),
                previous: p.current.clone(),
                incarnation: p.incarnation.clone(),
            })
            .collect()
    }
    fn schedule(&mut self) -> Result<Vec<Action>> {
        if self.inflight.is_some()
            || self.outcome.is_some()
            || self.phase == Phase::RecoveryHeld
            || self.logical_now < self.next_due
        {
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
    fn receive(&mut self, ticket: WorkTicket, replies: Vec<SourceReply>) -> Result<Vec<Action>> {
        let requests = self.source_requests(&ticket.kind);
        require(replies.len() == requests.len() && replies.len() <= wire::MAX_MEMBERS)?;
        let mut unavailable = false;
        for (request, reply) in requests.iter().zip(replies) {
            require(request.member.member_id == reply.member_id())?;
            let index = self
                .members
                .iter()
                .position(|m| m.last.member_id == request.member.member_id)
                .ok_or(Error::Invalid)?;
            match reply {
                SourceReply::Committed(proof) => {
                    self.proof(request, &proof)?;
                    let progress = &mut self.members[index];
                    if progress.original.is_none() {
                        progress.original = Some(proof.boundary.clone());
                        progress.original_observation = Some(proof.observation.clone());
                        progress.original_snapshot = Some(proof.owner_snapshot.clone());
                        progress.incarnation = Some(proof.incarnation.clone());
                    }
                    progress.current = Some(proof.boundary);
                    progress.current_observation = Some(proof.observation.clone());
                    progress.current_snapshot = Some(proof.owner_snapshot);
                    progress.last = proof.observation;
                    progress.unavailable_reason = None;
                    progress.last_checked_at = Some(self.logical_now);
                    progress.last_verified_at = Some(self.logical_now);
                }
                SourceReply::RetryableUnavailable { reason, .. }
                | SourceReply::ReplayGap { reason, .. } => {
                    reason.validate()?;
                    unavailable = true;
                    self.members[index].last = wire::MemberOutcome {
                        member_id: request.member.member_id,
                        state: wire::MemberState::SourceUnavailable,
                        evidence: None,
                    };
                    self.members[index].unavailable_reason = Some(reason);
                    self.members[index].last_checked_at = Some(self.logical_now);
                }
            }
        }
        self.inflight = None;
        if ticket.kind == WorkKind::Install {
            if self.members.iter().all(|m| m.original.is_some()) {
                // Even an already-satisfied install snapshot must be unconditionally
                // reobserved. No install response alone commits an outcome.
                self.phase = Phase::Pending;
                self.retries = 0;
                self.next_due = self.logical_now;
                return self.schedule();
            }
            self.retry()?;
            return Ok(Vec::new());
        }
        self.completed_dirty = ticket.dirty_through;
        if ticket.dirty_through != self.dirty {
            // Keep safe source progress, but never erase a hint that arrived
            // while this round was outside the owner's commit fence.
            if unavailable {
                self.retry()?;
            } else {
                self.retries = 0;
                self.next_due = self.logical_now;
            }
            return self.schedule();
        }
        let any = matches!(self.registration().predicate, wire::Predicate::Any { .. });
        let satisfied = self
            .members
            .iter()
            .filter(|m| m.last.state == wire::MemberState::Satisfied)
            .count();
        let failed = self
            .members
            .iter()
            .any(|m| m.last.state == wire::MemberState::SourceFailed);
        let pending = self.members.iter().any(|m| {
            matches!(
                m.last.state,
                wire::MemberState::Pending | wire::MemberState::SourceUnavailable
            )
        });
        if (any && satisfied > 0) || (!any && satisfied == self.members.len()) {
            return Ok(vec![self.finish(wire::WaitOutcomeKind::Satisfied, true)?]);
        }
        if failed && (!any || !pending) {
            return Ok(vec![
                self.finish(wire::WaitOutcomeKind::SourceFailed, true)?,
            ]);
        }
        if unavailable {
            self.retry()?;
        } else {
            self.retries = 0;
            self.next_due = add(self.logical_now, self.policy.body.check_interval_ms)?;
        }
        Ok(Vec::new())
    }
    fn proof(&self, request: &SourceRequest, proof: &CommittedObservation) -> Result<()> {
        proof.boundary.validate()?;
        proof.observation.validate()?;
        proof.incarnation.validate()?;
        wire::artifact(&proof.owner_snapshot)?;
        let boundary = &proof.boundary;
        let valid = proof.protocol == SourceProtocol::CommittedReplay
            && proof.subscription == request.subscription
            && proof.scope == request.scope
            && proof.binding == request.binding
            && proof.principal == request.principal
            && proof.source_access == request.source_access
            && boundary.member_id == request.member.member_id
            && boundary.owner == request.member.condition.owner()
            && boundary.condition_digest == canonical_digest(&request.member.condition)?
            && boundary.snapshot_digest == proof.owner_snapshot.digest
            && proof.projection_digest == canonical_digest(&proof.observation)?
            && proof.observation.member_id == request.member.member_id
            && proof.observation.state != wire::MemberState::SourceUnavailable;
        if !valid {
            return Err(Error::Proof);
        }
        if let wire::Condition::Timer { due_at_ms } = request.member.condition
            && proof.observation.state == wire::MemberState::Satisfied
            && self.logical_now < due_at_ms
        {
            return Err(Error::Proof);
        }
        if let Some(previous) = &request.previous {
            if request.incarnation.as_ref() != Some(&proof.incarnation)
                || proof.replay_from.as_ref() != Some(&previous.committed_replay_cursor)
                || previous.subscription_receipt != boundary.subscription_receipt
            {
                return Err(Error::Proof);
            }
        } else if proof.replay_from.is_some() {
            return Err(Error::Proof);
        }
        if let Some(evidence) = &proof.observation.evidence
            && (evidence.owner != boundary.owner
                || evidence.condition_digest != boundary.condition_digest)
        {
            return Err(Error::Proof);
        }
        Ok(())
    }
    fn retry(&mut self) -> Result<()> {
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
    fn finish(&mut self, kind: wire::WaitOutcomeKind, resume: bool) -> Result<Action> {
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
        require(
            self.authority.scope == registration.scope
                && self.authority.binding == registration.binding
                && self.authority.scope.project_id == self.authority.binding.project_id
                && registration.policy == self.policy.reference
                && self.logical_now >= self.created
                && self.deadline == add(self.created, self.policy.body.maximum_age_ms)?
                && self.revision > 0
                && self.completed_dirty <= self.dirty
                && self.work_generation <= u64::from(self.policy.body.max_work_attempts)
                && self.retries <= self.policy.body.max_retries,
        )?;
        for id in [
            self.ids.continuation_id,
            self.ids.outcome_id,
            self.ids.resume_intent_id,
        ] {
            wire::uuid(id)?;
        }
        require(self.members.len() == registration.predicate.members().len())?;
        for (member, progress) in registration.predicate.members().iter().zip(&self.members) {
            progress.last.validate()?;
            require(member.member_id == progress.last.member_id)?;
            require(
                progress.original.is_some() == progress.current.is_some()
                    && progress.original.is_some() == progress.incarnation.is_some()
                    && progress.original.is_some() == progress.original_observation.is_some()
                    && progress.original.is_some() == progress.current_observation.is_some()
                    && progress.original.is_some() == progress.original_snapshot.is_some()
                    && progress.original.is_some() == progress.current_snapshot.is_some(),
            )?;
            for (boundary, observation, snapshot) in [
                (
                    &progress.original,
                    &progress.original_observation,
                    &progress.original_snapshot,
                ),
                (
                    &progress.current,
                    &progress.current_observation,
                    &progress.current_snapshot,
                ),
            ] {
                if let (Some(boundary), Some(observation), Some(snapshot)) =
                    (boundary, observation, snapshot)
                {
                    boundary.validate()?;
                    observation.validate()?;
                    wire::artifact(snapshot)?;
                    require(
                        boundary.member_id == member.member_id
                            && boundary.owner == member.condition.owner()
                            && boundary.condition_digest == canonical_digest(&member.condition)?
                            && boundary.snapshot_digest == snapshot.digest
                            && observation.member_id == member.member_id,
                    )?;
                    if let Some(evidence) = &observation.evidence {
                        require(
                            evidence.owner == member.condition.owner()
                                && evidence.condition_digest
                                    == canonical_digest(&member.condition)?,
                        )?;
                    }
                }
            }
            if matches!(
                progress.last.state,
                wire::MemberState::Satisfied | wire::MemberState::SourceFailed
            ) {
                require(progress.current_observation.as_ref() == Some(&progress.last))?;
            }
            if let (Some(original), Some(current)) = (&progress.original, &progress.current) {
                require(original.subscription_receipt == current.subscription_receipt)?;
            }
            if let Some(incarnation) = &progress.incarnation {
                incarnation.validate()?;
            }
            if let Some(reason) = &progress.unavailable_reason {
                reason.validate()?;
            }
            for at in [progress.last_checked_at, progress.last_verified_at]
                .into_iter()
                .flatten()
            {
                instant(at)?;
                require(at >= self.created && at <= self.logical_now)?;
            }
            if let Some(verified) = progress.last_verified_at {
                require(
                    progress
                        .last_checked_at
                        .is_some_and(|checked| checked >= verified),
                )?;
            }
        }
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
        if let Some(ticket) = &self.inflight {
            require(
                ticket.wait == registration.wait_ref()
                    && ticket.generation == self.work_generation
                    && ticket.generation > 0
                    && ticket.dirty_through <= self.dirty
                    && self.outcome.is_none()
                    && self.phase != Phase::RecoveryHeld,
            )?;
            require(match ticket.kind {
                WorkKind::Install => {
                    self.phase == Phase::Installing
                        && self.members.iter().any(|m| m.original.is_none())
                }
                WorkKind::Reobserve => {
                    self.phase == Phase::Pending
                        && self.members.iter().all(|m| m.original.is_some())
                }
            })?;
        }
        require((self.phase == Phase::Terminal) == self.outcome.is_some())?;
        if let Some(outcome) = &self.outcome {
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
                wire::WaitOutcomeKind::SourceUnavailable
                | wire::WaitOutcomeKind::ManualOverride => return Err(Error::Invalid),
            }
            require(
                outcome.members
                    == self
                        .members
                        .iter()
                        .map(|m| m.last.clone())
                        .collect::<Vec<_>>(),
            )?;
        }
        if serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() > MAX_SNAPSHOT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}
fn outcome_digest(outcome: &wire::WaitOutcome) -> Result<wire::Digest> {
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
