//! Stored pre-offer source-export requests and their immutable source identity.
use aq_admission::{ArtifactRef, Digest, SourceAdmission};
use aq_session_protocol::Validate;
use aq_worker_protocol::{ObjectAlgorithm, ObjectId, SourceBinding};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub project_revision: u64,
    pub repository_id: Uuid,
    pub logical_identity: String,
    pub binding_generation: u64,
    pub base: ObjectId,
    pub source_retention_handle: Uuid,
    pub source_observation: ArtifactRef,
}
impl Request {
    pub fn validate(&self) -> Result<(), &'static str> {
        let revisions = [self.project_revision, self.binding_generation];
        let valid_revisions = revisions
            .into_iter()
            .all(|n| (1..=i64::MAX as u64).contains(&n));
        let identity = self.schema_version == 1 && self.ids_present() && valid_revisions;
        let scope = self.valid_repository() && self.valid_observation();
        if identity && scope {
            Ok(())
        } else {
            Err("invalid pre-offer source export request")
        }
    }
    fn ids_present(&self) -> bool {
        [
            self.operation_id,
            self.project_id,
            self.repository_id,
            self.source_retention_handle,
        ]
        .into_iter()
        .all(|id| !id.is_nil())
    }
    fn valid_repository(&self) -> bool {
        let name = &self.logical_identity;
        let valid_name =
            !name.trim().is_empty() && name.len() <= 512 && !name.chars().any(char::is_control);
        let valid_base = self.base.algorithm == ObjectAlgorithm::Sha1
            && self.base.hex.bytes().any(|b| b != b'0')
            && aq_worker_protocol::Validate::validate(&self.base).is_ok();
        valid_name && valid_base
    }
    fn valid_observation(&self) -> bool {
        [
            &self.source_observation.id,
            &self.source_observation.version,
        ]
        .into_iter()
        .all(|text| text.as_str().len() <= 512 && text.validate().is_ok())
    }
    pub fn matches_admission_scope(&self, observation: &SourceAdmission) -> bool {
        let project = (self.project_id, self.project_revision);
        let repository = (self.repository_id, self.binding_generation);
        project == (observation.project_id, observation.project_revision)
            && repository
                == (
                    observation.repository_id,
                    observation.repository_binding_generation,
                )
    }
    pub fn matches_resolved_object(&self, observation: &SourceAdmission) -> bool {
        observation.resolved_object.format == aq_admission::ObjectFormat::Sha1
            && observation.resolved_object.hex == self.base.hex
    }
    pub fn matches_source(&self, project: Uuid, source: &SourceBinding) -> bool {
        let scope = (project, source.repository_id, source.binding_generation);
        let expected = (self.project_id, self.repository_id, self.binding_generation);
        let object = source.logical_identity == self.logical_identity && source.base == self.base;
        scope == expected && object
    }
    fn canonical_bytes(&self) -> Result<Vec<u8>, &'static str> {
        self.validate()?;
        let mut value = serde_json::to_value(self).map_err(|_| "source export request encoding")?;
        value.sort_all_objects();
        serde_json::to_vec(&value).map_err(|_| "source export request encoding")
    }
    pub fn digest(&self) -> Result<Digest, &'static str> {
        Ok(Digest::of(&self.canonical_bytes()?))
    }
    pub fn encode(self) -> Result<EncodedRequest, &'static str> {
        let bytes = self.canonical_bytes()?;
        let digest = Digest::of(&bytes);
        Ok(EncodedRequest {
            request: self,
            bytes,
            digest,
        })
    }
}
pub struct EncodedRequest {
    pub request: Request,
    pub bytes: Vec<u8>,
    pub digest: Digest,
}
pub mod stored;
