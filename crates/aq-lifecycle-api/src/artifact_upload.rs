//! Worker request and receipt values for the lifecycle artifact upload protocol.
use crate::{ArtifactRef, ContractError, Text, worker_lifecycle as lc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Begin one artifact upload. `bytes` is the exact artifact length and
/// `artifact.digest` the sha256 the committed bytes must hash to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginArtifact {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub artifact: ArtifactRef,
    pub role: lc::ArtifactRole,
    pub bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTransferStatus {
    pub transfer_id: Uuid,
    pub offset: u64,
    pub bytes: u64,
    pub state: String,
    pub receipt: Option<lc::RetentionReceipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitArtifact {
    pub operation_id: Uuid,
}
impl ArtifactTransferStatus {
    pub fn uploading(transfer_id: Uuid, bytes: u64) -> Self {
        Self {
            transfer_id,
            offset: 0,
            bytes,
            state: "uploading".into(),
            receipt: None,
        }
    }
}

/// The frozen binding, declared artifact and acknowledged progress of an upload.
pub struct Upload {
    pub receipt_id: Uuid,
    pub artifact: ArtifactRef,
    pub role: lc::ArtifactRole,
    pub binding: lc::Binding,
    pub bytes: u64,
    pub offset: u64,
    pub state: String,
    pub receipt: Option<lc::RetentionReceipt>,
}
impl Upload {
    pub fn status(&self, id: Uuid) -> ArtifactTransferStatus {
        ArtifactTransferStatus {
            transfer_id: id,
            offset: self.offset,
            bytes: self.bytes,
            state: self.state.clone(),
            receipt: self.receipt.clone(),
        }
    }
    /// Validate the envelope against the acknowledged offset before writing a chunk.
    pub fn accept_chunk(&self, offset: u64, length: u64) -> Result<(), &'static str> {
        if self.state != "uploading" {
            return Err("transfer no longer accepts chunks");
        }
        if offset != self.offset || self.offset.saturating_add(length) > self.bytes {
            return Err("chunk offset or declared length mismatch");
        }
        Ok(())
    }
    /// Issue the canonical receipt for the frozen binding and retention window.
    pub fn retention_receipt(
        &self,
        now: u64,
        until: u64,
    ) -> Result<lc::RetentionReceipt, ContractError> {
        let receipt = lc::RetentionReceipt {
            receipt_id: self.receipt_id,
            owner: Text::new(format!("aq-server:{}", self.binding.project_id))?,
            binding_digest: lc::canonical_digest(&self.binding)?,
            artifact: self.artifact.clone(),
            role: self.role.clone(),
            bytes: self.bytes,
            retained_at_ms: now,
            retained_until_ms: until,
        };
        lc::validate_retention(std::slice::from_ref(&receipt), &self.binding)?;
        Ok(receipt)
    }
}
