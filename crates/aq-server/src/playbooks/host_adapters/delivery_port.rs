//! Production D7 [`Integration`] port over the D1 [`DeliveryService`].
//!
//! The adapter only maps calls: candidate creation, publication policy and
//! reconciliation stay inside D1 unchanged. The binding comes from the
//! project's revisioned repository binding (D2) plus the configured evidence
//! policy; review evidence comes from the review-decision service.
use super::*;
use crate::{
    delivery::{
        CandidateRequest, CandidateState, ChecksState, CommitMetadata, DeliverRequest,
        DeliveryError, DeliveryOutcome, DeliveryService, GitHubAdapter, GitHubError,
        ObservationRequest, OperationContext, PullRequest, PullRequestSpec, ReconcileRequest,
        Resolution, RetryAdvice,
    },
    error::AppError,
    playbooks::config::IntegrationSettings,
};

/// aq-git 0.2.0 cannot open pull requests and D1 refuses pull-request mode
/// before any GitHub call, so production delivery binds no GitHub client.
struct NoGitHub;
#[async_trait::async_trait]
impl GitHubAdapter for NoGitHub {
    async fn open_pull_request(
        &self,
        _: &PullRequestSpec,
    ) -> std::result::Result<PullRequest, GitHubError> {
        Err(GitHubError("not configured".into()))
    }
    async fn checks(&self, _: &PullRequest) -> std::result::Result<ChecksState, GitHubError> {
        Err(GitHubError("not configured".into()))
    }
}

pub struct DeliveryIntegration {
    state: crate::AppState,
    delivery: DeliveryService,
    settings: IntegrationSettings,
    reviews: Arc<crate::reviews::Service>,
    workspace: Uuid,
    project: Uuid,
    /// Namespace owner: the actor review decisions are inspected as.
    owner: Uuid,
}
impl DeliveryIntegration {
    pub(crate) fn new(
        state: crate::AppState,
        delivery: DeliveryService,
        settings: IntegrationSettings,
        reviews: Arc<crate::reviews::Service>,
        namespace: &Namespace,
    ) -> Self {
        Self {
            state,
            delivery,
            settings,
            reviews,
            workspace: namespace.workspace_id,
            project: namespace.project_id,
            owner: namespace.owner_user_id,
        }
    }
    fn own(&self, workspace: Uuid, project: Uuid) -> bool {
        workspace == self.workspace && project == self.project
    }
}
fn context(operation: Uuid, repository: Uuid, generation: u64) -> OperationContext {
    OperationContext {
        operation_id: operation,
        repository_id: repository,
        binding_generation: generation,
    }
}

#[async_trait::async_trait]
impl Integration for DeliveryIntegration {
    async fn binding(&self, workspace: Uuid, project: Uuid) -> Result<Option<IntegrationBinding>> {
        if !self.own(workspace, project) {
            return Ok(None);
        }
        let record = match crate::project_bindings::current(&self.state, project).await {
            Ok(r) => r,
            Err(AppError::NotFound(_)) => return Ok(None),
            Err(_) => return Err(Error::Unknown),
        };
        let b = record.binding;
        let Some(publication) = b.publication else {
            return Ok(None);
        };
        if !b.enabled || b.project_id != project || b.graph_workspace_id != workspace {
            return Ok(None);
        }
        Ok(Some(IntegrationBinding {
            repository_id: b.repository_id,
            binding_generation: b.repository_binding_generation,
            publication,
            evidence: self.settings.evidence.clone(),
        }))
    }

    async fn prepare(&self, call: &PrepareCall) -> Result<PrepareReply> {
        let request = CandidateRequest {
            context: context(call.operation, call.repository_id, call.binding_generation),
            base: call.base.clone(),
            source_receipts: call.sources.clone(),
            // Frozen with the intent: deterministic per operation.
            commit: CommitMetadata {
                name: self.settings.commit_name.clone(),
                email: self.settings.commit_email.clone(),
                message: format!("AQ candidate {}", call.operation),
            },
        };
        let receipt = match self.delivery.create_candidate(request).await {
            Ok(r) => r,
            Err(DeliveryError::InvalidInput(_)) => {
                return Ok(PrepareReply::Refused {
                    reason: "invalid_request".into(),
                });
            }
            Err(DeliveryError::IdempotencyConflict) => {
                return Ok(PrepareReply::Refused {
                    reason: "idempotency_conflict".into(),
                });
            }
            // Transport/protocol/storage: the intent stays pending and the
            // same operation is re-sent on re-dispatch.
            Err(_) => return Err(Error::Unknown),
        };
        Ok(match receipt.outcome {
            Ok(c) => match (c.state, c.candidate) {
                (CandidateState::Ready, Some(candidate)) => PrepareReply::Prepared { candidate },
                (CandidateState::Ready, None) => return Err(Error::Unknown),
                (CandidateState::Conflicted, _) => PrepareReply::Conflict {
                    step: c.conflict_step.map(|s| s as u64),
                    paths: c
                        .conflicting_paths
                        .iter()
                        .map(|p| {
                            hex::decode(&p.value)
                                .map(|b| String::from_utf8_lossy(&b).into_owned())
                                .unwrap_or_default()
                        })
                        .collect(),
                    truncated: c.conflict_paths_truncated,
                    count: c.conflict_path_count as u64,
                },
            },
            Err(e) if e.retry == RetryAdvice::SafeSameOperation => return Err(Error::Unknown),
            Err(e) => PrepareReply::Refused { reason: e.code },
        })
    }

