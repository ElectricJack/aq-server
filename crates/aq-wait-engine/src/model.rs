use super::*;

/// Trusted caller observation, never deserialized from an agent request. Outer
/// owner derives Binding through actual lifecycle_binding/authority joins.
#[derive(Clone, Debug)]
pub struct AuthorityFence {
    pub scope: wire::Scope,
    pub binding: wire::Binding,
    pub principal: wire::Digest,
    pub source_access: wire::Digest,
    pub task_cancelled: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorityKey {
    pub scope: wire::Scope,
    pub binding: wire::Binding,
    pub principal: wire::Digest,
    pub source_access: wire::Digest,
}
impl AuthorityFence {
    pub(crate) fn key(&self) -> Result<AuthorityKey> {
        self.scope.validate()?;
        self.binding.validate()?;
        require(self.scope.project_id == self.binding.project_id)?;
        Ok(AuthorityKey {
            scope: self.scope.clone(),
            binding: self.binding.clone(),
            principal: self.principal.clone(),
            source_access: self.source_access.clone(),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyBody {
    pub maximum_age_ms: u64,
    pub check_interval_ms: u64,
    pub retry_base_ms: u64,
    pub retry_cap_ms: u64,
    pub max_retries: u32,
    pub max_work_attempts: u32,
}
impl Validate for PolicyBody {
    fn validate(&self) -> std::result::Result<(), wire::ContractError> {
        wire::ensure(
            self.maximum_age_ms > 0
                && self.maximum_age_ms <= MAX_AGE_MS
                && (1000..=60_000).contains(&self.check_interval_ms)
                && self.retry_base_ms >= 1000
                && self.retry_base_ms <= self.retry_cap_ms
                && self.retry_cap_ms <= 60_000
                && (1..=4096).contains(&self.max_retries)
                && (1..=65_536).contains(&self.max_work_attempts),
            "bounded wait policy",
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPolicy {
    pub reference: wire::ArtifactRef,
    pub body: PolicyBody,
}
impl ResolvedPolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        wire::artifact(&self.reference)?;
        self.body.validate()?;
        require(self.reference.digest == canonical_digest(&self.body)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StableIds {
    pub continuation_id: Uuid,
    pub outcome_id: Uuid,
    pub resume_intent_id: Uuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionId {
    pub wait: wire::WaitRef,
    pub member_id: Uuid,
}
impl Validate for SubscriptionId {
    fn validate(&self) -> std::result::Result<(), wire::ContractError> {
        self.wait.validate()?;
        wire::uuid(self.member_id)
    }
}
impl SubscriptionId {
    pub fn digest(&self) -> Result<wire::Digest> {
        Ok(canonical_digest(self)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Install,
    Reobserve,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkTicket {
    pub wait: wire::WaitRef,
    pub generation: u64,
    pub dirty_through: u64,
    pub kind: WorkKind,
}
/// Trusted source call, not a public command. Every round is bounded by32 members;
/// the outside coordinator separately admits at most its fixed physical IO limit.
#[derive(Clone, Debug)]
pub struct SourceRequest {
    pub subscription: SubscriptionId,
    pub scope: wire::Scope,
    pub binding: wire::Binding,
    pub principal: wire::Digest,
    pub source_access: wire::Digest,
    pub member: wire::Member,
    pub previous: Option<wire::SourceBoundary>,
    pub incarnation: Option<wire::Text>,
}
/// Owner protocol qualification, not a numeric sequence guessed by this engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceProtocol {
    CommittedReplay,
    Unqualified,
}
/// No Deserialize: only the trusted source adapter/fixture constructs these.
/// The adapter must verify real committed coverage; a digest is not a signature.
#[derive(Clone, Debug)]
pub struct CommittedObservation {
    pub protocol: SourceProtocol,
    pub subscription: SubscriptionId,
    pub scope: wire::Scope,
    pub binding: wire::Binding,
    pub principal: wire::Digest,
    pub source_access: wire::Digest,
    pub incarnation: wire::Text,
    pub boundary: wire::SourceBoundary,
    /// Exact owner snapshot reference/hash, separate from this AQ projection.
    pub owner_snapshot: wire::ArtifactRef,
    pub projection_digest: wire::Digest,
    pub replay_from: Option<wire::Text>,
    pub observation: wire::MemberOutcome,
}
#[derive(Clone, Debug)]
pub enum SourceReply {
    Committed(Box<CommittedObservation>),
    RetryableUnavailable { member_id: Uuid, reason: wire::Text },
    ReplayGap { member_id: Uuid, reason: wire::Text },
}
impl SourceReply {
    pub fn member_id(&self) -> Uuid {
        match self {
            Self::Committed(v) => v.subscription.member_id,
            Self::RetryableUnavailable { member_id, .. } | Self::ReplayGap { member_id, .. } => {
                *member_id
            }
        }
    }
}
/// Ports are internal owner integrations. No agent-provided evidence/verdict
/// endpoint or generic workflow command/event registration is implemented.
pub trait SourceOwnerPort {
    fn install(&mut self, request: &SourceRequest) -> Result<SourceReply>;
    fn reobserve(&mut self, request: &SourceRequest) -> Result<SourceReply>;
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Installing,
    Pending,
    RecoveryHeld,
    Terminal,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemberProgress {
    pub original: Option<wire::SourceBoundary>,
    pub current: Option<wire::SourceBoundary>,
    pub original_observation: Option<wire::MemberOutcome>,
    pub current_observation: Option<wire::MemberOutcome>,
    pub original_snapshot: Option<wire::ArtifactRef>,
    pub current_snapshot: Option<wire::ArtifactRef>,
    pub incarnation: Option<wire::Text>,
    pub last: wire::MemberOutcome,
    pub unavailable_reason: Option<wire::Text>,
    pub last_checked_at: Option<u64>,
    pub last_verified_at: Option<u64>,
}
/// Read-only member freshness for owner/agent inspection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MemberFreshness {
    pub state: wire::MemberOutcome,
    pub unavailable_reason: Option<wire::Text>,
    pub last_checked_at_ms: Option<u64>,
    pub last_verified_at_ms: Option<u64>,
}
/// Registry data only; no active-grant, hold-proof or physical-state fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StoredSnapshot")]
pub struct Snapshot {
    pub(crate) request: wire::Request,
    pub(crate) authority: AuthorityKey,
    pub(crate) ids: StableIds,
    pub(crate) policy: ResolvedPolicy,
    pub(crate) created: u64,
    pub(crate) deadline: u64,
    pub(crate) logical_now: u64,
    pub(crate) revision: u64,
    pub(crate) dirty: u64,
    pub(crate) completed_dirty: u64,
    pub(crate) work_generation: u64,
    pub(crate) inflight: Option<WorkTicket>,
    pub(crate) next_due: u64,
    pub(crate) retries: u32,
    pub(crate) phase: Phase,
    pub(crate) members: Vec<MemberProgress>,
    pub(crate) outcome: Option<wire::WaitOutcome>,
    pub(crate) hold_reason: Option<wire::Text>,
    pub(crate) resume_allowed: bool,
    pub(crate) cycle: wire::CycleAssessment,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSnapshot {
    request: wire::Request,
    authority: AuthorityKey,
    ids: StableIds,
    policy: ResolvedPolicy,
    created: u64,
    deadline: u64,
    logical_now: u64,
    revision: u64,
    dirty: u64,
    completed_dirty: u64,
    work_generation: u64,
    inflight: Option<WorkTicket>,
    next_due: u64,
    retries: u32,
    phase: Phase,
    members: Vec<MemberProgress>,
    outcome: Option<wire::WaitOutcome>,
    hold_reason: Option<wire::Text>,
    resume_allowed: bool,
    cycle: wire::CycleAssessment,
}
impl TryFrom<StoredSnapshot> for Snapshot {
    type Error = Error;
    fn try_from(value: StoredSnapshot) -> Result<Self> {
        let result = Self {
            request: value.request,
            authority: value.authority,
            ids: value.ids,
            policy: value.policy,
            created: value.created,
            deadline: value.deadline,
            logical_now: value.logical_now,
            revision: value.revision,
            dirty: value.dirty,
            completed_dirty: value.completed_dirty,
            work_generation: value.work_generation,
            inflight: value.inflight,
            next_due: value.next_due,
            retries: value.retries,
            phase: value.phase,
            members: value.members,
            outcome: value.outcome,
            hold_reason: value.hold_reason,
            resume_allowed: value.resume_allowed,
            cycle: value.cycle,
        };
        result.validate()?;
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResumeIntent {
    pub intent_id: Uuid,
    pub continuation: wire::ContinuationRef,
    pub outcome: wire::OutcomeRef,
}
#[derive(Clone, Debug)]
pub enum Action {
    Install {
        ticket: WorkTicket,
        sources: Vec<SourceRequest>,
    },
    Reobserve {
        ticket: WorkTicket,
        sources: Vec<SourceRequest>,
    },
    PersistOutcome {
        outcome: Box<wire::WaitOutcome>,
        resume: Option<ResumeIntent>,
    },
    RetireResume {
        intent_id: Uuid,
        wait: wire::WaitRef,
    },
}
#[derive(Clone, Debug)]
pub struct TransitionPlan {
    pub expected_revision: Option<u64>,
    pub next: Snapshot,
    pub actions: Vec<Action>,
    pub replay: bool,
}
#[derive(Clone, Debug)]
pub enum Input {
    Drive,
    Hint {
        member_id: Uuid,
    },
    SourceBatch {
        ticket: WorkTicket,
        replies: Vec<SourceReply>,
    },
    /// Reclaim only after the outside durable owner proves the old work lease
    /// is no longer current. Old replies become stale; subscription IDs do not.
    Reclaim {
        ticket: WorkTicket,
    },
}
