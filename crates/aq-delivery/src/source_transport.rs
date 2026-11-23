//! Exact aq-git source transfer manifests, observations and import acknowledgements.
pub mod files;
#[cfg(any(test, feature = "test-support"))]
pub mod test_peer;
use aq_worker_protocol::{CentralRetentionReceipt, ObjectId, SourceBinding, TransferManifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// Identity and state returned by the fixed Git retention.inspect operation.
pub struct RetentionRecord<'a>(pub &'a Value);

impl RetentionRecord<'_> {
    pub fn matches_checkpoint(&self, manifest: &TransferManifest) -> bool {
        self.matches_handle(
            manifest.artifact_id,
            manifest.source.repository_id,
            manifest.source.binding_generation,
        ) && self.active_transfer()
            && self.matches_ref(manifest.artifact_id, &manifest.checkpoint.hex)
    }
    pub fn matches_handle(
        &self,
        artifact: Uuid,
        repository: Uuid,
        binding_generation: u64,
    ) -> bool {
        let value = self.0;
        value["version"] == 1
            && value["id"] == artifact.to_string()
            && value["repository_id"] == repository.to_string()
            && value["binding_generation"] == binding_generation
    }
    pub fn active_transfer(&self) -> bool {
        let value = self.0;
        value["generation"]
            .as_i64()
            .is_some_and(|generation| generation > 0)
            && value["state"] == "active"
            && value["resource"] == "transfer"
    }
    pub fn unreleased(&self) -> bool {
        self.0.get("release_operation") == Some(&Value::Null)
            && self.0.get("release_reason") == Some(&Value::Null)
    }
    pub fn matches_ref(&self, artifact: Uuid, oid: &str) -> bool {
        self.0["refs"] == json!([{"name":format!("refs/aq-git/transfers/{artifact}"),"oid":oid}])
    }
}

pub fn successful_reply(reply: &Value, request_id: &Value) -> bool {
    reply["protocol_version"] == 1
        && reply["request_id"] == *request_id
        && reply["ok"] == true
        && reply.get("error").is_none()
}

// Field order matches aq-git v1 Manifest exactly: its receipt hashes this
// canonical serialization, independently of transported JSON whitespace.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitManifest {
    pub format_version: u32,
    pub repository_id: String,
    pub repository_identity: String,
    pub binding_generation: u64,
    pub transfer_id: String,
    pub object_format: String,
    pub base: ObjectId,
    pub source: ObjectId,
    pub prerequisites: Vec<ObjectId>,
    pub bundle_size: u64,
    pub bundle_sha256: Option<String>,
    pub producer_installation_id: String,
    pub reference_only: bool,
    #[serde(default)]
    pub caller_correlation: Value,
}

impl GitManifest {
    pub fn matches_repository(&self, b: &SourceBinding) -> bool {
        self.repository_id == b.repository_id.to_string()
            && self.repository_identity == b.logical_identity
            && self.binding_generation == b.binding_generation
            && self.base == b.base
    }
    pub fn matches_bundle(&self, manifest: &TransferManifest) -> bool {
        self.transfer_id == manifest.artifact_id.to_string()
            && self.bundle_size == manifest.bundle_bytes
            && self.bundle_sha256 == manifest.bundle_digest
    }
    pub fn matches_transfer(&self, manifest: &TransferManifest) -> bool {
        self.supported_format()
            && self.matches_repository(&manifest.source)
            && self.matches_bundle(manifest)
    }
    pub fn is_full_base(&self, base: &ObjectId, max_bundle: u64) -> bool {
        let full_base = self.base == *base && self.source == *base && self.prerequisites.is_empty();
        self.supported_format()
            && full_base
            && !self.reference_only
            && (1..=max_bundle).contains(&self.bundle_size)
    }
    pub fn has_producer(&self) -> bool {
        Uuid::parse_str(&self.producer_installation_id).is_ok_and(|id| !id.is_nil())
    }
    pub fn matches_export_response(
        &self,
        response: &Value,
        operation: Uuid,
    ) -> serde_json::Result<bool> {
        let operation = operation.to_string();
        Ok(response.as_object().is_some_and(|v| v.len() == 3)
            && response["artifact_handle"] == operation
            && response["retention_handle"] == operation
            && response["manifest"] == serde_json::to_value(self)?)
    }
    pub fn supported_format(&self) -> bool {
        self.format_version == 1
            && self.object_format == "sha1"
            && self.reference_only == (self.bundle_size == 0)
    }
}

/// Validate the aq-git acknowledgement against the frozen projection. A late
/// completion can be historical. The receiver remains part of the evidence.
pub fn matches_import_receipt(
    value: &Value,
    receipt: &CentralRetentionReceipt,
) -> serde_json::Result<bool> {
    let artifact = receipt.retention_handle.to_string();
    let repository = value["repository_id"] == receipt.repository_id.to_string()
        && value["binding_generation"] == receipt.binding_generation;
    let transport = value["transfer_id"] == artifact
        && value["retention_handle"] == artifact
        && value["manifest_digest"] == receipt.manifest_digest;
    Ok(repository
        && transport
        && value["source"] == serde_json::to_value(&receipt.source)?
        && valid_receiver(value))
}
fn valid_receiver(value: &Value) -> bool {
    value["retention_state"] == "active"
        && value["historical"].is_boolean()
        && receiver_id(value).is_some()
}
pub fn receiver_id(value: &Value) -> Option<Uuid> {
    value["receiver_installation_id"]
        .as_str()
        .and_then(|v| Uuid::parse_str(v).ok())
}

/// Check the guard generation and the exact retained refs and operation in a release reply.
pub fn release_confirmed(request: &Value, response: &Value) -> bool {
    let params = &request["params"];
    let requested = (
        &params["handle_id"],
        &request["repository_id"],
        &request["expected_binding_generation"],
    );
    let actual = (
        &response["id"],
        &response["repository_id"],
        &response["binding_generation"],
    );
    let advanced = response["generation"]
        .as_u64()
        .zip(params["expected_generation"].as_u64())
        .is_some_and(|(actual, expected)| actual > expected);
    response["version"] == 1
        && actual == requested
        && advanced
        && released_as_requested(response, request)
}
fn released_as_requested(response: &Value, request: &Value) -> bool {
    let params = &request["params"];
    response["state"] == "released"
        && response["refs"] == params["refs"]
        && response["release_operation"] == request["operation_id"]
        && response["release_reason"] == params["reason"]
}

#[cfg(test)]
mod tests;
