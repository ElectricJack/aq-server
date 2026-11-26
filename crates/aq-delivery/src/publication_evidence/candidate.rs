use super::*;
use crate::{CandidateRequest, CandidateResult, CandidateState, CommitMetadata};
const COMMIT_NAME: &str = "AQ";
const COMMIT_EMAIL: &str = "aq@aq.invalid";

pub enum Candidate {
    Ready(ObjectId),
    Conflicted(Value),
}

impl Candidate {
    pub fn oid(&self) -> Option<&ObjectId> {
        match self {
            Self::Ready(oid) => Some(oid),
            Self::Conflicted(_) => None,
        }
    }
}

impl Candidate {
    pub fn from_receipt(receipt: CandidateResult) -> Option<Self> {
        if receipt.state == CandidateState::Ready {
            return receipt.candidate.map(Self::Ready);
        }
        Some(Self::Conflicted(
            json!({"reason":"conflicted","conflict_step":receipt.conflict_step,"conflict_path_count":receipt.conflict_path_count}),
        ))
    }
}

pub struct SavedCheckpoint<'a> {
    pub task: Uuid,
    pub source: Uuid,
    pub oid: &'a str,
}
impl SavedCheckpoint<'_> {
    pub fn request(&self, target: &PublicationTarget, base: &ObjectId) -> CandidateRequest {
        CandidateRequest {
            context: target.context(target.candidate_op),
            base: base.clone(),
            source_receipts: vec![self.source],
            commit: CommitMetadata {
                name: COMMIT_NAME.into(),
                email: COMMIT_EMAIL.into(),
                message: format!(
                    "AQ: task {} result {}\n\nRetained checkpoint {} published by {}.",
                    self.task, target.report_id, self.oid, OWNER
                ),
            },
        }
    }
}
