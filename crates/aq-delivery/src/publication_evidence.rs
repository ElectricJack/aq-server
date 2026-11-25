//! Durable evidence for the automatic publication of a retained result.
use crate::{
    OperationContext, PublicationPolicy,
    source_checks::{Repository, SourceHead},
};
use aq_worker_protocol::ObjectId;
use serde_json::{Value, json};
use uuid::Uuid;

pub const OWNER: &str = "aq.d1.default-policy-publication@1";

mod candidate;
mod outcome;
pub use candidate::{Candidate, SavedCheckpoint};
pub use outcome::{PublicationStatus, Recorded};

pub struct PublicationTarget {
    pub report_id: Uuid,
    project: Uuid,
    repository: Repository,
    pub policy: PublicationPolicy,
    pub candidate_op: Uuid,
    pub publish_op: Uuid,
}

impl PublicationTarget {
    pub fn new(
        report_id: Uuid,
        project: Uuid,
        repository: Repository,
        policy: PublicationPolicy,
    ) -> Self {
        Self {
            report_id,
            project,
            repository,
            policy,
            candidate_op: Uuid::new_v5(&report_id, b"aq.d1.default-policy.candidate"),
            publish_op: Uuid::new_v5(&report_id, b"aq.d1.default-policy.publish"),
        }
    }
    pub fn context(&self, operation_id: Uuid) -> OperationContext {
        OperationContext {
            operation_id,
            repository_id: self.repository.id,
            binding_generation: self.repository.generation,
        }
    }
    pub fn evidence<'a>(
        &'a self,
        base: &'a ObjectId,
        candidate: Option<&'a ObjectId>,
    ) -> PublicationEvidence<'a> {
        PublicationEvidence {
            target: self,
            base,
            candidate,
        }
    }
    /// Compare and advance the recorded head while the owner holds the target lock.
    pub fn advance(
        &self,
        current: Option<&SourceHead>,
        base: &ObjectId,
        oid: String,
    ) -> Result<Option<SourceHead>, crate::DeliveryError> {
        if current.is_some_and(|head| head.oid != base.hex) {
            return Ok(None);
        }
        let head = SourceHead {
            schema_version: 1,
            project_id: self.project,
            repository_id: self.repository.id,
            binding_generation: self.repository.generation,
            target: self.policy.target.clone(),
            sequence: current.map_or(1, |head| head.sequence + 1),
            oid,
            retention_handle: self.candidate_op,
            candidate_operation_id: self.candidate_op,
            publication_operation_id: self.publish_op,
            previous: base.hex.clone(),
        };
        if !head.validate() {
            return Err(crate::DeliveryError::Protocol);
        }
        Ok(Some(head))
    }
}
pub struct PublicationEvidence<'a> {
    target: &'a PublicationTarget,
    base: &'a ObjectId,
    candidate: Option<&'a ObjectId>,
}
impl PublicationEvidence<'_> {
    pub fn render(
        &self,
        phase: Option<&str>,
        result: Option<&Value>,
        head: Option<&SourceHead>,
    ) -> Value {
        let publication = self.target;
        json!({
            "owner": OWNER,
            "phase": phase,
            "report_id": publication.report_id,
            "policy": publication.policy,
            "candidate_operation_id": publication.candidate_op,
            "candidate": self.candidate,
            "publication_operation_id": publication.publish_op,
            "expected_old": self.base,
            "result": result,
            "source_head": head,
        })
    }
}
