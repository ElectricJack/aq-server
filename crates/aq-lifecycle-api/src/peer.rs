//! Pure bounded fake service. Its caller/source contexts are fixture inputs, not
//! runtime authentication, source verification or a lost-wakeup transaction.
use crate::*;
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorRole {
    SessionAgent,
    Operator,
    NodeRecovery,
}
#[derive(Clone, Debug)]
pub struct CallerContext {
    pub role: ActorRole,
    pub scope: Scope,
    pub binding: Option<Binding>,
    pub current_operation_id: Option<Uuid>,
    pub current_run_id: Option<Uuid>,
}
struct WaitRecord {
    request: WaitRegistration,
    registration: RegisteredWait,
    outcome: Option<WaitOutcome>,
}
pub struct ConformanceApi {
    context: CallerContext,
    advertised: Vec<Capability>,
    seen: BTreeMap<Uuid, (Digest, Response)>,
    waits: BTreeMap<Uuid, WaitRecord>,
    attachment: Option<ContinuationAttachment>,
    acknowledgment: Option<ArtifactRef>,
}
fn reference(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).expect("fixture ref"),
        version: Text::new("1").expect("fixture version"),
        digest: Digest::of(id.as_bytes()),
    }
}
impl ConformanceApi {
    pub fn new(context: CallerContext, advertised: Vec<Capability>) -> Result<Self, ContractError> {
        context.scope.validate()?;
        ensure(advertised.len() <= 16, "API advertisement bound")?;
        if let Some(binding) = &context.binding {
            binding.validate()?;
            ensure(
                binding.project_id == context.scope.project_id,
                "context project",
            )?;
        }
        ensure(
            context.role != ActorRole::SessionAgent || context.binding.is_some(),
            "agent context binding",
        )?;
        Ok(Self {
            context,
            advertised,
            seen: BTreeMap::new(),
            waits: BTreeMap::new(),
            attachment: None,
            acknowledgment: None,
        })
    }
    pub fn install_attachment(
        &mut self,
        value: ContinuationAttachment,
    ) -> Result<(), ContractError> {
        value.validate()?;
        ensure(
            self.context.binding.as_ref() == Some(&value.successor),
            "attachment caller binding",
        )?;
        ensure(self.attachment.is_none(), "one admitted continuation")?;
        self.attachment = Some(value);
        Ok(())
    }
    pub fn bootstrap_acknowledged(&self) -> bool {
        self.acknowledgment.is_some()
    }
    fn own(&self, binding: &Binding) -> Result<(), ContractError> {
        if self.context.role != ActorRole::SessionAgent {
            return Err(ContractError::Unsupported);
        }
        if self.context.binding.as_ref() != Some(binding) {
            return Err(ContractError::Stale);
        }
        Ok(())
    }
    pub fn receive(&mut self, request: &Request, now_ms: u64) -> Result<Response, ContractError> {
        request.validate()?;
        negotiate_api(&request.body.required_capability, &self.advertised)?;
        if self.context.role == ActorRole::NodeRecovery {
            return Err(ContractError::Unsupported);
        }
        if let Some((digest, response)) = self.seen.get(&request.body.operation_id) {
            return if digest == &request.body_digest {
                Ok(response.clone())
            } else {
                Err(ContractError::Conflict)
            };
        }
        ensure(self.seen.len() < 1024, "API receipt capacity")?;
        let body = match &request.body.command {
            Command::RegisterWait(value) => {
                self.register_wait(value, request.body.operation_id, now_ms)
            }
            Command::Checkpoint(value) => self.checkpoint(value),
            Command::ContinuationAck(value) => self.acknowledge(value),
            Command::Inspect(value) => self.inspect(value),
            Command::Operator(value) => self.operator(value),
        }?;
        let response = Response {
            api_version: API_VERSION,
            operation_id: request.body.operation_id,
            request_digest: request.body_digest.clone(),
            body,
        };
        response.validate_against(request)?;
        self.seen.insert(
            request.body.operation_id,
            (request.body_digest.clone(), response.clone()),
        );
        Ok(response)
    }
    fn register_wait(
        &mut self,
        value: &WaitRegistration,
        operation_id: Uuid,
        now_ms: u64,
    ) -> Result<ResponseBody, ContractError> {
        self.own(&value.binding)?;
        if value.scope != self.context.scope {
            return Err(ContractError::Stale);
        }
        value.reject_self(
            self.context.current_operation_id,
            self.context.current_run_id,
        )?;
        if let Some(old) = self.waits.get(&value.wait_id) {
            if old.request != *value {
                return Err(ContractError::Conflict);
            }
            return Ok(ResponseBody::WaitRegistered(Box::new(
                old.registration.clone(),
            )));
        }
        ensure(self.waits.len() < 256, "fake wait registry capacity")?;
        ensure(
            !self.waits.values().any(|old| {
                old.request.binding.attempt_id == value.binding.attempt_id && old.outcome.is_none()
            }),
            "one active suspension wait per attempt",
        )?;
        let registration = fixture_registration(value, operation_id, now_ms);
        registration.validate()?;
        self.waits.insert(
            value.wait_id,
            WaitRecord {
                request: value.clone(),
                registration: registration.clone(),
                outcome: None,
            },
        );
        Ok(ResponseBody::WaitRegistered(Box::new(registration)))
    }
    fn checkpoint(&self, value: &Checkpoint) -> Result<ResponseBody, ContractError> {
        self.own(&value.binding)?;
        let wait = self
            .waits
            .get(&value.continuation.wait.wait_id)
            .ok_or(ContractError::Stale)?;
        ensure(
            wait.registration.continuation == value.continuation,
            "checkpoint continuation binding",
        )?;
        Ok(ResponseBody::CheckpointRecorded {
            continuation: value.continuation.clone(),
            claim_digest: canonical_digest(&value.claim)?,
        })
    }
    fn acknowledge(&mut self, value: &AckRequest) -> Result<ResponseBody, ContractError> {
        self.own(&value.successor)?;
        let attachment = self.attachment.as_ref().ok_or(ContractError::Stale)?;
        value.validate_attachment(attachment)?;
        ensure(
            self.acknowledgment.is_none(),
            "continuation ack already recorded",
        )?;
        let receipt = reference("fake-authenticated-api-receipt-not-runtime-proof");
        self.acknowledgment = Some(receipt.clone());
        Ok(ResponseBody::ContinuationAcknowledged {
            admission_id: value.admission_id,
            continuation: value.continuation.clone(),
            acknowledgment_receipt: receipt,
        })
    }
    fn authorize_inspection(&self, target: &InspectTarget) -> Result<(), ContractError> {
        match target {
            InspectTarget::Session { binding } => self.own(binding)?,
            InspectTarget::Wait { wait } => {
                let row = self.waits.get(&wait.wait_id).ok_or(ContractError::Stale)?;
                self.own(&row.registration.binding)?;
                ensure(row.registration.wait == *wait, "inspection wait revision")?;
            }
            InspectTarget::Notification { .. } => {
                return Err(ContractError::Unsupported);
            }
        }
        Ok(())
    }
    fn inspect(&self, value: &InspectRequest) -> Result<ResponseBody, ContractError> {
        if value.scope != self.context.scope {
            return Err(ContractError::Stale);
        }
        if self.context.role == ActorRole::SessionAgent {
            self.authorize_inspection(&value.target)?;
        }
        let items = if let InspectTarget::Wait { wait } = &value.target {
            let row = self.waits.get(&wait.wait_id).ok_or(ContractError::Stale)?;
            vec![InspectionItem::Wait(Box::new(WaitView {
                registration: row.registration.clone(),
                outcome: row.outcome.clone(),
                continuation: ContinuationState::PendingPolicy,
                parking_report: None,
            }))]
        } else {
            Vec::new()
        };
        Ok(ResponseBody::Inspection(InspectionPage {
            scope: value.scope.clone(),
            items,
            next_cursor: None,
            complete_for_authorized_scope: false,
            reasons: vec![Text::new("fake peer has no runtime projection/source scan").unwrap()],
        }))
    }
    fn operator(&self, value: &OperatorRequest) -> Result<ResponseBody, ContractError> {
        if self.context.role != ActorRole::Operator {
            return Err(ContractError::Unsupported);
        }
        if value.scope != self.context.scope {
            return Err(ContractError::Stale);
        }
        Ok(ResponseBody::OperatorRequested {
            receipt: reference("fake-operator-request-not-executed"),
        })
    }
    /// Supply already verified owner observations in a fake test. There is no wire
    /// command that lets an agent submit these verdicts or a transport ack as truth.
    pub fn resolve_from_fixture(
        &mut self,
        wait_id: Uuid,
        verdict: FixtureVerdict,
    ) -> Result<WaitOutcome, ContractError> {
        let FixtureVerdict {
            members: outcomes,
            outcome_id,
            recorded_at_ms: now_ms,
        } = verdict;
        let row = self.waits.get_mut(&wait_id).ok_or(ContractError::Stale)?;
        ensure(row.outcome.is_none(), "immutable outcome")?;
        validate_observations(&row.request.predicate, &outcomes)?;
        let satisfied: Vec<Uuid> = outcomes
            .iter()
            .filter(|v| v.state == MemberState::Satisfied)
            .map(|v| v.member_id)
            .collect();
        let any = matches!(row.request.predicate, Predicate::Any { .. });
        let kind = fixture_outcome_kind(any, satisfied.len(), &outcomes)?;
        let outcome = WaitOutcome {
            outcome: OutcomeRef {
                outcome_id,
                wait: row.registration.wait.clone(),
                revision: 1,
                digest: Digest::of(
                    &serde_json::to_vec(&outcomes)
                        .map_err(|e| ContractError::Json(e.to_string()))?,
                ),
            },
            kind,
            selected_member_id: if any {
                satisfied.first().copied()
            } else {
                None
            },
            members: outcomes,
            recorded_at_ms: now_ms,
            unresolved_predicate: None,
        };
        outcome.validate_for(&row.request.predicate)?;
        row.outcome = Some(outcome.clone());
        Ok(outcome)
    }
}

