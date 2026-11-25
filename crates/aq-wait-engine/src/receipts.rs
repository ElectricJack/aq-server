//! Wire receipts and views derived from wait engine state.
use super::*;
mod observation;
pub use observation::Observed;
pub fn text(s: impl Into<String>) -> wire::Text {
    wire::Text::new(s).expect("static wait text")
}
pub fn artifact(id: impl Into<String>, digest: wire::Digest) -> wire::ArtifactRef {
    wire::ArtifactRef {
        id: text(id),
        version: text("1"),
        digest,
    }
}

/// The only policy this server resolves. Any other reference is refused.
pub fn default_policy() -> ResolvedPolicy {
    let body = PolicyBody {
        maximum_age_ms: MAX_AGE_MS,
        check_interval_ms: 60_000,
        retry_base_ms: 1_000,
        retry_cap_ms: 60_000,
        max_retries: 64,
        max_work_attempts: 65_536,
    };
    ResolvedPolicy {
        reference: artifact(
            "aq-server-wait-policy-default",
            wire::canonical_digest(&body).expect("policy digest"),
        ),
        body,
    }
}

pub fn respond(request: &wire::Request, body: wire::ResponseBody) -> wire::Response {
    wire::Response {
        api_version: wire::API_VERSION,
        operation_id: request.body.operation_id,
        request_digest: request.body_digest.clone(),
        body,
    }
}
pub fn refused(request: &wire::Request, code: wire::RefusalCode) -> wire::Response {
    respond(
        request,
        wire::ResponseBody::Refused {
            code,
            effect: wire::Effect::NotApplied,
        },
    )
}
pub fn contract_refusal(error: &wire::ContractError) -> wire::RefusalCode {
    match error {
        wire::ContractError::Invalid("self_wait") => wire::RefusalCode::SelfWait,
        wire::ContractError::Unsupported => wire::RefusalCode::UnsupportedCapability,
        wire::ContractError::Stale => wire::RefusalCode::StaleGeneration,
        _ => wire::RefusalCode::InvalidRequest,
    }
}
pub fn acknowledgment(
    request: &wire::Request,
    ack: &wire::AckRequest,
    intent_id: Uuid,
) -> wire::Response {
    let receipt = artifact(
        format!("agent-wait-ack-{}", ack.admission_id),
        wire::Digest::of(
            format!(
                "agent-wait-ack:{}:{}:{}",
                ack.admission_id,
                intent_id,
                ack.outcome.digest.as_str()
            )
            .as_bytes(),
        ),
    );
    respond(
        request,
        wire::ResponseBody::ContinuationAcknowledged {
            admission_id: ack.admission_id,
            continuation: ack.continuation.clone(),
            acknowledgment_receipt: receipt,
        },
    )
}
pub fn task_dependencies(registration: &wire::WaitRegistration) -> Vec<DependencyKey> {
    let project = registration.binding.project_id;
    registration
        .predicate
        .members()
        .iter()
        .filter_map(|member| match member.condition {
            wire::Condition::Task { task_id, .. } => Some(DependencyKey::Task {
                project,
                task: task_id,
            }),
            wire::Condition::Dependency {
                depends_on_task_id, ..
            } => Some(DependencyKey::Task {
                project,
                task: depends_on_task_id,
            }),
            _ => None,
        })
        .collect()
}
impl Snapshot {
    pub fn registered_wait(&self) -> Result<wire::RegisteredWait> {
        let registration = self.registration();
        let wait = registration.wait_ref();
        let hold = wire::Digest::of(
            format!(
                "agent-wait-hold:{}:{}:{}",
                wait.wait_id,
                registration.wait_generation,
                self.original_created_at_ms()
            )
            .as_bytes(),
        );
        let value = wire::RegisteredWait {
            scope: registration.scope.clone(),
            binding: registration.binding.clone(),
            wait: wait.clone(),
            predicate: registration.predicate.clone(),
            continuation: wire::ContinuationRef {
                continuation_id: self.ids().continuation_id,
                generation: registration.continuation_generation,
                wait,
            },
            original_created_at_ms: self.original_created_at_ms(),
            central_hold_receipt: artifact(
                format!("agent-wait-hold-{}", registration.wait_id),
                hold,
            ),
            boundaries: self.boundaries().ok_or(Error::Invalid)?,
            cycle_assessment: self.cycle().clone(),
            execution: wire::WaitExecutionState::PolicyPending,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn inspection(
        &self,
        registered: &wire::RegisteredWait,
        intent_state: Option<&str>,
        ack_receipt: Option<&wire::ArtifactRef>,
    ) -> Result<wire::WaitView> {
        let outcome = self.outcome().cloned();
        let cancelled = matches!(intent_state, Some("retired"))
            || outcome
                .as_ref()
                .is_some_and(|o| o.kind == wire::WaitOutcomeKind::Cancelled);
        let continuation = if let Some(receipt) = ack_receipt {
            wire::ContinuationState::BootstrapAcknowledged {
                receipt: receipt.clone(),
            }
        } else if cancelled {
            wire::ContinuationState::Cancelled
        } else if let Some(reason) = self.hold_reason() {
            wire::ContinuationState::Held {
                reason: reason.clone(),
            }
        } else {
            wire::ContinuationState::PendingPolicy
        };
        let value = wire::WaitView {
            registration: registered.clone(),
            outcome,
            continuation,
            parking_report: None,
        };
        value.validate()?;
        Ok(value)
    }
}
