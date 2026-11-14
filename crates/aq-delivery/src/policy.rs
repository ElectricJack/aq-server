//! Per-project publication policy and the single delivery entry point. The
//! policy is stored on the revisioned project binding; `deliver` maps aq-git
//! publish receipts onto typed outcomes so callers never parse wire errors.
use super::*;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMode {
    PullRequest,
    FastForward,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WaitForChecks {
    pub timeout_secs: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PublicationPolicy {
    pub mode: PublicationMode,
    /// Full `refs/heads/` branch; must be in the aq-git binding's allowed targets.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_for_checks: Option<WaitForChecks>,
}
impl PublicationPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.target.len() > 1024 {
            return Err(DeliveryError::InvalidInput("publication target too long"));
        }
        branch(&self.target)?;
        match (&self.mode, &self.wait_for_checks) {
            (PublicationMode::FastForward, Some(_)) => Err(DeliveryError::InvalidInput(
                "wait_for_checks requires pull_request mode",
            )),
            (_, Some(w)) if w.timeout_secs == 0 || w.timeout_secs > 86_400 => Err(
                DeliveryError::InvalidInput("wait_for_checks timeout out of range"),
            ),
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DeliverRequest {
    pub context: OperationContext,
    pub candidate_id: Uuid,
    pub expected_old: ObjectId,
}
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    TargetMoved,
    Rejected,
    Conflicted,
    TargetNotAllowed,
    PullRequestUnsupported,
}
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PublicationRefusal {
    pub operation_id: Uuid,
    pub reason: RefusalReason,
    pub detail: String,
    pub target: String,
    pub expected_old: Option<ObjectId>,
    pub observed_target: Option<ObjectId>,
    pub observed_at: Option<String>,
}
/// aq-git refused to run under a previous owner incarnation without cleanup
/// evidence. The destination stays held; an operator clears the fence with
/// [`ClearFenceRequest`] through the direct CLI, never automatically.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryBlocked {
    pub operation_id: Uuid,
    pub message: String,
    pub details: Value,
}
#[derive(Debug)]
pub enum DeliveryOutcome {
    Published {
        operation_id: Uuid,
        old: ObjectId,
        new: ObjectId,
        acknowledged_at: String,
    },
    /// Observed presence, never a claim that this operation caused it.
    ObservedPresent {
        operation_id: Uuid,
        observed_target: ObjectId,
        observed_at: String,
    },
    Refused(PublicationRefusal),
    /// Dispatched without acknowledgement: reconcile under a new operation ID.
    Ambiguous {
        operation_id: Uuid,
    },
    /// Another publication to this destination is unresolved.
    DestinationBlocked,
    RecoveryBlocked(RecoveryBlocked),
    Failed {
        operation_id: Uuid,
        error: GitError,
    },
}
/// aq-git `recovery.clear-fence`. Accepted only over the direct CLI (a socket
/// request is refused), so this only builds the envelope for an operator.
#[derive(Debug, Clone, Serialize)]
pub struct ClearFenceRequest {
    pub expected_incarnation: String,
    pub reason: String,
    pub evidence: Value,
    pub old_environment_stopped: bool,
}
impl ClearFenceRequest {
    pub fn cli_envelope(&self, operation_id: Uuid) -> Result<Value> {
        if operation_id.is_nil()
            || self.expected_incarnation.trim().is_empty()
            || self.reason.trim().is_empty()
            || self.reason.len() > 4096
            || !reconcile::evidence(&self.evidence)
            || !self.old_environment_stopped
        {
            return Err(DeliveryError::InvalidInput(
                "clear-fence requires incarnation, reason, evidence and old_environment_stopped=true",
            ));
        }
        Ok(
            json!({"protocol_version":1,"request_id":operation_id.to_string(),"operation_id":operation_id,
            "operation":"recovery.clear-fence","params":{"expected_incarnation":self.expected_incarnation,
            "reason":self.reason,"evidence":self.evidence,"old_environment_stopped":true}}),
        )
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct PullRequestSpec {
    pub repository_id: Uuid,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Pending,
    Passed,
    Failed,
}
#[derive(Debug, thiserror::Error)]
#[error("github: {0}")]
pub struct GitHubError(pub String);
/// Pull-request publication seam. aq-git 0.2.0 cannot create the head branch,
/// so `deliver` refuses pull_request policies before calling this.
#[async_trait::async_trait]
pub trait GitHubAdapter: Send + Sync {
    async fn open_pull_request(
        &self,
        spec: &PullRequestSpec,
    ) -> std::result::Result<PullRequest, GitHubError>;
    async fn checks(&self, pr: &PullRequest) -> std::result::Result<ChecksState, GitHubError>;
}
impl DeliveryService {
    pub async fn deliver(
        &self,
        policy: &PublicationPolicy,
        r: DeliverRequest,
        _github: &dyn GitHubAdapter,
    ) -> Result<DeliveryOutcome> {
        policy.validate()?;
        let operation_id = r.context.operation_id;
        let refuse = |reason, detail: &str, observed_target, observed_at| {
            DeliveryOutcome::Refused(PublicationRefusal {
                operation_id,
                reason,
                detail: detail.into(),
                target: policy.target.clone(),
                expected_old: Some(r.expected_old.clone()),
                observed_target,
                observed_at,
            })
        };
        if policy.mode == PublicationMode::PullRequest {
            // Never degrade to fast-forward.
            return Ok(refuse(
                RefusalReason::PullRequestUnsupported,
                "aq-git 0.2.0 cannot create pull-request branches",
                None,
                None,
            ));
        }
        let receipt = match self
            .publish(PublishRequest {
                context: r.context.clone(),
                candidate_id: r.candidate_id,
                target: policy.target.clone(),
                expected_old: r.expected_old.clone(),
            })
            .await
        {
            Ok(receipt) => receipt,
            Err(DeliveryError::Indeterminate) => {
                return Ok(DeliveryOutcome::Ambiguous { operation_id });
            }
            Err(DeliveryError::DestinationBlocked) => {
                return Ok(DeliveryOutcome::DestinationBlocked);
            }
            Err(DeliveryError::InvalidInput(CANDIDATE_NOT_READY)) => {
                return Ok(refuse(
                    RefusalReason::Conflicted,
                    CANDIDATE_NOT_READY,
                    None,
                    None,
                ));
            }
            Err(e) => return Err(e),
        };
        Ok(match receipt.outcome {
            Ok(PublishResult::Applied {
                old,
                new,
                acknowledged_at,
                ..
            }) => DeliveryOutcome::Published {
                operation_id,
                old,
                new,
                acknowledged_at,
            },
            Ok(PublishResult::AlreadyPresent {
                observed_target,
                observed_at,
                ..
            }) => DeliveryOutcome::ObservedPresent {
                operation_id,
                observed_target,
                observed_at,
            },
            Ok(PublishResult::Rejected {
                reason,
                observed_target,
                observed_at,
            }) => {
                let kind = if reason == "target_moved" {
                    RefusalReason::TargetMoved
                } else {
                    RefusalReason::Rejected
                };
                refuse(kind, &reason, observed_target, observed_at)
            }
            Err(e) if e.code == "recovery_blocked" => {
                DeliveryOutcome::RecoveryBlocked(RecoveryBlocked {
                    operation_id,
                    message: e.message,
                    details: e.details,
                })
            }
            Err(e) if e.uncertain() => DeliveryOutcome::Ambiguous { operation_id },
            Err(e) => match e.code.as_str() {
                "target_moved" => refuse(RefusalReason::TargetMoved, &e.message, None, None),
                "conflicted" => refuse(RefusalReason::Conflicted, &e.message, None, None),
                "unsupported_feature" => {
                    refuse(RefusalReason::TargetNotAllowed, &e.message, None, None)
                }
                _ => DeliveryOutcome::Failed {
                    operation_id,
                    error: e,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_validation_and_round_trip() {
        let p: PublicationPolicy =
            serde_json::from_value(json!({"mode":"fast_forward","target":"refs/heads/main"}))
                .unwrap();
        p.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"mode":"fast_forward","target":"refs/heads/main"})
        );
        let checks = |mode| PublicationPolicy {
            mode,
            target: "refs/heads/main".into(),
            wait_for_checks: Some(WaitForChecks { timeout_secs: 600 }),
        };
        assert!(checks(PublicationMode::FastForward).validate().is_err());
        checks(PublicationMode::PullRequest).validate().unwrap();
        let mut zero = checks(PublicationMode::PullRequest);
        zero.wait_for_checks = Some(WaitForChecks { timeout_secs: 0 });
        assert!(zero.validate().is_err());
        for bad in ["main", "refs/tags/v1", "refs/heads/a..b", "refs/heads/"] {
            let mut p = p.clone();
            p.target = bad.into();
            assert!(p.validate().is_err(), "{bad}");
        }
        assert!(
            serde_json::from_value::<PublicationPolicy>(
                json!({"mode":"fast_forward","target":"refs/heads/main","extra":1})
            )
            .is_err()
        );
    }

    #[test]
    fn clear_fence_envelope_is_direct_cli_only() {
        let id = Uuid::new_v4();
        let mut r = ClearFenceRequest {
            expected_incarnation: "inc-1".into(),
            reason: "old host destroyed".into(),
            evidence: json!({"ticket":"OPS-1"}),
            old_environment_stopped: true,
        };
        let v = r.cli_envelope(id).unwrap();
        assert_eq!(v["operation"], "recovery.clear-fence");
        assert_eq!(v["operation_id"], json!(id));
        assert!(v.get("repository_id").is_none());
        assert!(v.get("expected_binding_generation").is_none());
        assert_eq!(
            v["params"],
            json!({"expected_incarnation":"inc-1","reason":"old host destroyed","evidence":{"ticket":"OPS-1"},"old_environment_stopped":true})
        );
        r.old_environment_stopped = false;
        assert!(r.cli_envelope(id).is_err());
        r.old_environment_stopped = true;
        r.evidence = json!({});
        assert!(r.cli_envelope(id).is_err());
    }
}
