use super::*;
pub struct Observed {
    pub state: wire::MemberState,
    pub result: Option<wire::ArtifactRef>,
    pub revision: String,
    pub snapshot: wire::ArtifactRef,
}
impl Observed {
    pub fn committed(self, request: &SourceRequest) -> Result<SourceReply> {
        let owner = request.member.condition.owner();
        let condition_digest = wire::canonical_digest(&request.member.condition)?;
        let observation = wire::MemberOutcome {
            member_id: request.member.member_id,
            state: self.state,
            evidence: self.result.map(|result| wire::SourceEvidence {
                owner: owner.clone(),
                condition_digest: condition_digest.clone(),
                revision: text(self.revision.clone()),
                result,
            }),
        };
        let subscription = request.subscription.digest()?;
        Ok(SourceReply::Committed(Box::new(CommittedObservation {
            protocol: SourceProtocol::CommittedReplay,
            subscription: request.subscription.clone(),
            scope: request.scope.clone(),
            binding: request.binding.clone(),
            principal: request.principal.clone(),
            source_access: request.source_access.clone(),
            incarnation: text("1"),
            boundary: wire::SourceBoundary {
                member_id: request.member.member_id,
                owner,
                condition_digest,
                committed_revision: text(self.revision.clone()),
                committed_replay_cursor: text(self.revision),
                subscription_receipt: artifact("wait-subscription", subscription),
                snapshot_digest: self.snapshot.digest.clone(),
            },
            owner_snapshot: self.snapshot,
            projection_digest: wire::canonical_digest(&observation)?,
            replay_from: request
                .previous
                .as_ref()
                .map(|b| b.committed_replay_cursor.clone()),
            observation,
        })))
    }
}
