//! The three aq-git operations the delivery commands use (spec 0016 sections
//! 4 and 6.2), as a port and its socket client.
//!
//! Wire shapes: `repo.retain_ref` and `ref.delete` follow aq-git's epic 0
//! tasks (spec 0016 section 4 and task G3); `publish` is the publication
//! of a retained ref under the binding's allowed prefix with a lease. They are
//! sent in the same frame as every other aq-git operation
//! (`protocol_version`, `request_id` = `operation_id`, `operation`,
//! `repository_id`, `expected_binding_generation`, `params`). The operation
//! id is aq-git's idempotency key, so a repeated call with the same id is
//! safe. Mechanism only: nothing here decides whether to publish.
use super::GitTarget;
use crate::{DeliveryError, protocol};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use uuid::Uuid;

/// The prefix a task branch lives under, in central aq-git and on the forge.
pub const TASK_BRANCH_PREFIX: &str = "refs/heads/aq/";
/// Spec 0017 §6.2: the namespace of the attempts' try refs, never published.
pub const TRY_PREFIX: &str = "refs/heads/aq-try/";

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
    /// aq-git's `details`, such as the operation that holds a destination.
    pub details: Value,
}
impl GitFailure {
    pub(crate) fn transport() -> Self {
        Self {
            code: "transport".into(),
            message: "aq-git did not answer".into(),
            transient: true,
            details: Value::Null,
        }
    }
    fn protocol() -> Self {
        Self {
            code: "protocol".into(),
            message: "invalid or incompatible aq-git response".into(),
            transient: false,
            details: Value::Null,
        }
    }
}

#[async_trait::async_trait]
pub trait GitPort: Send + Sync {
    /// Read the original publisher journal; unsupported implementations fail closed.
    async fn inspect_publication(
        &self,
        _operation: Uuid,
        _repo: &GitTarget,
        _reference: &str,
        _old: Option<&str>,
        _source: &str,
    ) -> Result<Value, GitFailure> {
        Err(GitFailure::protocol())
    }

    /// Observe/fetch an indeterminate publisher under its journal generation; never push.
    async fn reconcile_publication(
        &self,
        _operation: Uuid,
        _repo: &GitTarget,
        _original: Uuid,
        _generation: u64,
        _source: &str,
    ) -> Result<Value, GitFailure> {
        Err(GitFailure::protocol())
    }
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
        expected_source: Option<&str>,
    ) -> Result<RefPublication, GitFailure>;
    /// Deletes `reference` on the remote and in the central pool (`ref.delete`).
    /// The lease holds even when one side is already absent.
    async fn delete_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> Result<RefDeletion, GitFailure>;
    /// Deletes `reference` in the central pool only, under the lease
    /// `expected_head`, for when the remote branch moved past it by a refresh.
    async fn delete_central_ref(
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
fn sha1(hex: &str) -> Result<Value, GitFailure> {
    if hex.len() == 40 && object_id(hex) {
        Ok(json!({"algorithm":"sha1","hex":hex}))
    } else {
        Err(GitFailure::protocol())
    }
}

/// How long aq-git may take to answer a ref read of the central repository.
/// shortcut: a fixed budget for local reads; move it to Settings if a read ever reaches the remote.
const LOCAL_CALL: Duration = Duration::from_secs(30);
/// The time aq-git has to write its answer after the deadline it was given.
const REPLY_MARGIN: Duration = Duration::from_secs(5);

/// The aq-git socket (`SourceRetentionConfig::git_socket`).
#[derive(Debug, Clone)]
pub struct SocketGit {
    socket: PathBuf,
    /// The `deadline_ms` each call gives aq-git.
    deadline: Duration,
}
impl SocketGit {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
            deadline: LOCAL_CALL,
        }
    }

    /// Calls that reach the remote (sync, publish, branch deletion) get
    /// Settings `delivery-limits.git_transfer_timeout_ms`. aq-git caps a
    /// publish at 300 s of its own.
    pub fn waiting(self, deadline: Duration) -> Self {
        Self { deadline, ..self }
    }

    async fn call(
        &self,
        (operation, name): (Uuid, &str),
        repo: &GitTarget,
        params: Value,
    ) -> Result<Value, GitFailure> {
        let request = json!({
            "protocol_version": 1, "request_id": operation.to_string(),
            "operation_id": operation, "operation": name,
            "repository_id": repo.repository_id,
            "expected_binding_generation": repo.binding_generation,
            "params": params, "deadline_ms": self.deadline.as_millis() as u64,
        });
        let mut bytes = serde_json::to_vec(&request).map_err(|_| GitFailure::protocol())?;
        bytes.push(b'\n');
        let raw = protocol::exchange_within(&self.socket, &bytes, self.deadline + REPLY_MARGIN)
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
                    details: v["error"]["details"].clone(),
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
        let result = self
            .call((Uuid::new_v4(), "ref.show"), repo, params)
            .await?;
        let oid = &result["oid"];
        let typed = oid.is_null() || oid["algorithm"] == "sha1";
        if result["ref"] != reference || !typed {
            return Err(GitFailure::protocol());
        }
        match oid["hex"].as_str() {
            None if oid.is_null() => Ok(None),
            Some(hex) if object_id(hex) => Ok(Some(hex.to_owned())),
            _ => Err(GitFailure::protocol()),
        }
    }

    /// Spec 0017 §6.2: deletes an attempt's try ref (`refs/heads/aq-try/…`)
    /// from the central repository only (`ref.delete` with `remote: false`);
    /// an absent ref is already deleted. Never touches GitHub.
    pub async fn delete_try_ref(
        &self,
        repo: &GitTarget,
        reference: &str,
    ) -> Result<RefDeletion, GitFailure> {
        if !reference.starts_with(TRY_PREFIX) {
            return Err(GitFailure::protocol());
        }
        let params = json!({"ref": reference, "remote": false, "expected_old": null});
        let result = self
            .call((Uuid::new_v4(), "ref.delete"), repo, params)
            .await?;
        match result["outcome"].as_str() {
            Some("deleted") => Ok(RefDeletion::Deleted),
            Some("already_absent") => Ok(RefDeletion::AlreadyAbsent),
            _ => Err(GitFailure::protocol()),
        }
    }
}

