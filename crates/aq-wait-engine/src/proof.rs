use super::*;

impl CommittedObservation {
    pub(super) fn verify(&self, request: &SourceRequest, now: u64) -> Result<()> {
        self.boundary.validate()?;
        self.observation.validate()?;
        self.incarnation.validate()?;
        wire::artifact(&self.owner_snapshot)?;
        let verified = self.matches_subscription(request)
            && self.matches_boundary(request)?
            && self.matches_observation(request)?
            && self.matches_time(&request.member.condition, now)
            && self.matches_replay(request)
            && self.matches_evidence();
        verified.then_some(()).ok_or(Error::Proof)
    }
    fn matches_time(&self, condition: &wire::Condition, now: u64) -> bool {
        match condition {
            wire::Condition::Timer { due_at_ms }
                if self.observation.state == wire::MemberState::Satisfied =>
            {
                now >= *due_at_ms
            }
            _ => true,
        }
    }
    fn matches_evidence(&self) -> bool {
        self.observation.evidence.as_ref().is_none_or(|evidence| {
            evidence.owner == self.boundary.owner
                && evidence.condition_digest == self.boundary.condition_digest
        })
    }
    fn matches_subscription(&self, request: &SourceRequest) -> bool {
        self.protocol == SourceProtocol::CommittedReplay
            && self.subscription == request.subscription
            && self.scope == request.scope
            && self.binding == request.binding
            && self.principal == request.principal
            && self.source_access == request.source_access
    }
    fn matches_boundary(&self, request: &SourceRequest) -> Result<bool> {
        let boundary = &self.boundary;
        Ok(boundary.member_id == request.member.member_id
            && boundary.owner == request.member.condition.owner()
            && boundary.condition_digest == canonical_digest(&request.member.condition)?
            && boundary.snapshot_digest == self.owner_snapshot.digest)
    }
    fn matches_observation(&self, request: &SourceRequest) -> Result<bool> {
        Ok(
            self.projection_digest == canonical_digest(&self.observation)?
                && self.observation.member_id == request.member.member_id
                && self.observation.state != wire::MemberState::SourceUnavailable,
        )
    }
    fn matches_replay(&self, request: &SourceRequest) -> bool {
        match &request.previous {
            Some(previous) => {
                request.incarnation.as_ref() == Some(&self.incarnation)
                    && self.replay_from.as_ref() == Some(&previous.committed_replay_cursor)
                    && previous.subscription_receipt == self.boundary.subscription_receipt
            }
            None => self.replay_from.is_none(),
        }
    }
}
