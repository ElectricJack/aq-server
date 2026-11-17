use aq_worker_protocol::{ObjectId, SourceRole, TransferManifest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

pub fn digest(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
pub async fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
/// The aq-git artifact handle of a pushed transfer. A retry of the same
/// transfer must name the same artifact, so it is derived, not drawn.
pub fn artifact_id(transfer: Uuid) -> Uuid {
    let mut h = Sha256::new();
    h.update(b"aq-source-push/artifact/v1/");
    h.update(transfer.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&h.finalize()[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
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
    /// The staged aq-git manifest names exactly the frozen source binding and
    /// the bundle the transfer declared.
    pub fn binds(&self, t: &TransferManifest) -> bool {
        let (m, b) = (self, &t.source);
        m.format_version == 1
            && m.repository_id == b.repository_id.to_string()
            && m.repository_identity == b.logical_identity
            && m.binding_generation == b.binding_generation
            && m.base == b.base
            && m.transfer_id == t.artifact_id.to_string()
            && m.bundle_size == t.bundle_bytes
            && m.bundle_sha256 == t.bundle_digest
            && m.object_format == "sha1"
            && m.reference_only == (m.bundle_size == 0)
    }
    pub fn is_checkpoint_of(&self, t: &TransferManifest) -> bool {
        self.source == t.checkpoint && t.role == SourceRole::Checkpoint
    }
}
/// One request frame to the aq-git socket and its confirmed `result`. The
/// caller builds the request from a finite allowlist and bounds the wait; an
/// error here means the outcome is unknown, never that the operation failed.
pub async fn socket_call(socket: &Path, request: &Value) -> anyhow::Result<Value> {
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let mut bytes = serde_json::to_vec(request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut frame = Vec::new();
    let mut reader = BufReader::new(stream).take(1024 * 1024 + 1);
    reader.read_until(b'\n', &mut frame).await?;
    anyhow::ensure!(
        frame.len() <= 1024 * 1024 && frame.last() == Some(&b'\n'),
        "invalid aq-git frame"
    );
    let v: Value = serde_json::from_slice(&frame)?;
    anyhow::ensure!(
        v["protocol_version"] == 1
            && v["request_id"] == request["request_id"]
            && v["operation_id"] == request["operation_id"]
            && v["ok"] == true
            && v.get("error").is_none()
            && v.get("result").is_some(),
        "aq-git outcome not confirmed: {}",
        v
    );
    Ok(v["result"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transfer_always_names_the_same_artifact() {
        let transfer = Uuid::from_u128(7);
        let artifact = artifact_id(transfer);
        assert_eq!(artifact, artifact_id(transfer));
        assert_ne!(artifact, artifact_id(Uuid::from_u128(8)));
        assert_ne!(artifact, transfer);
        assert!(!artifact.is_nil());
        assert_eq!(artifact.get_version_num(), 8);
    }
}