/// Observations and recording metadata supplied by the conformance test owner.
pub struct FixtureVerdict {
    pub members: Vec<MemberOutcome>,
    pub outcome_id: Uuid,
    pub recorded_at_ms: u64,
}

fn fixture_registration(
    value: &WaitRegistration,
    operation_id: Uuid,
    now_ms: u64,
) -> RegisteredWait {
    RegisteredWait {
        scope: value.scope.clone(),
        binding: value.binding.clone(),
        wait: value.wait_ref(),
        predicate: value.predicate.clone(),
        continuation: ContinuationRef {
            continuation_id: operation_id,
            generation: value.continuation_generation,
            wait: value.wait_ref(),
        },
        original_created_at_ms: now_ms,
        central_hold_receipt: reference("fake-central-hold-only"),
        boundaries: value
            .predicate
            .members()
            .iter()
            .map(|member| SourceBoundary {
                member_id: member.member_id,
                owner: member.condition.owner(),
                condition_digest: canonical_digest(&member.condition).unwrap(),
                committed_revision: Text::new("fixture-committed-revision").unwrap(),
                committed_replay_cursor: Text::new("fixture-committed-replay-boundary").unwrap(),
                subscription_receipt: reference("fake-subscription-not-runtime-proof"),
                snapshot_digest: Digest::of(b"fixture-source-snapshot"),
            })
            .collect(),
        cycle_assessment: CycleAssessment::ExternalUnknown {
            reason: Text::new("runtime graph/source cycle verification remains required").unwrap(),
        },
        execution: WaitExecutionState::PolicyPending,
    }
}

