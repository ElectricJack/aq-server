//! Capability-negotiated immutable source and transfer contracts.
use super::*;
pub const SOURCE_CAPABILITY: &str = "source-workspaces-v1";
pub const GRAPH_ACCESS_CAPABILITY: &str = "scoped-agent-access-v1";
pub const SOURCE_TRANSFER_CAPABILITY: &str = "source-transfer-v1";
pub const MAX_CHUNK_BYTES: usize = 256 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectAlgorithm {
    Sha1,
}
wire! {
    ObjectId { algorithm: ObjectAlgorithm, hex: String }
    SourceBinding { repository_id: Uuid, binding_generation: u64, logical_identity: String, base: ObjectId }
    GraphBinding { workspace_id: Uuid, profile_id: String, profile_revision: String, profile_digest: String, context_digest: String }
    ExecutionBindings { source: Option<SourceBinding>, graph: Option<GraphBinding> }
    TransferManifest { transfer_id: Uuid, role: SourceRole, checkpoint: ObjectId, attempt: AttemptRef, resolved_spec_digest: String, source: SourceBinding, artifact_id: Uuid, manifest_digest: String, bundle_digest: Option<String>, manifest_bytes: u64, bundle_bytes: u64 }
    TransferChunkRequest { identity: NodeIdentity, operation_id: Uuid, transfer_id: Uuid, file: TransferFile, offset: u64, length: u32, digest: String }
    TransferStatusRequest { identity: NodeIdentity, operation_id: Uuid, transfer_id: Uuid }
    TransferStatus { transfer_id: Uuid, manifest_offset: u64, bundle_offset: u64, receipt: Option<CentralRetentionReceipt> }
    CentralRetentionReceipt { receipt_id: Uuid, transfer_id: Uuid, repository_id: Uuid, binding_generation: u64, source: ObjectId, retention_handle: Uuid, manifest_digest: String }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransferFile {
    Manifest,
    Bundle,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Base,
    Checkpoint,
}
fn valid_ids(ids: &[Uuid]) -> bool {
    ids.iter().all(|id| !id.is_nil())
}

impl Validate for ObjectId {
    fn validate(&self) -> Result<(), ValidationError> {
        check(lower_hex(&self.hex, 40), "invalid typed oid")
    }
}
impl Validate for SourceBinding {
    fn validate(&self) -> Result<(), ValidationError> {
        self.base.validate()?;
        check(
            !self.repository_id.is_nil()
                && self.binding_generation > 0
                && text(&self.logical_identity, 256)
                && !self.logical_identity.chars().any(char::is_control),
            "invalid source binding",
        )
    }
}
impl Validate for GraphBinding {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.workspace_id.is_nil()
                && text(&self.profile_id, 128)
                && self
                    .profile_revision
                    .parse::<i64>()
                    .is_ok_and(|n| n > 0 && n.to_string() == self.profile_revision)
                && digest(&self.profile_digest)
                && digest(&self.context_digest),
            "invalid graph binding",
        )
    }
}
impl Validate for ExecutionBindings {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.source.is_some() || self.graph.is_some(),
            "empty execution bindings",
        )?;
        if let Some(s) = &self.source {
            s.validate()?;
        }
        if let Some(g) = &self.graph {
            g.validate()?;
        }
        Ok(())
    }
}
const MANIFEST_LIMIT: u64 = 1024 * 1024;
const BUNDLE_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

impl TransferManifest {
    fn valid_bundle(&self) -> bool {
        match &self.bundle_digest {
            None => self.bundle_bytes == 0,
            Some(value) => (1..=BUNDLE_LIMIT).contains(&self.bundle_bytes) && digest(value),
        }
    }
}

impl Validate for TransferManifest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.attempt.validate()?;
        self.source.validate()?;
        self.checkpoint.validate()?;
        check(
            valid_ids(&[self.transfer_id, self.artifact_id])
                && (self.role != SourceRole::Base || self.checkpoint == self.source.base)
                && digest(&self.resolved_spec_digest)
                && digest(&self.manifest_digest)
                && (1..=MANIFEST_LIMIT).contains(&self.manifest_bytes)
                && self.valid_bundle(),
            "invalid transfer manifest",
        )
    }
}
/// Chunk bytes travel as raw HTTP body; descriptor length/digest cover those exact bytes.
impl Validate for TransferChunkRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            valid_ids(&[self.operation_id, self.transfer_id])
                && self.length > 0
                && self.length as usize <= MAX_CHUNK_BYTES
                && self.offset.checked_add(self.length as u64).is_some()
                && digest(&self.digest),
            "invalid transfer chunk",
        )
    }
}
impl Validate for TransferStatusRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            valid_ids(&[self.operation_id, self.transfer_id]),
            "invalid transfer status request",
        )
    }
}
impl Validate for CentralRetentionReceipt {
    fn validate(&self) -> Result<(), ValidationError> {
        self.source.validate()?;
        check(
            valid_ids(&[
                self.receipt_id,
                self.transfer_id,
                self.repository_id,
                self.retention_handle,
            ]) && self.binding_generation > 0
                && digest(&self.manifest_digest),
            "invalid retention receipt",
        )
    }
}
impl Validate for TransferStatus {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.transfer_id.is_nil()
                && self.manifest_offset <= MANIFEST_LIMIT
                && self.bundle_offset <= BUNDLE_LIMIT,
            "invalid transfer status",
        )?;
        if let Some(r) = &self.receipt {
            r.validate()?;
            check(
                r.transfer_id == self.transfer_id,
                "receipt transfer mismatch",
            )?;
        }
        Ok(())
    }
}
