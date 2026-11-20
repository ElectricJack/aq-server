//! The three aq-git operations the delivery commands use (spec 0016 sections
//! 4 and 6.2), as a port and its socket client.
//!
//! Wire shapes: `repo.retain_ref` and `ref.delete` follow aq-git's epic 0
//! tasks (spec 0016 section 4 and task G3); `ref.publish` is the publication
//! of a retained ref under the binding's allowed prefix with a lease. They are
//! sent in the same frame as every other aq-git operation
//! (`protocol_version`, `request_id` = `operation_id`, `operation`,
//! `repository_id`, `expected_binding_generation`, `params`). The operation
//! id is aq-git's idempotency key, so a repeated call with the same id is
//! safe. Mechanism only: nothing here decides whether to publish.
use super::GitTarget;
use crate::{DeliveryError, protocol};
use serde_json::{Value, json};
use std::path::PathBuf;
use uuid::Uuid;

/// The prefix a task branch lives under, in central aq-git and on the forge.
pub const TASK_BRANCH_PREFIX: &str = "refs/heads/aq/";

/// A commit retained under a handle by `repo.retain_ref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedRef {
    pub head: String,
    pub retention_handle: Uuid,
}

/// What publishing a ref to the remote came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefPublication {
    /// The remote ref is now at `head` (written now, or already there).
    Published { head: String },
    /// The lease did not hold: the remote ref is not where the caller said.
    Moved,
    /// Central aq-git has no such ref.
    NoRef,
}

/// What deleting a ref, on the remote and in the central pool, came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefDeletion {
    Deleted,
    AlreadyAbsent,
}

/// A call that did not answer as an operation result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("aq-git {code}: {message}")]
pub struct GitFailure {
    /// aq-git's error code, or `transport` / `protocol` for this side.
    pub code: String,
    pub message: String,
    /// The same operation may be sent again (aq-git said so, or no answer).
    pub transient: bool,
}
impl GitFailure {
    pub(crate) fn transport() -> Self {
        Self {
            code: "transport".into(),
            message: "aq-git did not answer".into(),
            transient: true,
        }
    }
    fn protocol() -> Self {
        Self {
            code: "protocol".into(),
            message: "invalid or incompatible aq-git response".into(),
            transient: false,
        }
    }
}

#[async_trait::async_trait]
pub trait GitPort: Send + Sync {
    /// Syncs `reference` from the remote and pins its head (`repo.retain_ref`).
    async fn retain_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<RetainedRef, GitFailure>;
    /// Publishes `reference` (under [`TASK_BRANCH_PREFIX`]) to the remote under
    /// the same name. `expected_old` is the lease: the commit the remote ref is
    /// at, or `None` when it must be absent.
    async fn publish_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_old: Option<&str>,
    ) -> Result<RefPublication, GitFailure>;
    /// Reads the central task branch before publishing a recorded result.
    async fn task_head(
        &self,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<Option<String>, GitFailure>;
    /// Deletes `reference` on the remote and in the central pool (`ref.delete`).
    /// The lease holds even when one side is already absent.
    async fn delete_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> Result<RefDeletion, GitFailure>;
}

/// A hex object id as aq-git writes it: 40 or 64 lowercase hex digits.
pub fn object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn zero_id(like: usize) -> String {
    "0".repeat(like)
}

/// The aq-git socket (`SourceRetentionConfig::git_socket`).
#[derive(Debug, Clone)]
pub struct SocketGit {
    socket: PathBuf,
}
impl SocketGit {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    async fn call(
        &self,
        operation: Uuid,
        name: &str,
        repo: &GitTarget,
        params: Value,
    ) -> Result<Value, GitFailure> {
        let request = json!({
            "protocol_version": 1, "request_id": operation.to_string(),
            "operation_id": operation, "operation": name,
            "repository_id": repo.repository_id,
            "expected_binding_generation": repo.binding_generation,
            "params": params, "deadline_ms": 30000,
        });
        let mut bytes = serde_json::to_vec(&request).map_err(|_| GitFailure::protocol())?;
        bytes.push(b'\n');
        let raw = protocol::exchange(&self.socket, &bytes)
            .await
            .map_err(|e| match e {
                DeliveryError::Protocol => GitFailure::protocol(),
                _ => GitFailure::transport(),
            })?;
        let v: Value = serde_json::from_slice(&raw).map_err(|_| GitFailure::protocol())?;
        if v["protocol_version"] != 1 || v["request_id"] != operation.to_string() {
            return Err(GitFailure::protocol());
        }
        match v["ok"].as_bool() {
            Some(true) if v["result"].is_object() => Ok(v["result"].clone()),
            Some(false) => {
                let text = |value: &Value| {
                    value
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(256)
                        .collect::<String>()
                };
                Err(GitFailure {
                    code: text(&v["error"]["code"]),
                    message: text(&v["error"]["message"]),
                    transient: v["error"]["retry"] == "safe_same_operation",
                })
            }
            _ => Err(GitFailure::protocol()),
        }
    }
}

