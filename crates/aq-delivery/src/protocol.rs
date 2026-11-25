//! The finite central aq-git v1 protocol surface. No arbitrary caller JSON is sent.
use super::{DeliveryError, OperationContext, Result};
use aq_worker_protocol::{ObjectId, Validate};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

pub const MAX_REQUEST: usize = 1024 * 1024;
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitMetadata {
    pub name: String,
    pub email: String,
    pub message: String,
}
impl CommitMetadata {
    pub(super) fn validate(&self) -> Result<()> {
        let valid_identity =
            |value: &str| !value.is_empty() && !value.contains(['\n', '\r', '\0', '<', '>']);
        let identity = valid_identity(&self.name) && valid_identity(&self.email);
        let message = self.message.len() <= 65536 && !self.message.contains('\0');
        if !identity || !message {
            return Err(DeliveryError::InvalidInput("invalid commit metadata"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedSource {
    pub oid: ObjectId,
    pub retention_handle: Uuid,
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Ready,
    Conflicted,
}
#[derive(Debug, Clone, Deserialize)]
pub struct ConflictPath {
    pub encoding: String,
    pub value: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct CandidateResult {
    pub candidate_id: Uuid,
    pub state: CandidateState,
    pub candidate: Option<ObjectId>,
    pub clean_prefix: ObjectId,
    pub retention_handle: Uuid,
    pub conflict_step: Option<usize>,
    pub conflicting_paths: Vec<ConflictPath>,
    pub conflict_paths_truncated: bool,
    pub conflict_path_count: usize,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PublishResult {
    Applied {
        target: String,
        old: ObjectId,
        new: ObjectId,
        acknowledged_at: String,
    },
    AlreadyPresent {
        observed_target: ObjectId,
        observed_at: String,
        caused_by_this_operation: bool,
    },
    /// `target_moved` carries the observed remote OID; other reasons may not.
    Rejected {
        reason: String,
        #[serde(default)]
        observed_target: Option<ObjectId>,
        #[serde(default)]
        observed_at: Option<String>,
    },
}
/// aq-git §10.3 `operation.reconcile` / `operation.resolve` reply. Exactly one of
/// a settling `resolution` or a still-ambiguous `outcome: "indeterminate"`.
#[derive(Debug, Clone, Deserialize)]
pub struct ReconcileResult {
    pub target_operation_id: Uuid,
    #[serde(default)]
    pub resolution: Option<Resolution>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub observed_target: Option<ObjectId>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub observation_error: Option<String>,
}
impl ReconcileResult {
    /// Observed presence or an explicit owner resolution. Never `applied`:
    /// historical remote effect of the original dispatch stays unknown.
    pub fn is_settled(&self) -> bool {
        self.resolution.is_some()
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Resolution {
    AlreadyPresent {
        observed_target: ObjectId,
        observed_at: String,
        caused_by_this_operation: bool,
    },
    Resolved {
        disposition: String,
        reason: String,
        evidence: Value,
        resolved_at: String,
    },
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Pending,
    Complete,
    Failed,
}
#[derive(Debug, Clone, Deserialize)]
pub struct OperationResult {
    pub version: u32,
    pub id: Uuid,
    pub generation: u64,
    pub status: OperationStatus,
    pub phase: String,
    pub payload: Value,
    pub payload_hash: String,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub metadata: Value,
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetryAdvice {
    SafeSameOperation,
    RefreshInputs,
    ReconcileFirst,
    Manual,
}
#[derive(Debug, Clone, Deserialize)]
pub struct GitError {
    pub code: String,
    pub message: String,
    pub details: Value,
    pub retry: RetryAdvice,
}
impl GitError {
    pub(super) fn retryable_candidate(&self) -> bool {
        matches!(self.code.as_str(), "overloaded" | "io_error" | "state_busy")
            && self.retry == RetryAdvice::SafeSameOperation
            && !self.uncertain()
    }
    pub(super) fn uncertain(&self) -> bool {
        self.code == "operation_indeterminate"
            || self.retry == RetryAdvice::ReconcileFirst
            || self.details["dispatched"] == true
            || self.details["remote_effect_unknown"] == true
    }
}
/// Historical exact response, not a claim of a new remote effect on replay.
#[derive(Debug, Clone)]
pub struct Receipt<T> {
    pub raw: Vec<u8>,
    pub operation_state: Option<OperationStatus>,
    pub outcome: std::result::Result<T, GitError>,
}
pub(super) trait WireResult: DeserializeOwned {
    fn validate(&self, context: &OperationContext, operation: &str, params: &Value) -> bool;
}
impl WireResult for OperationResult {
    fn validate(&self, c: &OperationContext, _: &str, params: &Value) -> bool {
        self.version == 1
            && params["target_operation_id"] == self.id.to_string()
            && self.payload["repository_id"] == c.repository_id.to_string()
            && self.payload["expected_binding_generation"] == c.binding_generation
    }
}
mod candidate;
mod envelope;
mod publication;
pub(super) use envelope::decode;
pub(super) async fn exchange(path: &Path, request: &[u8]) -> Result<Vec<u8>> {
    exchange_within(path, request, Duration::from_secs(35)).await
}
/// [`exchange`] with the caller's wait, for operations that reach a remote.
pub(super) async fn exchange_within(
    path: &Path,
    request: &[u8],
    wait: Duration,
) -> Result<Vec<u8>> {
    if request.len() > MAX_REQUEST {
        return Err(DeliveryError::InvalidInput("request exceeds 1 MiB"));
    }
    let call = async {
        let mut stream = tokio::net::UnixStream::connect(path).await?;
        stream.write_all(request).await?;
        let mut reader = BufReader::new(stream).take((MAX_RESPONSE + 1) as u64);
        let mut raw = Vec::new();
        reader.read_until(b'\n', &mut raw).await?;
        if raw.len() > MAX_RESPONSE || raw.last() != Some(&b'\n') {
            return Err(DeliveryError::Protocol);
        }
        Ok(raw)
    };
    tokio::time::timeout(wait, call)
        .await
        .map_err(|_| DeliveryError::Transport)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn delivery_frames_enforce_both_limits_and_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("socket");
        assert!(matches!(
            exchange(&path, &vec![b'x'; MAX_REQUEST + 1]).await,
            Err(DeliveryError::InvalidInput(_))
        ));
        for (n, newline, accepted) in [
            (MAX_RESPONSE, true, true),
            (MAX_RESPONSE + 1, true, false),
            (16, false, false),
        ] {
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let peer = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut request = Vec::new();
                reader.read_until(b'\n', &mut request).await.unwrap();
                let mut bytes = vec![b' '; n];
                if newline {
                    bytes[n - 1] = b'\n';
                }
                let _ = reader.get_mut().write_all(&bytes).await;
            });
            assert_eq!(exchange(&path, b"{}\n").await.is_ok(), accepted);
            peer.await.unwrap();
            std::fs::remove_file(&path).unwrap();
        }
    }
    #[test]
    fn delivery_envelopes_reject_uncorrelated_contradictory_and_causal_presence() {
        let c = OperationContext {
            operation_id: Uuid::new_v4(),
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
        };
        let good = json!({"protocol_version":1,"request_id":c.operation_id.to_string(),"operation_id":c.operation_id,"ok":true,"result":{"outcome":"already_present","observed_target":{"algorithm":"sha1","hex":"a".repeat(40)},"observed_at":"2026-11-02T00:00:00Z","caused_by_this_operation":false}});
        assert!(
            decode::<PublishResult>(
                serde_json::to_vec(&good).unwrap(),
                &c,
                "publish",
                &json!({})
            )
            .is_ok()
        );
        for case in 0..6 {
            let mut v = good.clone();
            match case {
                0 => v["request_id"] = json!("wrong"),
                1 => v["operation_id"] = json!(Uuid::new_v4()),
                2 => v["protocol_version"] = json!(2),
                3 => v["error"] = json!({}),
                4 => v["result"]["caused_by_this_operation"] = json!(true),
                _ => {
                    v.as_object_mut().unwrap().remove("result");
                }
            }
            assert!(
                matches!(
                    decode::<PublishResult>(
                        serde_json::to_vec(&v).unwrap(),
                        &c,
                        "publish",
                        &json!({})
                    ),
                    Err(DeliveryError::Protocol)
                ),
                "case {case}"
            );
        }
        let error = json!({"protocol_version":1,"request_id":c.operation_id.to_string(),"ok":false,"error":{"code":"operation_indeterminate","message":"unknown","details":{},"retry":"reconcile_first"}});
        let decoded = decode::<PublishResult>(
            serde_json::to_vec(&error).unwrap(),
            &c,
            "publish",
            &json!({}),
        )
        .unwrap();
        assert!(decoded.outcome.unwrap_err().uncertain());
    }
}
