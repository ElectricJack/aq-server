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
        if self.name.is_empty()
            || self.email.is_empty()
            || self.message.len() > 65536
            || self.name.contains(['\n', '\r', '\0', '<', '>'])
            || self.email.contains(['\n', '\r', '\0', '<', '>'])
            || self.message.contains('\0')
        {
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
impl WireResult for CandidateResult {
    fn validate(&self, c: &OperationContext, operation: &str, params: &Value) -> bool {
        let expected = if operation == "candidate.create" {
            c.operation_id.to_string()
        } else {
            params["candidate_id"].as_str().unwrap_or_default().into()
        };
        if self.candidate_id.to_string() != expected
            || self.retention_handle != self.candidate_id
            || self.clean_prefix.validate().is_err()
            || self.clean_prefix.hex.bytes().all(|b| b == b'0')
            || self.conflicting_paths.len() > 10_000
            || self
                .conflicting_paths
                .iter()
                .map(|p| p.value.len())
                .sum::<usize>()
                > 1024 * 1024
            || self.conflicting_paths.iter().any(|p| {
                p.encoding != "hex"
                    || p.value.is_empty()
                    || !p.value.len().is_multiple_of(2)
                    || !p
                        .value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            || self.conflict_path_count < self.conflicting_paths.len()
            || (!self.conflict_paths_truncated
                && self.conflict_path_count != self.conflicting_paths.len())
        {
            return false;
        }
        match self.state {
            CandidateState::Ready => {
                self.candidate
                    .as_ref()
                    .is_some_and(|v| v.validate().is_ok() && *v == self.clean_prefix)
                    && self.conflict_step.is_none()
                    && self.conflicting_paths.is_empty()
                    && self.conflict_path_count == 0
                    && !self.conflict_paths_truncated
            }
            CandidateState::Conflicted => {
                self.candidate.is_none()
                    && self.conflict_step.is_some_and(|step| {
                        step < 64
                            && (operation != "candidate.create"
                                || params["sources"].as_array().is_some_and(|s| step < s.len()))
                    })
            }
        }
    }
}
impl WireResult for PublishResult {
    fn validate(&self, _: &OperationContext, _: &str, params: &Value) -> bool {
        match self {
            Self::Applied {
                target,
                old,
                new,
                acknowledged_at,
            } => {
                params["target"] == *target
                    && serde_json::to_value(old).ok().as_ref() == Some(&params["expected_old"])
                    && old.validate().is_ok()
                    && new.validate().is_ok()
                    && serde_json::to_value(new).ok().as_ref() == Some(&params["candidate"])
                    && !new.hex.bytes().all(|b| b == b'0')
                    && chrono::DateTime::parse_from_rfc3339(acknowledged_at).is_ok()
            }
            Self::AlreadyPresent {
                observed_target,
                observed_at,
                caused_by_this_operation,
            } => {
                !caused_by_this_operation
                    && observed_target.validate().is_ok()
                    && !observed_target.hex.bytes().all(|b| b == b'0')
                    && chrono::DateTime::parse_from_rfc3339(observed_at).is_ok()
            }
            Self::Rejected {
                reason,
                observed_target,
                observed_at,
            } => {
                !reason.is_empty()
                    && observed_target
                        .as_ref()
                        .is_none_or(|o| o.validate().is_ok())
                    && observed_at
                        .as_ref()
                        .is_none_or(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
            }
        }
    }
}
impl WireResult for ReconcileResult {
    fn validate(&self, _: &OperationContext, operation: &str, params: &Value) -> bool {
        if params["target_operation_id"] != self.target_operation_id.to_string()
            || self
                .observed_target
                .as_ref()
                .is_some_and(|o| o.validate().is_err())
        {
            return false;
        }
        match (&self.resolution, self.outcome.as_deref()) {
            (None, Some("indeterminate")) => operation == "operation.reconcile",
            (
                Some(Resolution::AlreadyPresent {
                    observed_target,
                    observed_at,
                    caused_by_this_operation,
                }),
                None,
            ) => {
                operation == "operation.reconcile"
                    && !caused_by_this_operation
                    && observed_target.validate().is_ok()
                    && !observed_target.hex.bytes().all(|b| b == b'0')
                    && chrono::DateTime::parse_from_rfc3339(observed_at).is_ok()
            }
            (
                Some(Resolution::Resolved {
                    disposition,
                    reason,
                    resolved_at,
                    ..
                }),
                None,
            ) => {
                operation == "operation.resolve"
                    && params["disposition"] == *disposition
                    && params["reason"] == *reason
                    && chrono::DateTime::parse_from_rfc3339(resolved_at).is_ok()
            }
            _ => false,
        }
    }
}
impl WireResult for OperationResult {
    fn validate(&self, c: &OperationContext, _: &str, params: &Value) -> bool {
        self.version == 1
            && params["target_operation_id"] == self.id.to_string()
            && self.payload["repository_id"] == c.repository_id.to_string()
            && self.payload["expected_binding_generation"] == c.binding_generation
    }
}
pub(super) fn decode<T: WireResult>(
    raw: Vec<u8>,
    c: &OperationContext,
    operation: &str,
    params: &Value,
) -> Result<Receipt<T>> {
    let v: Value = serde_json::from_slice(&raw).map_err(|_| DeliveryError::Protocol)?;
    if v["protocol_version"] != 1
        || v["request_id"] != c.operation_id.to_string()
        || v.get("operation_id")
            .is_some_and(|id| *id != c.operation_id.to_string())
        || !v["ok"].is_boolean()
        || (v.get("result").is_some() == v.get("error").is_some())
    {
        return Err(DeliveryError::Protocol);
    }
    let operation_state: Option<OperationStatus> = v
        .get("operation_state")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| DeliveryError::Protocol)?;
    let outcome = if v["ok"] == true {
        if matches!(operation, "candidate.create" | "publish")
            && operation_state
                .as_ref()
                .is_some_and(|s| *s != OperationStatus::Complete)
        {
            return Err(DeliveryError::Protocol);
        }
        if matches!(operation, "candidate.create" | "candidate.show")
            && (v["result"].get("candidate").is_none()
                || v["result"].get("conflict_step").is_none())
        {
            return Err(DeliveryError::Protocol);
        }
        if matches!(operation, "candidate.create" | "publish") && v.get("operation_id").is_none() {
            return Err(DeliveryError::Protocol);
        }
        let result: T =
            serde_json::from_value(v.get("result").ok_or(DeliveryError::Protocol)?.clone())
                .map_err(|_| DeliveryError::Protocol)?;
        if !result.validate(c, operation, params) {
            return Err(DeliveryError::Protocol);
        }
        Ok(result)
    } else {
        let error: GitError =
            serde_json::from_value(v.get("error").ok_or(DeliveryError::Protocol)?.clone())
                .map_err(|_| DeliveryError::Protocol)?;
        if error.code.is_empty() {
            return Err(DeliveryError::Protocol);
        }
        Err(error)
    };
    Ok(Receipt {
        raw,
        operation_state,
        outcome,
    })
}
pub(super) async fn exchange(path: &Path, request: &[u8]) -> Result<Vec<u8>> {
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
    tokio::time::timeout(Duration::from_secs(35), call)
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