impl SocketGit {
    /// `ref.show {ref}` (spec 0016 §5.1): the commit a branch of the central
    /// repository is at, or `None` when the branch is absent. A read.
    pub async fn show_ref(
        &self,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<Option<String>, GitFailure> {
        let params = json!({"ref": reference});
        let result = self.call(Uuid::new_v4(), "ref.show", repo, params).await?;
        let oid = &result["oid"];
        if result["ref"] != reference || !(oid.is_null() || oid["algorithm"] == "sha1") {
            return Err(GitFailure::protocol());
        }
        match oid["hex"].as_str() {
            None if oid.is_null() => Ok(None),
            Some(hex) if object_id(hex) => Ok(Some(hex.to_owned())),
            _ => Err(GitFailure::protocol()),
        }
    }
}

fn head_of(result: &Value) -> Result<String, GitFailure> {
    result["head"]
        .as_str()
        .filter(|h| object_id(h))
        .map(str::to_owned)
        .ok_or_else(GitFailure::protocol)
}
fn task_ref(reference: &str) -> Result<&str, GitFailure> {
    if reference.starts_with(TASK_BRANCH_PREFIX) && crate::branch(reference).is_ok() {
        return Ok(reference);
    }
    Err(GitFailure {
        code: "invalid_ref".into(),
        message: format!("only refs under {TASK_BRANCH_PREFIX} may be published or deleted"),
        transient: false,
    })
}

#[async_trait::async_trait]
impl GitPort for SocketGit {
    async fn retain_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<RetainedRef, GitFailure> {
        if crate::branch(reference).is_err() {
            return Err(GitFailure {
                code: "invalid_ref".into(),
                message: "not a full branch ref".into(),
                transient: false,
            });
        }
        let result = self
            .call(
                operation,
                "repo.retain_ref",
                repo,
                json!({"ref": reference}),
            )
            .await?;
        Ok(RetainedRef {
            head: head_of(&result)?,
            retention_handle: result["retention_handle"]
                .as_str()
                .and_then(|h| Uuid::parse_str(h).ok())
                .ok_or_else(GitFailure::protocol)?,
        })
    }

    async fn publish_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_old: Option<&str>,
    ) -> Result<RefPublication, GitFailure> {
        let reference = task_ref(reference)?;
        let old = match expected_old {
            Some(old) if object_id(old) => old.to_owned(),
            Some(_) => {
                return Err(GitFailure {
                    code: "invalid_lease".into(),
                    message: "expected_old is not an object id".into(),
                    transient: false,
                });
            }
            None => zero_id(40),
        };
        let result = self
            .call(
                operation,
                "ref.publish",
                repo,
                json!({"ref": reference, "target": reference, "expected_old": old}),
            )
            .await;
        let result = match result {
            Ok(r) => r,
            Err(e) if e.code == "ref_not_found" => return Ok(RefPublication::NoRef),
            Err(e) => return Err(e),
        };
        match result["outcome"].as_str() {
            Some("applied" | "already_present") => Ok(RefPublication::Published {
                head: head_of(&result)?,
            }),
            Some("rejected") => match result["reason"].as_str() {
                Some("target_moved") => Ok(RefPublication::Moved),
                Some("ref_not_found") => Ok(RefPublication::NoRef),
                _ => Err(GitFailure {
                    code: "rejected".into(),
                    message: result["reason"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(256)
                        .collect(),
                    transient: false,
                }),
            },
            _ => Err(GitFailure::protocol()),
        }
    }

    async fn task_head(
        &self,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<Option<String>, GitFailure> {
        self.show_ref(repo, task_ref(reference)?).await
    }

    async fn delete_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> Result<RefDeletion, GitFailure> {
        let reference = task_ref(reference)?;
        let lease = expected_head
            .map(|hex| {
                if hex.len() != 40 || !object_id(hex) {
                    return Err(GitFailure::protocol());
                }
                Ok(json!({"algorithm": "sha1", "hex": hex}))
            })
            .transpose()?;
        let result = self
            .call(
                operation,
                "ref.delete",
                repo,
                json!({"ref": reference, "remote": true, "expected_old": lease}),
            )
            .await?;
        match result["outcome"].as_str() {
            Some("deleted") => Ok(RefDeletion::Deleted),
            Some("already_absent") => Ok(RefDeletion::AlreadyAbsent),
            _ => Err(GitFailure::protocol()),
        }
    }
}