    async fn publish(&self, call: &PublishCall) -> Result<PublishReply> {
        let request = DeliverRequest {
            context: context(call.operation, call.repository_id, call.binding_generation),
            candidate_id: call.candidate_operation,
            expected_old: call.expected_old.clone(),
        };
        let outcome = match self
            .delivery
            .deliver(&call.publication, request, &NoGitHub)
            .await
        {
            Ok(o) => o,
            Err(DeliveryError::InvalidInput(_)) => {
                return Ok(PublishReply::Refused {
                    reason: "invalid_request".into(),
                });
            }
            Err(DeliveryError::IdempotencyConflict) => {
                return Ok(PublishReply::Refused {
                    reason: "idempotency_conflict".into(),
                });
            }
            // The caller treats an error as ambiguous: reconcile, never re-publish.
            Err(_) => return Err(Error::Unknown),
        };
        Ok(match outcome {
            DeliveryOutcome::Published { old, new, .. } => PublishReply::Published { old, new },
            DeliveryOutcome::ObservedPresent {
                observed_target, ..
            } => PublishReply::ObservedPresent {
                observed: observed_target,
            },
            DeliveryOutcome::Refused(r) => PublishReply::Refused {
                reason: serde_json::to_value(r.reason)?
                    .as_str()
                    .unwrap_or("publish_failed")
                    .into(),
            },
            DeliveryOutcome::Ambiguous { .. } => PublishReply::Ambiguous,
            DeliveryOutcome::DestinationBlocked => PublishReply::Refused {
                reason: "destination_blocked".into(),
            },
            DeliveryOutcome::RecoveryBlocked(_) => PublishReply::Refused {
                reason: "recovery_blocked".into(),
            },
            DeliveryOutcome::Failed { error, .. } => PublishReply::Refused { reason: error.code },
        })
    }

    async fn reconcile(&self, call: &ReconcileCall) -> Result<ReconcileReply> {
        // The target's generation is read from aq-git, never guessed. One
        // observation per reconcile operation (deterministic id).
        let show = self
            .delivery
            .operation_show(ObservationRequest {
                context: context(
                    Uuid::new_v5(&call.operation, b"aq.integration.operation-show"),
                    call.repository_id,
                    call.binding_generation,
                ),
                target_id: call.target_operation,
            })
            .await;
        let Ok(Ok(target)) = show.map(|r| r.outcome) else {
            return Ok(ReconcileReply::Unresolved);
        };
        let reconciled = self
            .delivery
            .reconcile(ReconcileRequest {
                context: context(call.operation, call.repository_id, call.binding_generation),
                target_operation_id: call.target_operation,
                expected_generation: target.generation,
            })
            .await;
        Ok(match reconciled.map(|r| r.outcome) {
            Ok(Ok(r)) => match r.resolution {
                Some(Resolution::AlreadyPresent {
                    observed_target, ..
                }) => ReconcileReply::Present {
                    observed: observed_target,
                },
                Some(Resolution::Resolved { disposition, .. }) => {
                    ReconcileReply::Resolved { disposition }
                }
                None => ReconcileReply::Unresolved,
            },
            _ => ReconcileReply::Unresolved,
        })
    }

    async fn review(
        &self,
        workspace: Uuid,
        project: Uuid,
        decision: Uuid,
    ) -> Result<Option<ReviewEvidence>> {
        if !self.own(workspace, project) {
            return Ok(None);
        }
        let view = match self.reviews.inspect(self.owner, decision).await {
            Ok(v) => v,
            Err(AppError::Forbidden(_) | AppError::NotFound(_)) => return Ok(None),
            Err(_) => return Err(Error::Unknown),
        };
        let finality = &view["receipt"]["finality"];
        let answered = finality["kind"] == "human_answer";
        let approved = answered && finality["choice"] == "approve";
        let issuer_authorized = answered
            && view["request"]["eligible_owners"]
                .as_array()
                .is_some_and(|o| o.contains(&finality["owner_user_id"]));
        // Review subjects are artifact refs ({id, version}); they only match a
        // candidate whose id and numeric revision they name.
        let subject = &view["request"]["subject"];
        let revision = subject["version"]
            .as_str()
            .and_then(|v| v.parse::<i64>().ok())
            .map_or(subject["version"].clone(), Value::from);
        Ok(Some(ReviewEvidence {
            subject: json!({"id": subject["id"], "revision": revision}),
            approved,
            issuer_authorized,
        }))
    }
}