fn sha1_hex(oid: &Value) -> Result<String, GitFailure> {
    oid["hex"]
        .as_str()
        .filter(|h| oid["algorithm"] == "sha1" && h.len() == 40 && object_id(h))
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
        details: Value::Null,
    })
}

impl RefPublication {
    fn from_reply(
        result: &Value,
        reference: &str,
        old: &Value,
        expected_source: Option<&str>,
    ) -> Result<Self, GitFailure> {
        let head = match result["outcome"].as_str() {
            Some("applied") if result["target"] == reference && &result["old"] == old => {
                sha1_hex(&result["new"])?
            }
            Some("already_present") => sha1_hex(&result["observed_target"])?,
            Some("rejected") if result["reason"] == "target_moved" => {
                let observed = sha1_hex(&result["observed_target"]).ok();
                match observed {
                    Some(head) if expected_source == Some(head.as_str()) => head,
                    _ => return Ok(RefPublication::Moved),
                }
            }
            Some("rejected") => {
                return Err(GitFailure {
                    code: "rejected".into(),
                    message: result["reason"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(256)
                        .collect(),
                    transient: false,
                    details: Value::Null,
                });
            }
            _ => return Err(GitFailure::protocol()),
        };
        if expected_source.is_some_and(|expected| head != expected) {
            return Err(GitFailure::protocol());
        }
        Ok(RefPublication::Published { head })
    }
}
#[async_trait::async_trait]
impl GitPort for SocketGit {
    async fn inspect_publication(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        old: Option<&str>,
        source: &str,
    ) -> Result<Value, GitFailure> {
        let result = self
            .call(
                (Uuid::new_v4(), "operation.show"),
                repo,
                json!({"target_operation_id":operation}),
            )
            .await?;
        let payload = &result["payload"];
        let params = &payload["params"];
        if result["version"] != 1
            || result["id"] != operation.to_string()
            || payload["operation"] != "publish"
            || payload["repository_id"] != repo.repository_id.to_string()
            || payload["expected_binding_generation"] != repo.binding_generation
            || params["source_ref"] != reference
            || params["target"] != reference
            || params.get("force").is_some()
            || params
                .get("update")
                .is_some_and(|v| old.is_none() || v != "replace")
            || params["expected_source"] != sha1(source)?
            || params["expected_old"]
                != sha1(old.unwrap_or("0000000000000000000000000000000000000000"))?
        {
            return Err(GitFailure::protocol());
        }
        let generation = result["generation"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or_else(GitFailure::protocol)?;
        let hash = crate::digest(payload).map_err(|_| GitFailure::protocol())?;
        if result["payload_hash"] != hash {
            return Err(GitFailure::protocol());
        }
        let status = match result["status"].as_str() {
            Some(s @ ("pending" | "complete" | "failed")) => s,
            _ => return Err(GitFailure::protocol()),
        };
        // Unknown historical/future phases stay unrecorded rather than exposing text.
        let phase = result["phase"].as_str().filter(|s| {
            matches!(
                *s,
                "queued" | "prepared" | "dispatched" | "indeterminate" | "terminal" | "complete"
            )
        });
        let reply = &result["result"];
        // A terminal result alongside an error is contradictory journal evidence.
        // Neither it nor a failed/pending record can attest success or rejection.
        let definitive = status == "complete" && result["error"].is_null();
        let outcome = match reply["outcome"].as_str() {
            Some(s @ ("applied" | "already_present" | "rejected" | "indeterminate")) => Some(s),
            _ => None,
        };
        let reason = match reply["reason"].as_str() {
            Some(
                s @ ("remote_rejected" | "target_moved" | "source_moved" | "non_fast_forward"),
            ) => Some(s),
            _ => None,
        };
        let published = definitive
            && match outcome {
                Some("applied") => {
                    reply["target"] == reference
                        && reply["old"] == params["expected_old"]
                        && reply["new"] == params["expected_source"]
                        && reply["acknowledged_at"]
                            .as_str()
                            .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
                }
                Some("already_present") => {
                    // The bound publisher also attests containment at a later head.
                    sha1_hex(&reply["observed_target"]).is_ok_and(|oid| oid != "0".repeat(40))
                        && reply["caused_by_this_operation"] == false
                        && reply["observed_at"]
                            .as_str()
                            .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok())
                }
                _ => false,
            };
        let capabilities = self
            .call((Uuid::new_v4(), "capabilities"), repo, json!({}))
            .await
            .ok();
        let classification = capabilities
            .as_ref()
            .and_then(|v| v["capability_tokens"].as_array())
            .is_some_and(|features| {
                features
                    .iter()
                    .any(|f| f == "publication_rejection_classification_v1")
            });
        let class = reply["rejection_class"]
            .as_str()
            .filter(|_| {
                classification
                    && definitive
                    && outcome == Some("rejected")
                    && reason == Some("remote_rejected")
            })
            .map(|s| match s {
                "non_fast_forward" | "stale_lease" | "hook_declined" => s,
                _ => "unknown",
            });
        let receipt = if published {
            if outcome == Some("applied") {
                json!({"outcome":"applied","target":reference,"old":params["expected_old"],"new":params["expected_source"],
                    "acknowledged_at":reply["acknowledged_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t|t.to_rfc3339())})
            } else {
                json!({"outcome":"already_present","observed_target":{"algorithm":"sha1","hex":sha1_hex(&reply["observed_target"])?},"caused_by_this_operation":false,
                    "observed_at":reply["observed_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t|t.to_rfc3339())})
            }
        } else {
            Value::Null
        };
        let indeterminate =
            !definitive || (matches!(outcome, Some("applied" | "already_present")) && !published);
        let outcome = if indeterminate {
            Some("indeterminate")
        } else {
            outcome
        };
        let reason = if indeterminate { None } else { reason };
        let error_code = match result["error"]["code"].as_str() {
            Some(
                c @ ("operation_indeterminate"
                | "remote_unreachable"
                | "target_moved"
                | "source_moved"
                | "object_unavailable"
                | "binding_generation_mismatch"),
            ) => Some(c),
            _ => None,
        };
        Ok(
            json!({"operation_id":operation,"generation":generation,"status":status,"phase":phase,
            "payload_hash":hash,"outcome":outcome,"reason":reason,"error_code":error_code,"published":published,"receipt":receipt,
            "classification_capability":if capabilities.is_none() {"unavailable"} else if classification {"supported"} else {"unsupported"},
            "rejection_class":class,"classification_record":if class.is_some() {"recorded"} else {"not_recorded"},
            "repository_id":repo.repository_id,"binding_generation":repo.binding_generation,
            "source_ref":reference,"expected_source":source,"target":reference,"expected_old":old}),
        )
    }
    async fn reconcile_publication(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        original: Uuid,
        generation: u64,
        source: &str,
    ) -> Result<Value, GitFailure> {
        if operation == original || operation.is_nil() || generation == 0 {
            return Err(GitFailure::protocol());
        }
        let reply = self
            .call(
                (operation, "operation.reconcile"),
                repo,
                json!({"target_operation_id":original,"expected_generation":generation}),
            )
            .await?;
        if reply["target_operation_id"] != original.to_string() {
            return Err(GitFailure::protocol());
        }
        let resolution = &reply["resolution"];
        let observed_at = resolution["observed_at"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.to_rfc3339());
        if reply["outcome"].is_null()
            && resolution["outcome"] == "already_present"
            && sha1_hex(&resolution["observed_target"]).is_ok_and(|oid| oid != "0".repeat(40))
            && resolution["caused_by_this_operation"] == false
            && observed_at.is_some()
        {
            return Ok(
                json!({"state":"published","head":source,"observed_head":resolution["observed_target"]["hex"],"caused_by_this_operation":false,"observed_at":observed_at}),
            );
        }
        // A moved/absent ref or a lost read remains uncertain, never a fabricated rejection.
        Ok(json!({"state":"held","reason":"operation_indeterminate"}))
    }

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
                details: Value::Null,
            });
        }
        let result = self
            .call(
                (operation, "repo.retain_ref"),
                repo,
                json!({"ref": reference}),
            )
            .await?;
        if result["ref"] != reference {
            return Err(GitFailure::protocol());
        }
        Ok(RetainedRef {
            head: sha1_hex(&result["head"])?,
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
        expected_source: Option<&str>,
    ) -> Result<RefPublication, GitFailure> {
        let reference = task_ref(reference)?;
        let old = sha1(expected_old.unwrap_or("0000000000000000000000000000000000000000"))?;
        let mut params = json!({"source_ref": reference, "target": reference, "expected_old": old});
        // A repaired task can rebuild its branch from main. The remote update
        // still requires the exact commit of the previous publication.
        if expected_old.is_some() {
            params["update"] = json!("replace");
        }
        if let Some(source) = expected_source {
            params["expected_source"] = sha1(source)?;
        }
        let mut released = false;
        let result = loop {
            match self
                .call((operation, "publish"), repo, params.clone())
                .await
            {
                Ok(result) => break result,
                Err(e) if e.code == "object_unavailable" => return Ok(RefPublication::NoRef),
                Err(e) if matches!(e.code.as_str(), "target_moved" | "source_moved") => {
                    return Ok(RefPublication::Moved);
                }
                // The same operation, never dispatched, goes on once the hold is released.
                Err(e)
                    if e.code == "operation_indeterminate"
                        && !released
                        && self
                            .release_destination(operation, repo, reference, &e.details)
                            .await =>
                {
                    released = true;
                }
                Err(e) => return Err(e),
            }
        };
        RefPublication::from_reply(&result, reference, &old, expected_source)
    }

    async fn delete_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> Result<RefDeletion, GitFailure> {
        self.delete((operation, repo), reference, expected_head, true)
            .await
    }
    async fn delete_central_ref(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        expected_head: Option<&str>,
    ) -> Result<RefDeletion, GitFailure> {
        self.delete((operation, repo), reference, expected_head, false)
            .await
    }
}
impl SocketGit {
    /// aq-git §10.3: an earlier dispatched publish whose outcome aq-git never
    /// learned holds `reference`. Observe it under a new operation ID
    /// (`operation.reconcile`, which settles it when its push landed). When the
    /// remote is still at that publication's own lease, its push had no effect
    /// and a new publication is permitted (`operation.resolve`). aq-git runs one
    /// mutation per repository at a time and never redispatches a publish, so no
    /// publisher of the held operation is running. `true` once the hold is gone.
    async fn release_destination(
        &self,
        operation: Uuid,
        repo: &GitTarget,
        reference: &str,
        held: &Value,
    ) -> bool {
        let Some(blocking) = held["blocking_operation_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .filter(|id| *id != operation && held["target"] == reference)
        else {
            return false;
        };
        let show = json!({"target_operation_id": blocking});
        let Ok(shown) = self
            .call((Uuid::new_v4(), "operation.show"), repo, show)
            .await
        else {
            return false;
        };
        let (Some(generation), Some(lease)) = (
            shown["generation"].as_u64(),
            shown["metadata"]["expected_old"]
                .as_str()
                .filter(|o| object_id(o)),
        ) else {
            return false;
        };
        let mut target =
            json!({"target_operation_id": blocking, "expected_generation": generation});
        let Ok(observed) = self
            .call(
                (Uuid::new_v4(), "operation.reconcile"),
                repo,
                target.clone(),
            )
            .await
        else {
            return false;
        };
        if observed["resolution"].is_object() {
            return true;
        }
        if observed["outcome"] != "indeterminate" || observed["observed_target"]["hex"] != lease {
            return false;
        }
        target["disposition"] = json!("permit_new_operation");
        target["reason"] = json!("the remote branch is still at the held publication's lease");
        target["old_publishers_stopped"] = json!(true);
        target["evidence"] = json!({"lease": lease, "reconciled": observed});
        self.call((Uuid::new_v4(), "operation.resolve"), repo, target)
            .await
            .is_ok_and(|resolved| resolved["resolution"].is_object())
    }

    async fn delete(
        &self,
        (operation, repo): (Uuid, &GitTarget),
        reference: &str,
        expected_head: Option<&str>,
        remote: bool,
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
                (operation, "ref.delete"),
                repo,
                json!({"ref": reference, "remote": remote, "expected_old": lease}),
            )
            .await?;
        match result["outcome"].as_str() {
            Some("deleted") => Ok(RefDeletion::Deleted),
            Some("already_absent") => Ok(RefDeletion::AlreadyAbsent),
            _ => Err(GitFailure::protocol()),
        }
    }
}