impl AckRequest {
    fn validate_attachment(
        &self,
        attachment: &ContinuationAttachment,
    ) -> Result<(), ContractError> {
        let same_admission = self.admission_id == attachment.admission_id
            && self.continuation == attachment.continuation;
        if !same_admission {
            return Err(ContractError::Stale);
        }
        if self.attachment_digest != canonical_digest(attachment)?
            || self.outcome != attachment.outcome
        {
            return Err(ContractError::Stale);
        }
        Ok(())
    }
}

fn validate_observations(
    predicate: &Predicate,
    outcomes: &[MemberOutcome],
) -> Result<(), ContractError> {
    let members = predicate.members();
    ensure(
        outcomes.len() == members.len(),
        "all source members reobserved",
    )?;
    for (member, result) in members.iter().zip(outcomes) {
        result.validate()?;
        ensure(result.member_id == member.member_id, "member outcome order")?;
        if let Some(evidence) = &result.evidence {
            ensure(
                evidence.owner == member.condition.owner()
                    && evidence.condition_digest == canonical_digest(&member.condition)?,
                "source owner/condition mismatch",
            )?;
        }
    }
    Ok(())
}

fn fixture_outcome_kind(
    any: bool,
    satisfied_count: usize,
    outcomes: &[MemberOutcome],
) -> Result<WaitOutcomeKind, ContractError> {
    let complete = if any {
        satisfied_count > 0
    } else {
        satisfied_count == outcomes.len()
    };
    let kind = if complete {
        WaitOutcomeKind::Satisfied
    } else if any && outcomes.iter().any(|v| v.state == MemberState::Pending) {
        return Err(ContractError::Invalid("wait remains pending"));
    } else if outcomes
        .iter()
        .any(|v| v.state == MemberState::SourceFailed)
    {
        WaitOutcomeKind::SourceFailed
    } else if outcomes
        .iter()
        .any(|v| v.state == MemberState::SourceUnavailable)
    {
        WaitOutcomeKind::SourceUnavailable
    } else {
        return Err(ContractError::Invalid("wait remains pending"));
    };
    Ok(kind)
}
