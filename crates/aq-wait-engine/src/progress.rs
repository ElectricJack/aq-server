use super::*;

impl MemberProgress {
    pub(super) fn commit(&mut self, proof: CommittedObservation, now: u64) {
        if self.original.is_none() {
            self.original = Some(proof.boundary.clone());
            self.original_observation = Some(proof.observation.clone());
            self.original_snapshot = Some(proof.owner_snapshot.clone());
            self.incarnation = Some(proof.incarnation.clone());
        }
        self.current = Some(proof.boundary);
        self.current_observation = Some(proof.observation.clone());
        self.current_snapshot = Some(proof.owner_snapshot);
        self.last = proof.observation;
        self.unavailable_reason = None;
        self.last_checked_at = Some(now);
        self.last_verified_at = Some(now);
    }
    pub(super) fn pending(member_id: Uuid) -> Self {
        Self {
            original: None,
            current: None,
            original_observation: None,
            current_observation: None,
            original_snapshot: None,
            current_snapshot: None,
            incarnation: None,
            last: wire::MemberOutcome {
                member_id,
                state: wire::MemberState::Pending,
                evidence: None,
            },
            unavailable_reason: None,
            last_checked_at: None,
            last_verified_at: None,
        }
    }

    pub(super) fn validate(
        &self,
        member: &wire::Member,
        created: u64,
        logical_now: u64,
    ) -> Result<()> {
        self.last.validate()?;
        require(member.member_id == self.last.member_id)?;
        self.validate_boundaries(member)?;
        if matches!(
            self.last.state,
            wire::MemberState::Satisfied | wire::MemberState::SourceFailed
        ) {
            require(self.current_observation.as_ref() == Some(&self.last))?;
        }
        if let (Some(original), Some(current)) = (&self.original, &self.current) {
            require(original.subscription_receipt == current.subscription_receipt)?;
        }
        if let Some(incarnation) = &self.incarnation {
            incarnation.validate()?;
        }
        if let Some(reason) = &self.unavailable_reason {
            reason.validate()?;
        }
        self.validate_freshness(created, logical_now)?;
        Ok(())
    }
    fn validate_boundaries(&self, member: &wire::Member) -> Result<()> {
        let installed = self.original.is_some();
        require(
            [
                self.current.is_some(),
                self.incarnation.is_some(),
                self.original_observation.is_some(),
                self.current_observation.is_some(),
                self.original_snapshot.is_some(),
                self.current_snapshot.is_some(),
            ]
            .into_iter()
            .all(|present| present == installed),
        )?;
        for (boundary, observation, snapshot) in [
            (
                &self.original,
                &self.original_observation,
                &self.original_snapshot,
            ),
            (
                &self.current,
                &self.current_observation,
                &self.current_snapshot,
            ),
        ] {
            if let (Some(boundary), Some(observation), Some(snapshot)) =
                (boundary, observation, snapshot)
            {
                validate_observation(member, boundary, observation, snapshot)?;
            }
        }
        Ok(())
    }
    fn validate_freshness(&self, created: u64, logical_now: u64) -> Result<()> {
        for at in [self.last_checked_at, self.last_verified_at]
            .into_iter()
            .flatten()
        {
            instant(at)?;
            require(at >= created && at <= logical_now)?;
        }
        if let Some(verified) = self.last_verified_at {
            require(
                self.last_checked_at
                    .is_some_and(|checked| checked >= verified),
            )?;
        }
        Ok(())
    }
}
fn validate_observation(
    member: &wire::Member,
    boundary: &wire::SourceBoundary,
    observation: &wire::MemberOutcome,
    snapshot: &wire::ArtifactRef,
) -> Result<()> {
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
                && evidence.condition_digest == canonical_digest(&member.condition)?,
        )?;
    }
    Ok(())
}
