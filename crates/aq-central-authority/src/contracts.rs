use super::*;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CentralSubject {
    CentralConversation {
        actor_id: Uuid,
        conversation_id: Uuid,
        conversation_revision: u64,
        policy: ArtifactRef,
    },
}
impl CentralSubject {
    pub fn actor_id(&self) -> Uuid {
        match self {
            Self::CentralConversation { actor_id, .. } => *actor_id,
        }
    }
    pub fn conversation_id(&self) -> Uuid {
        match self {
            Self::CentralConversation {
                conversation_id, ..
            } => *conversation_id,
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::CentralConversation {
                actor_id,
                conversation_id,
                conversation_revision,
                policy,
            } => {
                id(*actor_id)?;
                id(*conversation_id)?;
                revision(*conversation_revision)?;
                artifact(policy)
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfilePins {
    pub catalog: ArtifactRef,
    pub agent: ArtifactRef,
    pub intelligence: ArtifactRef,
    pub execution: ArtifactRef,
    pub composition: ArtifactRef,
    pub selection: ArtifactRef,
    pub host_plan: ArtifactRef,
    pub adapter: ArtifactRef,
    pub environment: ArtifactRef,
    pub renderer: ArtifactRef,
    pub startup: ArtifactRef,
}
impl ProfilePins {
    pub fn validate(&self) -> Result<()> {
        for r in [
            &self.catalog,
            &self.agent,
            &self.intelligence,
            &self.execution,
            &self.composition,
            &self.selection,
            &self.host_plan,
            &self.adapter,
            &self.environment,
            &self.renderer,
            &self.startup,
        ] {
            artifact(r)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailRef {
    pub message_id: Uuid,
    pub cursor: u64,
    pub payload_digest: Digest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailBatch {
    pub conversation_id: Uuid,
    pub through_cursor: u64,
    #[schemars(length(min = 1, max = 16))]
    pub messages: Vec<MailRef>,
}
impl MailBatch {
    pub fn validate(&self) -> Result<()> {
        id(self.conversation_id)?;
        revision(self.through_cursor)?;
        ensure(
            !self.messages.is_empty() && self.messages.len() <= MAX_BATCH,
            "batch bound",
        )?;
        let mut previous = 0;
        let mut ids = std::collections::BTreeSet::new();
        for m in &self.messages {
            id(m.message_id)?;
            revision(m.cursor)?;
            ensure(
                m.cursor > previous && m.cursor <= self.through_cursor && ids.insert(m.message_id),
                "batch order/identity",
            )?;
            previous = m.cursor;
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        digest(self)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralPlan {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub subject: CentralSubject,
    pub session: SessionIdentity,
    pub generation: u64,
    pub profiles: ProfilePins,
    pub batch: MailBatch,
    pub dispatch_id: Uuid,
    pub host_operation_id: Uuid,
    #[schemars(range(min = 1, max = 3600000))]
    pub wall_time_ms: u64,
    #[schemars(range(min = 1, max = 120000))]
    pub max_permit_ms: u64,
}
impl CentralPlan {
    pub fn validate(&self) -> Result<()> {
        ensure(self.schema_version == 1, "plan version")?;
        self.subject.validate()?;
        central(&self.session, self.generation)?;
        self.profiles.validate()?;
        self.batch.validate()?;
        ensure(
            self.batch.conversation_id == self.subject.conversation_id(),
            "batch subject",
        )?;
        id(self.dispatch_id)?;
        id(self.host_operation_id)?;
        ensure(
            self.dispatch_id != self.host_operation_id,
            "dispatch/host operation identities",
        )?;
        ensure(
            self.wall_time_ms > 0
                && self.wall_time_ms <= MAX_WALL_MS
                && self.max_permit_ms > 0
                && self.max_permit_ms <= MAX_PERMIT_MS
                && self.max_permit_ms <= self.wall_time_ms,
            "turn/permit bound",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        digest(self)
    }
    pub fn binding(&self) -> Result<CentralBinding> {
        self.validate()?;
        Ok(CentralBinding {
            subject: self.subject.clone(),
            session: self.session.clone(),
            generation: self.generation,
            plan_digest: self.digest()?,
        })
    }
    pub fn dispatch(&self, permit: PermitData) -> Result<DispatchAssociation> {
        permit.validate()?;
        ensure(permit.binding == self.binding()?, "dispatch plan binding")?;
        Ok(DispatchAssociation {
            dispatch_id: self.dispatch_id,
            host_operation_id: self.host_operation_id,
            batch_digest: self.batch.digest()?,
            permit,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CentralBinding {
    pub subject: CentralSubject,
    pub session: SessionIdentity,
    pub generation: u64,
    pub plan_digest: Digest,
}
impl CentralBinding {
    pub fn validate(&self) -> Result<()> {
        self.subject.validate()?;
        central(&self.session, self.generation)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Clock {
    pub machine_boot_id: Uuid,
    pub boot_ms: u64,
}
impl Clock {
    pub fn validate(&self) -> Result<()> {
        id(self.machine_boot_id)
    }
}
/// Structural permit metadata; this is NOT the physical host's authority object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermitData {
    pub binding: CentralBinding,
    pub machine_boot_id: Uuid,
    pub sequence: u64,
    pub issued_boot_ms: u64,
    pub expires_boot_ms: u64,
}
impl PermitData {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        id(self.machine_boot_id)?;
        revision(self.sequence)?;
        let span = self
            .expires_boot_ms
            .checked_sub(self.issued_boot_ms)
            .ok_or(Error::Invalid("permit time"))?;
        ensure(span > 0 && span <= MAX_PERMIT_MS, "permit duration")
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchAssociation {
    pub dispatch_id: Uuid,
    pub host_operation_id: Uuid,
    pub batch_digest: Digest,
    pub permit: PermitData,
}
impl DispatchAssociation {
    pub fn validate(&self) -> Result<()> {
        id(self.dispatch_id)?;
        id(self.host_operation_id)?;
        ensure(
            self.dispatch_id != self.host_operation_id,
            "dispatch/host identity",
        )?;
        self.permit.validate()
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostLaunchRef {
    pub binding: CentralBinding,
    pub launch_id: Uuid,
    pub intent: ArtifactRef,
}
impl HostLaunchRef {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        id(self.launch_id)?;
        artifact(&self.intent)
    }
}
/// A reference to an owner's evidence. Shape validation cannot authenticate it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostObservationRef {
    pub binding: CentralBinding,
    pub launch_id: Uuid,
    pub guardian: ArtifactRef,
    pub observation: ArtifactRef,
}
impl HostObservationRef {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        id(self.launch_id)?;
        artifact(&self.guardian)?;
        artifact(&self.observation)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopEvidenceRef {
    pub host: HostObservationRef,
    pub stopped_writers: ArtifactRef,
}
impl StopEvidenceRef {
    pub fn validate(&self) -> Result<()> {
        self.host.validate()?;
        artifact(&self.stopped_writers)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HandoffDisposition {
    HandedOff,
    NotHandedOff,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandoffEvidence {
    pub association: DispatchAssociation,
    pub disposition: HandoffDisposition,
    pub receipt: ArtifactRef,
}
impl HandoffEvidence {
    pub fn validate(&self) -> Result<()> {
        self.association.validate()?;
        artifact(&self.receipt)
    }
}
/// Future reply association only; no sender authentication or mailbox mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplyAssociation {
    pub dispatch: DispatchAssociation,
    pub reply_message_id: Uuid,
    pub reply_operation_id: Uuid,
    pub body_digest: Digest,
}
impl ReplyAssociation {
    pub fn validate(&self) -> Result<()> {
        self.dispatch.validate()?;
        id(self.reply_message_id)?;
        id(self.reply_operation_id)?;
        ensure(
            self.reply_message_id != self.reply_operation_id,
            "reply identities",
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailHoldReason {
    Pending,
    Unknown,
    HandedOff,
}
/// Aggregated over ALL retained generations by a future authenticated owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoryDisposition {
    NoPriorDispatch,
    Retryable {
        evidence: HandoffEvidence,
    },
    Held {
        association: DispatchAssociation,
        reason: MailHoldReason,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageHistory {
    pub message: MailRef,
    pub disposition: HistoryDisposition,
}
/// Bounded candidate query results, not a proof that the source was authenticated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailHistoryFence {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub actor_id: Uuid,
    pub conversation_id: Uuid,
    pub through_generation: u64,
    pub source_snapshot: ArtifactRef,
    #[schemars(length(min = 1, max = 16))]
    pub entries: Vec<MessageHistory>,
    /// Candidate dispatch and host-input UUIDs share a per-actor operation namespace.
    #[schemars(length(min = 2, max = 2))]
    pub operations: Vec<OperationHistory>,
}
impl MailHistoryFence {
    pub fn validate(&self) -> Result<()> {
        ensure(self.schema_version == 1, "history version")?;
        id(self.actor_id)?;
        id(self.conversation_id)?;
        revision(self.through_generation)?;
        artifact(&self.source_snapshot)?;
        ensure(
            !self.entries.is_empty() && self.entries.len() <= MAX_BATCH,
            "history bound",
        )?;
        let mut ids = std::collections::BTreeSet::new();
        let mut cursors = std::collections::BTreeSet::new();
        for entry in &self.entries {
            id(entry.message.message_id)?;
            revision(entry.message.cursor)?;
            ensure(
                ids.insert(entry.message.message_id) && cursors.insert(entry.message.cursor),
                "duplicate history candidate",
            )?;
            let association = match &entry.disposition {
                HistoryDisposition::NoPriorDispatch => None,
                HistoryDisposition::Retryable { evidence } => {
                    evidence.validate()?;
                    ensure(
                        evidence.disposition == HandoffDisposition::NotHandedOff,
                        "retry requires exact not-handed-off receipt",
                    )?;
                    Some(&evidence.association)
                }
                HistoryDisposition::Held { association, .. } => {
                    association.validate()?;
                    Some(association)
                }
            };
            if let Some(a) = association {
                let b = &a.permit.binding;
                ensure(
                    b.subject.actor_id() == self.actor_id
                        && b.subject.conversation_id() == self.conversation_id
                        && b.generation <= self.through_generation,
                    "history association subject/generation",
                )?;
            }
        }
        ensure(self.operations.len() == 2, "operation history bound")?;
        let mut operation_ids = std::collections::BTreeSet::new();
        for operation in &self.operations {
            id(operation.operation_id)?;
            ensure(
                operation_ids.insert(operation.operation_id),
                "duplicate operation candidate",
            )?;
            if let OperationDisposition::Used { generation, .. } = operation.disposition {
                revision(generation)?;
                ensure(
                    generation <= self.through_generation,
                    "operation generation",
                )?;
            }
        }
        Ok(())
    }
}
pub fn validate_successor_history(
    plan: &CentralPlan,
    previous_generation: u64,
    history: &MailHistoryFence,
) -> Result<()> {
    plan.validate()?;
    history.validate()?;
    if history.actor_id != plan.subject.actor_id()
        || history.conversation_id != plan.subject.conversation_id()
        || history.through_generation != previous_generation
        || previous_generation.checked_add(1) != Some(plan.generation)
    {
        return Err(Error::Refused(Refusal::Binding));
    }
    if history.entries.len() != plan.batch.messages.len() {
        return Err(Error::Refused(Refusal::Binding));
    }
    for message in &plan.batch.messages {
        let entry = history
            .entries
            .iter()
            .find(|e| {
                e.message.message_id == message.message_id || e.message.cursor == message.cursor
            })
            .ok_or(Error::Refused(Refusal::Binding))?;
        if entry.message != *message {
            return Err(Error::Refused(Refusal::Conflict));
        }
        if matches!(entry.disposition, HistoryDisposition::Held { .. }) {
            return Err(Error::Refused(Refusal::BatchHeld));
        }
    }
    for operation_id in [plan.dispatch_id, plan.host_operation_id] {
        let operation = history
            .operations
            .iter()
            .find(|o| o.operation_id == operation_id)
            .ok_or(Error::Refused(Refusal::Binding))?;
        if !matches!(operation.disposition, OperationDisposition::Unused) {
            return Err(Error::Refused(Refusal::Conflict));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationRole {
    Dispatch,
    HostInput,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OperationDisposition {
    Unused,
    Used {
        generation: u64,
        role: OperationRole,
        association_digest: Digest,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationHistory {
    pub operation_id: Uuid,
    pub disposition: OperationDisposition,
}
