//! Pre-offer source export ownership. A token is minted only after durable quota
//! reservation; the I/O owner cannot turn an arbitrary request into a token.
use aq_admission::{ArtifactRef, Digest};
use aq_session_protocol::Validate;
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
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
        if self.schema_version != 1
            || [
                self.operation_id,
                self.project_id,
                self.repository_id,
                self.source_retention_handle,
            ]
            .iter()
            .any(Uuid::is_nil)
            || self.project_revision == 0
            || self.project_revision > i64::MAX as u64
            || self.binding_generation == 0
            || self.binding_generation > i64::MAX as u64
            || self.logical_identity.trim().is_empty()
            || self.logical_identity.len() > 512
            || self.logical_identity.chars().any(char::is_control)
            || self.base.algorithm != ObjectAlgorithm::Sha1
            || self.base.hex.len() != 40
            || !self.base.hex.bytes().any(|b| b != b'0')
            || !self
                .base
                .hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.source_observation.id.as_str().len() > 512
            || self.source_observation.version.as_str().len() > 512
            || self.source_observation.id.validate().is_err()
            || self.source_observation.version.validate().is_err()
        {
            return Err("invalid pre-offer source export request");
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest, &'static str> {
        self.validate()?;
        aq_selection::digest(self).map_err(|_| "source export request encoding")
    }
}
/// No constructor or deserializer is exposed to production callers. The future
/// reserve service owns construction after its quota transaction commits.
pub struct ReservedExport {
    request: Request,
    request_digest: Digest,
    reserved_bytes: u64,
}
impl ReservedExport {
    pub fn request(&self) -> &Request {
        &self.request
    }
    pub fn request_digest(&self) -> &Digest {
        &self.request_digest
    }
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_bytes
    }
}
/// Explicit component-test capability; not a durable reservation or a route.
#[cfg(any(test, feature = "test-support"))]
pub fn synthetic_reserved_export(
    request: Request,
    reserved_bytes: u64,
) -> Result<ReservedExport, &'static str> {
    let request_digest = request.digest()?;
    if reserved_bytes <= 1_048_576 || reserved_bytes > i64::MAX as u64 {
        return Err("invalid synthetic source reservation bound");
    }
    Ok(ReservedExport {
        request,
        request_digest,
        reserved_bytes,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> Request {
        Request {
            schema_version: 1,
            operation_id: Uuid::from_u128(1),
            project_id: Uuid::from_u128(2),
            project_revision: 1,
            repository_id: Uuid::from_u128(3),
            logical_identity: "fixture-repository".into(),
            binding_generation: 1,
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
            source_retention_handle: Uuid::from_u128(4),
            source_observation: ArtifactRef {
                id: aq_admission::Text::new("synthetic-observation").unwrap(),
                version: aq_admission::Text::new("1").unwrap(),
                digest: Digest::of(b"observation"),
            },
        }
    }
    #[test]
    fn export_identity_is_exact_and_no_execution_binding_is_fabricated() {
        let r = request();
        let token = synthetic_reserved_export(r.clone(), 2_097_152).unwrap();
        assert_eq!(token.request_digest(), &r.digest().unwrap());
        assert_eq!(token.reserved_bytes(), 2_097_152);
        let mut changed = r.clone();
        changed.project_revision += 1;
        assert_ne!(r.digest().unwrap(), changed.digest().unwrap());
        changed = r.clone();
        changed.base.hex = "A".repeat(40);
        assert!(changed.validate().is_err());
        changed = r.clone();
        changed.base.hex = "0".repeat(40);
        assert!(changed.validate().is_err());
        changed = r.clone();
        changed.operation_id = Uuid::nil();
        assert!(changed.validate().is_err());
        changed = r;
        changed.logical_identity = "x\n".into();
        assert!(changed.validate().is_err());
        assert!(
            serde_json::from_str::<Request>("{\"schema_version\":1,\"chosen_path\":\"x\"}")
                .is_err()
        );
    }
}
