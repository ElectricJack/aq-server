use crate::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskRequirement {
    Successful,
    Settled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationOwner {
    GraphDelivery,
    SourceRetention,
    Assessment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    ProviderAvailability,
    WorkspaceCapacity,
    WorkerSlots,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SenderConstraint {
    AnyAuthorized,
    Task { task_id: Uuid },
    Session { binding: Binding },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Condition {
    Task {
        task_id: Uuid,
        revision: Text,
        requirement: TaskRequirement,
    },
    Dependency {
        edge_id: Uuid,
        edge_revision: Text,
        from_task_id: Uuid,
        depends_on_task_id: Uuid,
        requirement: TaskRequirement,
    },
    Message {
        thread_id: Uuid,
        exclusive_cursor: u64,
        sender: SenderConstraint,
    },
    HumanDecision {
        decision_id: Uuid,
        subject_id: Uuid,
        subject_revision: Text,
        question_digest: Digest,
    },
    Timer {
        due_at_ms: u64,
    },
    Operation {
        owner: OperationOwner,
        operation_id: Uuid,
        revision: Text,
    },
    PlaybookRun {
        run_id: Uuid,
        revision: Text,
    },
    WaitOutcome {
        wait: WaitRef,
    },
    Resource {
        resource: ResourceKind,
        selection_snapshot: ArtifactRef,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceOwner {
    TaskGraph,
    Mailbox,
    HumanDecision,
    Clock,
    Operation,
    Playbook,
    WaitRegistry,
    Scheduler,
}
impl Condition {
    pub fn owner(&self) -> SourceOwner {
        match self {
            Self::Task { .. } | Self::Dependency { .. } => SourceOwner::TaskGraph,
            Self::Message { .. } => SourceOwner::Mailbox,
            Self::HumanDecision { .. } => SourceOwner::HumanDecision,
            Self::Timer { .. } => SourceOwner::Clock,
            Self::Operation { .. } => SourceOwner::Operation,
            Self::PlaybookRun { .. } => SourceOwner::Playbook,
            Self::WaitOutcome { .. } => SourceOwner::WaitRegistry,
            Self::Resource { .. } => SourceOwner::Scheduler,
        }
    }
}
impl Validate for Condition {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Task {
                task_id, revision, ..
            } => {
                uuid(*task_id)?;
                revision.validate()
            }
            Self::Dependency {
                edge_id,
                edge_revision,
                from_task_id,
                depends_on_task_id,
                ..
            } => {
                for id in [edge_id, from_task_id, depends_on_task_id] {
                    uuid(*id)?;
                }
                ensure(from_task_id != depends_on_task_id, "self_wait")?;
                edge_revision.validate()
            }
            Self::Message {
                thread_id, sender, ..
            } => {
                uuid(*thread_id)?;
                match sender {
                    SenderConstraint::AnyAuthorized => Ok(()),
                    SenderConstraint::Task { task_id } => uuid(*task_id),
                    SenderConstraint::Session { binding } => binding.validate(),
                }
            }
            Self::HumanDecision {
                decision_id,
                subject_id,
                subject_revision,
                ..
            } => {
                uuid(*decision_id)?;
                uuid(*subject_id)?;
                subject_revision.validate()
            }
            Self::Timer { due_at_ms } => ensure(*due_at_ms > 0, "timer instant"),
            Self::Operation {
                operation_id,
                revision,
                ..
            } => {
                uuid(*operation_id)?;
                revision.validate()
            }
            Self::PlaybookRun { run_id, revision } => {
                uuid(*run_id)?;
                revision.validate()
            }
            Self::WaitOutcome { wait } => wait.validate(),
            Self::Resource {
                selection_snapshot, ..
            } => artifact(selection_snapshot),
        }
    }
}
record!(Member {
    member_id: Uuid,
    condition: Condition
});
impl Validate for Member {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.member_id)?;
        self.condition.validate()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    Single { member: Member },
    All { members: Vec<Member> },
    Any { members: Vec<Member> },
}
impl Predicate {
    pub fn members(&self) -> Vec<&Member> {
        match self {
            Self::Single { member } => vec![member],
            Self::All { members } | Self::Any { members } => members.iter().collect(),
        }
    }
}
impl Validate for Predicate {
    fn validate(&self) -> Result<(), ContractError> {
        let members = self.members();
        ensure(
            !members.is_empty() && members.len() <= MAX_MEMBERS,
            "predicate member bound",
        )?;
        if !matches!(self, Self::Single { .. }) {
            ensure(members.len() >= 2, "composite predicate minimum")?;
        }
        let mut ids = BTreeSet::new();
        let mut conditions = BTreeSet::new();
        for member in members {
            member.validate()?;
            ensure(ids.insert(member.member_id), "duplicate wait member")?;
            ensure(
                conditions.insert(canonical_digest(&member.condition)?),
                "duplicate condition",
            )?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitPreference {
    Economical,
    Hibernate,
}
record!(WaitRegistration {
    scope: Scope,
    binding: Binding,
    wait_id: Uuid,
    wait_generation: u64,
    continuation_generation: u64,
    policy: ArtifactRef,
    predicate: Predicate,
    predicate_digest: Digest,
    preference: WaitPreference
});
impl WaitRegistration {
    pub fn reject_self(
        &self,
        current_operation: Option<Uuid>,
        current_run: Option<Uuid>,
    ) -> Result<(), ContractError> {
        for member in self.predicate.members() {
            let self_wait = match &member.condition {
                Condition::Task { task_id, .. } => *task_id == self.binding.task_id,
                Condition::Dependency {
                    depends_on_task_id, ..
                } => *depends_on_task_id == self.binding.task_id,
                Condition::Operation { operation_id, .. } => {
                    Some(*operation_id) == current_operation
                }
                Condition::PlaybookRun { run_id, .. } => Some(*run_id) == current_run,
                Condition::WaitOutcome { wait } => wait.wait_id == self.wait_id,
                Condition::Message { sender, .. } => match sender {
                    SenderConstraint::Task { task_id } => *task_id == self.binding.task_id,
                    SenderConstraint::Session { binding } => binding == &self.binding,
                    _ => false,
                },
                _ => false,
            };
            ensure(!self_wait, "self_wait")?;
        }
        Ok(())
    }
    pub fn wait_ref(&self) -> WaitRef {
        WaitRef {
            wait_id: self.wait_id,
            generation: self.wait_generation,
            predicate_digest: self.predicate_digest.clone(),
        }
    }
}
impl Validate for WaitRegistration {
    fn validate(&self) -> Result<(), ContractError> {
        self.scope.validate()?;
        self.binding.validate()?;
        uuid(self.wait_id)?;
        ensure(
            self.scope.project_id == self.binding.project_id,
            "wait project binding",
        )?;
        ensure(
            self.wait_generation > 0 && self.continuation_generation > 0,
            "wait/continuation generation",
        )?;
        artifact(&self.policy)?;
        self.predicate.validate()?;
        ensure(
            canonical_digest(&self.predicate)? == self.predicate_digest,
            "predicate digest",
        )?;
        self.reject_self(None, None)
    }
}
// Source-owner committed/replay boundary, not a maximum allocated sequence.
record!(SourceBoundary {
    member_id: Uuid,
    owner: SourceOwner,
    condition_digest: Digest,
    committed_revision: Text,
    committed_replay_cursor: Text,
    subscription_receipt: ArtifactRef,
    snapshot_digest: Digest
});
impl Validate for SourceBoundary {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.member_id)?;
        self.committed_revision.validate()?;
        self.committed_replay_cursor.validate()?;
        artifact(&self.subscription_receipt)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CycleAssessment {
    NoKnownCycle { revision: ArtifactRef },
    ExternalUnknown { reason: Text },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitExecutionState {
    PolicyPending,
    WarmBounded { expires_at_ms: u64 },
    SuspensionRequested,
    ParkedReported { receipt: ArtifactRef },
}
record!(RegisteredWait {scope:Scope,binding:Binding,wait:WaitRef,predicate:Predicate,continuation:ContinuationRef,original_created_at_ms:u64,central_hold_receipt:ArtifactRef,boundaries:Vec<SourceBoundary>,cycle_assessment:CycleAssessment,execution:WaitExecutionState});
impl RegisteredWait {
    fn validate_boundaries(&self) -> Result<(), ContractError> {
        ensure(
            !self.boundaries.is_empty() && self.boundaries.len() <= MAX_MEMBERS,
            "source boundary bound",
        )?;
        let mut ids = BTreeSet::new();
        for boundary in &self.boundaries {
            boundary.validate()?;
            ensure(ids.insert(boundary.member_id), "duplicate source boundary")?;
        }
        let members = self.predicate.members();
        ensure(
            members.len() == self.boundaries.len(),
            "registered source boundaries",
        )?;
        for (member, boundary) in members.iter().zip(&self.boundaries) {
            ensure(
                member.member_id == boundary.member_id
                    && member.condition.owner() == boundary.owner
                    && canonical_digest(&member.condition)? == boundary.condition_digest,
                "source boundary condition",
            )?;
        }
        Ok(())
    }
}
impl Validate for RegisteredWait {
    fn validate(&self) -> Result<(), ContractError> {
        self.scope.validate()?;
        self.binding.validate()?;
        self.wait.validate()?;
        self.predicate.validate()?;
        ensure(
            canonical_digest(&self.predicate)? == self.wait.predicate_digest,
            "registered predicate digest",
        )?;
        self.continuation.validate()?;
        ensure(
            self.continuation.wait == self.wait,
            "registration continuation binding",
        )?;
        ensure(
            self.scope.project_id == self.binding.project_id,
            "registered project",
        )?;
        artifact(&self.central_hold_receipt)?;
        self.validate_boundaries()?;
        match &self.cycle_assessment {
            CycleAssessment::NoKnownCycle { revision } => artifact(revision)?,
            CycleAssessment::ExternalUnknown { reason } => reason.validate()?,
        }
        match &self.execution {
            WaitExecutionState::WarmBounded { expires_at_ms } => ensure(
                *expires_at_ms > self.original_created_at_ms
                    && expires_at_ms - self.original_created_at_ms <= 30000,
                "warm wait bound",
            )?,
            WaitExecutionState::ParkedReported { receipt } => artifact(receipt)?,
            _ => {}
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MemberState {
    Pending,
    Satisfied,
    SourceFailed,
    SourceUnavailable,
}
record!(SourceEvidence {
    owner: SourceOwner,
    condition_digest: Digest,
    revision: Text,
    result: ArtifactRef
});
impl Validate for SourceEvidence {
    fn validate(&self) -> Result<(), ContractError> {
        self.revision.validate()?;
        artifact(&self.result)
    }
}
record!(MemberOutcome {member_id:Uuid,state:MemberState,evidence:Option<SourceEvidence>});
impl Validate for MemberOutcome {
    fn validate(&self) -> Result<(), ContractError> {
        uuid(self.member_id)?;
        if let Some(e) = &self.evidence {
            e.validate()?;
        }
        ensure(
            !matches!(
                self.state,
                MemberState::Satisfied | MemberState::SourceFailed
            ) || self.evidence.is_some(),
            "source result evidence",
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcomeKind {
    Satisfied,
    SourceFailed,
    Cancelled,
    DeadlineReached,
    SourceUnavailable,
    ManualOverride,
}
record!(WaitOutcome {outcome:OutcomeRef,kind:WaitOutcomeKind,members:Vec<MemberOutcome>,selected_member_id:Option<Uuid>,recorded_at_ms:u64,unresolved_predicate:Option<Digest>});
impl Validate for WaitOutcome {
    fn validate(&self) -> Result<(), ContractError> {
        self.outcome.validate()?;
        ensure(
            !self.members.is_empty() && self.members.len() <= MAX_MEMBERS,
            "outcome member bound",
        )?;
        let mut ids = BTreeSet::new();
        for member in &self.members {
            member.validate()?;
            ensure(ids.insert(member.member_id), "duplicate member outcome")?;
        }
        if let Some(id) = self.selected_member_id {
            ensure(ids.contains(&id), "selected member")?;
        }
        if self.kind == WaitOutcomeKind::ManualOverride {
            ensure(
                self.unresolved_predicate.as_ref() == Some(&self.outcome.wait.predicate_digest),
                "manual override preserves unresolved predicate",
            )?;
        } else {
            ensure(
                self.unresolved_predicate.is_none(),
                "unresolved predicate only manual override",
            )?;
        }
        Ok(())
    }
}

impl WaitOutcome {
    fn validate_members(&self, predicate: &Predicate) -> Result<(), ContractError> {
        let members = predicate.members();
        ensure(
            members.len() == self.members.len(),
            "outcome member completeness",
        )?;
        for (wanted, got) in members.iter().zip(&self.members) {
            ensure(wanted.member_id == got.member_id, "outcome member identity")?;
            if let Some(e) = &got.evidence {
                ensure(
                    e.owner == wanted.condition.owner()
                        && e.condition_digest == canonical_digest(&wanted.condition)?,
                    "outcome source owner/condition",
                )?;
            }
        }
        Ok(())
    }
    fn validate_source_failure(
        &self,
        any: bool,
        state: MemberState,
        message: &'static str,
    ) -> Result<(), ContractError> {
        ensure(self.members.iter().any(|v| v.state == state), message)?;
        if any {
            ensure(
                !self
                    .members
                    .iter()
                    .any(|v| matches!(v.state, MemberState::Pending | MemberState::Satisfied)),
                "any predicate still pending",
            )?;
        }
        Ok(())
    }

    fn validate_satisfaction(&self, any: bool) -> Result<(), ContractError> {
        let satisfied: Vec<Uuid> = self
            .members
            .iter()
            .filter(|v| v.state == MemberState::Satisfied)
            .map(|v| v.member_id)
            .collect();
        ensure(
            if any {
                !satisfied.is_empty()
            } else {
                satisfied.len() == self.members.len()
            },
            "satisfaction semantics",
        )?;
        ensure(
            self.selected_member_id
                == if any {
                    satisfied.first().copied()
                } else {
                    None
                },
            "deterministic selection",
        )?;
        Ok(())
    }
    pub fn validate_for(&self, predicate: &Predicate) -> Result<(), ContractError> {
        self.validate()?;
        predicate.validate()?;
        ensure(
            canonical_digest(predicate)? == self.outcome.wait.predicate_digest,
            "outcome predicate",
        )?;
        self.validate_members(predicate)?;
        let any = matches!(predicate, Predicate::Any { .. });
        match self.kind {
            WaitOutcomeKind::Satisfied => self.validate_satisfaction(any)?,
            WaitOutcomeKind::SourceFailed => {
                self.validate_source_failure(any, MemberState::SourceFailed, "failure evidence")?
            }
            WaitOutcomeKind::SourceUnavailable => self.validate_source_failure(
                any,
                MemberState::SourceUnavailable,
                "unavailable source",
            )?,
            _ => {}
        }
        Ok(())
    }
}
