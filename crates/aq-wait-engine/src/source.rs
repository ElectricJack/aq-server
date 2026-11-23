use super::*;

impl Snapshot {
    pub(super) fn source_requests(&self, kind: &WorkKind) -> Vec<SourceRequest> {
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
    pub(super) fn receive(
        &mut self,
        ticket: WorkTicket,
        replies: Vec<SourceReply>,
    ) -> Result<Vec<Action>> {
        let requests = self.source_requests(&ticket.kind);
        require(replies.len() == requests.len() && replies.len() <= wire::MAX_MEMBERS)?;
        let mut unavailable = false;
        for (request, reply) in requests.iter().zip(replies) {
            unavailable |= self.record_reply(request, reply)?;
        }
        self.inflight = None;
        if ticket.kind == WorkKind::Install {
            return self.finish_install();
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
        if let Some(kind) = self.source_outcome() {
            return Ok(vec![self.finish(kind, true)?]);
        }
        if unavailable {
            self.retry()?;
        } else {
            self.retries = 0;
            self.next_due = add(self.logical_now, self.policy.body.check_interval_ms)?;
        }
        Ok(Vec::new())
    }
    fn source_outcome(&self) -> Option<wire::WaitOutcomeKind> {
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
        let complete = if any {
            satisfied > 0
        } else {
            satisfied == self.members.len()
        };
        if complete {
            return Some(wire::WaitOutcomeKind::Satisfied);
        }
        let can_fail = !any || !pending;
        if failed && can_fail {
            return Some(wire::WaitOutcomeKind::SourceFailed);
        }
        None
    }
    fn record_reply(&mut self, request: &SourceRequest, reply: SourceReply) -> Result<bool> {
        require(request.member.member_id == reply.member_id())?;
        let index = self
            .members
            .iter()
            .position(|m| m.last.member_id == request.member.member_id)
            .ok_or(Error::Invalid)?;
        match reply {
            SourceReply::Committed(proof) => {
                proof.verify(request, self.logical_now)?;
                self.members[index].commit(*proof, self.logical_now);
                Ok(false)
            }
            SourceReply::RetryableUnavailable { reason, .. }
            | SourceReply::ReplayGap { reason, .. } => {
                reason.validate()?;
                self.members[index].last = wire::MemberOutcome {
                    member_id: request.member.member_id,
                    state: wire::MemberState::SourceUnavailable,
                    evidence: None,
                };
                self.members[index].unavailable_reason = Some(reason);
                self.members[index].last_checked_at = Some(self.logical_now);
                Ok(true)
            }
        }
    }
    fn finish_install(&mut self) -> Result<Vec<Action>> {
        if self.members.iter().all(|m| m.original.is_some()) {
            // Even an already-satisfied install snapshot must be unconditionally
            // reobserved. No install response alone commits an outcome.
            self.phase = Phase::Pending;
            self.retries = 0;
            self.next_due = self.logical_now;
            return self.schedule();
        }
        self.retry()?;
        Ok(Vec::new())
    }
}
