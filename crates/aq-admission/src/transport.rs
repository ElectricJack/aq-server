//! AQ-owned profiled transport v1, additive to the frozen preparation documents.
//! These envelopes are not installed handlers and never verify physical stop.
use crate::*;
use aq_session_protocol::{Capability, ProfiledAcceptance, ProfiledGrant, Validate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
/// Stable task aggregate: changing attempts or lifecycle records cannot reset budgets.
/// The namespace and raw network-order encoding are frozen independently of crate versions.
pub fn task_lineage_id(project: Uuid, workspace: Uuid, task: Uuid) -> Result<Uuid> {
    id(project)?;
    id(workspace)?;
    id(task)?;
    let namespace = Uuid::from_u128(0x43c0fccfeb6e5357949c0bc68fbf51ea);
    let mut name = [0u8; 48];
    name[..16].copy_from_slice(project.as_bytes());
    name[16..32].copy_from_slice(workspace.as_bytes());
    name[32..].copy_from_slice(task.as_bytes());
    Ok(Uuid::new_v5(&namespace, &name))
}
/// Called by the authoritative server producer, never from caller-selected lineage IDs.
pub fn lifecycle_binding(offer: &ProfiledOffer) -> Result<aq_lifecycle_protocol::Binding> {
    offer
        .validate()
        .map_err(|_| Invalid("profiled lifecycle offer"))?;
    let bindings = &offer.manifest.bindings;
    let generation = match offer.binding.session {
        SessionIdentity::Remote {
            assignment_generation,
            ..
        } => assignment_generation,
        SessionIdentity::Central { generation, .. } => generation,
    };
    let result = aq_lifecycle_protocol::Binding {
        project_id: bindings.project_id,
        task_id: bindings.task_id,
        lineage_id: task_lineage_id(bindings.project_id, bindings.graph_id, bindings.task_id)?,
        attempt_id: offer.binding.attempt_id,
        attempt_generation: generation,
        session: offer.binding.session.clone(),
    };
    result
        .validate()
        .map_err(|_| Invalid("lifecycle binding"))?;
    Ok(result)
}
pub const MAX_FRAME_BYTES: usize = 1_048_576;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub node_id: Uuid,
    pub boot_id: Uuid,
}
impl WorkerIdentity {
    pub fn validate(&self) -> Result<()> {
        id(self.node_id)?;
        id(self.boot_id)
    }
    fn matches(&self, binding: &LaunchBinding) -> bool {
        matches!(binding.session,SessionIdentity::Remote{node_id,boot_id,..}if node_id==self.node_id&&boot_id==self.boot_id)
    }
}
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    ArtifactTransferV1,
    ServerMaterialV1,
    DurableHostJournalV1,
    HostStopEvidenceV1,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryClock {
    pub server_time_ms: u64,
    pub expires_at_ms: u64,
    pub valid_for_ms: u64,
}
impl DeliveryClock {
    pub fn new(server_time_ms: u64, expires_at_ms: u64) -> Result<Self> {
        let valid_for_ms = expires_at_ms
            .checked_sub(server_time_ms)
            .ok_or(Invalid("expired delivery"))?;
        let result = Self {
            server_time_ms,
            expires_at_ms,
            valid_for_ms,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        require(
            self.server_time_ms > 0
                && (1..=120_000).contains(&self.valid_for_ms)
                && self.server_time_ms.checked_add(self.valid_for_ms) == Some(self.expires_at_ms),
            "conservative delivery TTL",
        )
    }
    /// Charge the entire request/response interval. Never anchor a cached old TTL
    /// to a new retry: the server MUST retime every response from immutable expiry.
    pub fn client_deadline_ns(&self, sent_ns: u64, received_ns: u64) -> Result<u64> {
        self.validate()?;
        require(received_ns >= sent_ns, "monotonic response interval")?;
        let deadline = sent_ns
            .checked_add(self.valid_for_ms * 1_000_000)
            .ok_or(Invalid("monotonic deadline overflow"))?;
        require(received_ns < deadline, "delivery consumed remaining TTL")?;
        Ok(deadline)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub runtime: ArtifactRef,
    pub capabilities: Vec<Capability>,
    pub features: Vec<Feature>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Poll {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub inventory_sequence: u64,
    pub available: Resources,
    pub limit: u32,
    pub after_offer: Option<Uuid>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferedLaunch {
    pub offer: ProfiledOffer,
    pub lifecycle_binding: aq_lifecycle_protocol::Binding,
    pub clock: DeliveryClock,
    pub reservation_ttl_ms: u64,
    pub required_features: Vec<Feature>,
    pub artifacts: Vec<RetainedArtifact>,
}
impl OfferedLaunch {
    pub fn validate(&self) -> Result<()> {
        self.offer
            .validate()
            .map_err(|_| Invalid("profiled offer"))?;
        require(
            matches!(self.offer.binding.session, SessionIdentity::Remote { .. }),
            "remote offered launch",
        )?;
        require(
            self.lifecycle_binding == lifecycle_binding(&self.offer)?,
            "server-derived task lineage",
        )?;
        self.clock.validate()?;
        require(
            (1..=120_000).contains(&self.reservation_ttl_ms),
            "reservation TTL",
        )?;
        features(&self.required_features)?;
        for required in [
            Feature::ArtifactTransferV1,
            Feature::ServerMaterialV1,
            Feature::DurableHostJournalV1,
        ] {
            require(
                self.required_features.contains(&required),
                "mandatory profiled runtime feature",
            )?;
        }
        verify_offer_artifacts(&self.offer, &self.artifacts, self.clock.server_time_ms)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PollPage {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub server_time_ms: u64,
    pub offers: Vec<OfferedLaunch>,
    pub cancellations: Vec<Cancellation>,
    pub next_after: Option<Uuid>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Cancellation {
    pub cancellation_id: Uuid,
    pub binding: LaunchBinding,
    pub authority_revision: u64,
    pub reason: CancellationReason,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    OwnerRequest,
    InputsChanged,
    Expired,
    CredentialRevoked,
    RecoveryFence,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub acceptance: ProfiledAcceptance,
    pub reservation: NoLaunchReservation,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub journal_revision: u64,
    pub journal_digest: Digest,
    pub reservation_remaining_ms: u64,
}
/// Preparation permission is not execution authority. Delivery and a private
/// binding receipt must complete before the server can issue an actual grant.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparationPlan {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub reservation: NoLaunchReservation,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub clock: DeliveryClock,
    pub materials: Vec<MaterialLease>,
}
impl PreparationPlan {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "preparation plan version")?;
        id(self.operation_id)?;
        self.binding
            .validate()
            .map_err(|_| Invalid("preparation binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote preparation",
        )?;
        self.reservation.validate()?;
        id(self.guardian_id)?;
        id(self.launch_id)?;
        self.clock.validate()?;
        require(
            !self.materials.is_empty() && self.materials.len() <= 16,
            "preparation material bound",
        )?;
        let mut seen = std::collections::BTreeSet::new();
        for lease in &self.materials {
            lease.validate()?;
            require(
                lease.binding == self.binding
                    && seen.insert(lease.lease_id)
                    && lease.issued_at_ms <= self.clock.server_time_ms
                    && self.clock.expires_at_ms <= lease.expires_at_ms,
                "preparation material scope and expiry",
            )?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReceipt {
    pub artifact: ArtifactRef,
    pub bytes: u64,
    pub digest: Digest,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoLaunchReservation {
    pub reservation_id: Uuid,
    pub reservation_operation_id: Uuid,
    pub reservation_journal_digest: Digest,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prepare {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub reservation: NoLaunchReservation,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub journal_revision: u64,
    pub journal_digest: Digest,
    pub artifacts: Vec<ArtifactReceipt>,
    pub materials: Vec<MaterialReceipt>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrantedLaunch {
    pub schema_version: u32,
    pub grant: ProfiledGrant,
    pub reservation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub clock: DeliveryClock,
    pub materials: Vec<MaterialLease>,
}
impl GrantedLaunch {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "grant transport version")?;
        self.grant
            .validate()
            .map_err(|_| Invalid("profiled grant"))?;
        id(self.reservation_id)?;
        id(self.guardian_id)?;
        id(self.launch_id)?;
        self.clock.validate()?;
        require(
            self.clock.expires_at_ms == self.grant.expires_at_ms,
            "immutable grant expiry",
        )?;
        require(
            !self.materials.is_empty() && self.materials.len() <= 16,
            "server material grant profile",
        )?;
        let mut seen = std::collections::BTreeSet::new();
        for lease in &self.materials {
            lease.validate()?;
            require(
                lease.binding == self.grant.binding
                    && lease.issued_at_ms <= self.clock.server_time_ms
                    && lease.expires_at_ms >= self.grant.expires_at_ms
                    && seen.insert(lease.lease_id),
                "grant material scope/expiry",
            )?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Renew {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub reservation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub journal_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialFetch {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub lease_id: Uuid,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialDelivery {
    pub lease: MaterialLease,
    pub clock: DeliveryClock,
}
impl MaterialDelivery {
    pub fn verify_received(
        &self,
        binding: &LaunchBinding,
        bytes: &[u8],
        sent_ns: u64,
        received_ns: u64,
    ) -> Result<u64> {
        require(
            self.clock.expires_at_ms == self.lease.expires_at_ms,
            "material delivery expiry",
        )?;
        let deadline = self.clock.client_deadline_ns(sent_ns, received_ns)?;
        self.lease
            .verify_material(binding, bytes, self.clock.server_time_ms)?;
        Ok(deadline)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArtifactFetch {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub artifact: ArtifactRef,
    pub offset: u64,
    pub length: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum JournalEvidence {
    Recorded { revision: u64, digest: Digest },
    Unavailable { reason: JournalUnavailable },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JournalUnavailable {
    Missing,
    Unreadable,
    Corrupt,
    GuardianUnavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopDisposition {
    NotStarted {
        reservation: NoLaunchReservation,
        /// Last observed grant; None means durable reservation closed before
        /// observing a grant, not a claim that the server never issued one.
        grant_id: Option<Uuid>,
    },
    Stopped {
        grant_id: Uuid,
        stop_receipt_digest: Digest,
    },
    Unknown {
        grant_id: Option<Uuid>,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Cancellation,
    ExecutionExpiry,
    Completed,
    Failed,
    GuardianFailure,
    Reconciliation,
    PreparationAborted,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkerStopEvidence {
    pub schema_version: u32,
    pub binding: LaunchBinding,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub stop_operation_id: Uuid,
    pub journal: JournalEvidence,
    pub disposition: StopDisposition,
    pub reason: StopReason,
}
impl WorkerStopEvidence {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "stop evidence version")?;
        self.binding
            .validate()
            .map_err(|_| Invalid("stop binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote stop authority",
        )?;
        id(self.guardian_id)?;
        id(self.launch_id)?;
        id(self.stop_operation_id)?;
        if let JournalEvidence::Recorded { revision: r, .. } = &self.journal {
            revision(*r)?;
        }
        match &self.disposition {
            StopDisposition::NotStarted {
                reservation,
                grant_id,
            } => {
                reservation.validate()?;
                if let Some(grant) = grant_id {
                    id(*grant)?;
                }
                require(
                    matches!(self.journal, JournalEvidence::Recorded { .. }),
                    "no-launch requires durable guardian reservation journal",
                )?;
            }
            StopDisposition::Stopped { grant_id, .. } => {
                id(*grant_id)?;
                require(
                    matches!(self.journal, JournalEvidence::Recorded { .. }),
                    "stopped requires durable owner journal",
                )?;
            }
            StopDisposition::Unknown { grant_id } => {
                if let Some(grant) = grant_id {
                    id(*grant)?;
                }
            }
        }
        Ok(())
    }
    /// Structural completeness only. This never verifies physical stop or
    /// authorizes claim release; the assigned qualified producer must do so.
    pub fn is_release_candidate(&self) -> bool {
        self.validate().is_ok() && !matches!(self.disposition, StopDisposition::Unknown { .. })
    }
}
impl NoLaunchReservation {
    pub fn validate(&self) -> Result<()> {
        id(self.reservation_id)?;
        id(self.reservation_operation_id)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutcome {
    Success,
    Failed,
    Cancelled,
    Expired,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultReport {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub report_id: Uuid,
    pub supersedes: Option<Uuid>,
    pub outcome: ProcessOutcome,
    pub stop: WorkerStopEvidence,
    pub source_receipt_ids: Vec<Uuid>,
    pub pending_effect_ids: Vec<Uuid>,
    pub conclusion: Label,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reconcile {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub after_attempt: Option<Uuid>,
    pub limit: u32,
    pub attempts: Vec<ReconcileAttempt>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconcileAttempt {
    pub binding: LaunchBinding,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub last_grant_id: Option<Uuid>,
    pub journal: JournalEvidence,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TransportDocument {
    Register(Register),
    Poll(Poll),
    PollPage(PollPage),
    Accept(Accept),
    PreparationPlan(PreparationPlan),
    Prepare(Prepare),
    Grant(GrantedLaunch),
    Renew(Renew),
    MaterialFetch(MaterialFetch),
    MaterialDelivery(MaterialDelivery),
    ArtifactFetch(ArtifactFetch),
    StopEvidence(WorkerStopEvidence),
    Result(Box<ResultReport>),
    Reconcile(Reconcile),
}
fn features(values: &[Feature]) -> Result<()> {
    require(
        values.len() <= 4
            && values
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == values.len(),
        "runtime features",
    )
}
fn scope(identity: &WorkerIdentity, binding: &LaunchBinding) -> Result<()> {
    identity.validate()?;
    binding.validate().map_err(|_| Invalid("launch binding"))?;
    require(identity.matches(binding), "assigned node/boot scope")
}
fn header(version: u32, operation: Uuid, identity: &WorkerIdentity) -> Result<()> {
    require(version == 1, "transport version")?;
    id(operation)?;
    identity.validate()
}
fn ids(values: &[Uuid], max: usize) -> Result<()> {
    require(values.len() <= max, "receipt array bound")?;
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        id(*value)?;
        require(seen.insert(value), "duplicate receipt identity")?;
    }
    Ok(())
}
impl TransportDocument {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Register(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                artifact(&v.runtime)?;
                features(&v.features)?;
                aq_session_protocol::require_capabilities(&v.capabilities, &v.capabilities)
                    .map_err(|_| Invalid("registered capabilities"))?;
            }
            Self::Poll(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                revision(v.inventory_sequence)?;
                require((1..=8).contains(&v.limit), "poll page limit")?;
                if let Some(after) = v.after_offer {
                    id(after)?;
                }
            }
            Self::PollPage(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                require(
                    v.server_time_ms > 0 && v.offers.len() <= 8 && v.cancellations.len() <= 32,
                    "poll page bound",
                )?;
                if let Some(after) = v.next_after {
                    id(after)?;
                }
                let mut seen = std::collections::BTreeSet::new();
                for offer in &v.offers {
                    offer.validate()?;
                    scope(&v.identity, &offer.offer.binding)?;
                    require(
                        offer.clock.server_time_ms == v.server_time_ms
                            && seen.insert(offer.offer.binding.offer_id),
                        "poll delivery clock/duplicate offer",
                    )?;
                }
                for cancellation in &v.cancellations {
                    id(cancellation.cancellation_id)?;
                    scope(&v.identity, &cancellation.binding)?;
                    revision(cancellation.authority_revision)?;
                }
            }
            Self::Accept(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                v.acceptance
                    .validate()
                    .map_err(|_| Invalid("profiled acceptance"))?;
                scope(&v.identity, &v.acceptance.binding)?;
                v.reservation.validate()?;
                id(v.guardian_id)?;
                id(v.launch_id)?;
                revision(v.journal_revision)?;
                require(
                    (1..=120_000).contains(&v.reservation_remaining_ms),
                    "local reservation remaining",
                )?;
            }
            Self::PreparationPlan(v) => v.validate()?,
            Self::Prepare(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                scope(&v.identity, &v.binding)?;
                v.reservation.validate()?;
                id(v.guardian_id)?;
                id(v.launch_id)?;
                revision(v.journal_revision)?;
                require(
                    v.artifacts.len() <= 128 && !v.materials.is_empty() && v.materials.len() <= 16,
                    "preparation receipt bound",
                )?;
                let mut seen = std::collections::BTreeSet::new();
                for receipt in &v.artifacts {
                    artifact(&receipt.artifact)?;
                    require(
                        receipt.digest == receipt.artifact.digest
                            && receipt.bytes > 0
                            && receipt.bytes <= 1_073_741_824
                            && seen.insert(receipt.artifact.digest.clone()),
                        "artifact preparation receipt",
                    )?;
                }
                let mut leases = std::collections::BTreeSet::new();
                for receipt in &v.materials {
                    require(
                        receipt.schema_version == 1
                            && receipt.received_at_ms > 0
                            && receipt.binding == v.binding
                            && leases.insert(receipt.lease_id),
                        "material preparation receipt",
                    )?;
                    id(receipt.operation_id)?;
                    id(receipt.lease_id)?;
                    artifact(&receipt.private_binding)?;
                }
            }
            Self::Grant(v) => v.validate()?,
            Self::Renew(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                scope(&v.identity, &v.binding)?;
                id(v.grant_id)?;
                id(v.reservation_id)?;
                id(v.guardian_id)?;
                id(v.launch_id)?;
                revision(v.journal_revision)?;
            }
            Self::MaterialFetch(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                scope(&v.identity, &v.binding)?;
                id(v.lease_id)?;
            }
            Self::MaterialDelivery(v) => {
                v.lease.validate()?;
                v.clock.validate()?;
                require(
                    v.clock.expires_at_ms == v.lease.expires_at_ms
                        && v.lease.issued_at_ms <= v.clock.server_time_ms,
                    "material delivery clock",
                )?;
            }
            Self::ArtifactFetch(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                scope(&v.identity, &v.binding)?;
                artifact(&v.artifact)?;
                require(
                    (1..=262_144).contains(&v.length)
                        && v.offset
                            .checked_add(u64::from(v.length))
                            .is_some_and(|end| end <= 1_073_741_824),
                    "artifact bounded range",
                )?;
            }
            Self::StopEvidence(v) => v.validate()?,
            Self::Result(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                scope(&v.identity, &v.binding)?;
                id(v.report_id)?;
                if let Some(previous) = v.supersedes {
                    id(previous)?;
                    require(previous != v.report_id, "result supersedes self")?;
                }
                v.stop.validate()?;
                require(v.stop.binding == v.binding, "result stop binding")?;
                ids(&v.source_receipt_ids, 64)?;
                ids(&v.pending_effect_ids, 64)?;
            }
            Self::Reconcile(v) => {
                header(v.schema_version, v.operation_id, &v.identity)?;
                require(
                    (1..=32).contains(&v.limit) && v.attempts.len() <= 32,
                    "reconcile page bound",
                )?;
                if let Some(after) = v.after_attempt {
                    id(after)?;
                }
                let mut seen = std::collections::BTreeSet::new();
                for attempt in &v.attempts {
                    scope(&v.identity, &attempt.binding)?;
                    id(attempt.guardian_id)?;
                    id(attempt.launch_id)?;
                    if let Some(grant) = attempt.last_grant_id {
                        id(grant)?;
                    }
                    if let JournalEvidence::Recorded { revision: r, .. } = attempt.journal {
                        revision(r)?;
                    }
                    require(
                        seen.insert(attempt.binding.attempt_id),
                        "duplicate reconciliation attempt",
                    )?;
                }
            }
        }
        require(
            serde_json::to_vec(self)
                .map_err(|_| Invalid("transport encoding"))?
                .len()
                <= MAX_FRAME_BYTES,
            "transport frame bound",
        )
    }
}
pub fn decode_transport(bytes: &[u8]) -> Result<TransportDocument> {
    require(bytes.len() <= MAX_FRAME_BYTES, "transport frame bound")?;
    let result: TransportDocument =
        serde_json::from_slice(bytes).map_err(|_| Invalid("transport JSON shape"))?;
    result.validate()?;
    Ok(result)
}
/// Digest-only conformance ledger, not a physical stop verifier. The actual
/// daemon must authenticate the assigned qualified host owner and verify central
/// source/effect receipts before releasing anything. Unknown never qualifies.
#[derive(Default)]
pub struct StopJournalLedger {
    records: std::collections::BTreeMap<(Digest, Uuid, Uuid, Uuid), Digest>,
    heads: std::collections::BTreeMap<(Digest, Uuid, Uuid), (u64, Digest)>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalAdmission {
    Recorded,
    Replay,
}
impl StopJournalLedger {
    pub fn admit(&mut self, evidence: &WorkerStopEvidence) -> Result<JournalAdmission> {
        evidence.validate()?;
        let binding = aq_session_protocol::canonical_digest(&evidence.binding)
            .map_err(|_| Invalid("stop binding digest"))?;
        let mut value = serde_json::to_value(evidence).map_err(|_| Invalid("stop encoding"))?;
        value.sort_all_objects();
        let digest = Digest::of(&serde_json::to_vec(&value).map_err(|_| Invalid("stop encoding"))?);
        let subject = (binding, evidence.guardian_id, evidence.launch_id);
        let key = (
            subject.0.clone(),
            subject.1,
            subject.2,
            evidence.stop_operation_id,
        );
        if let Some(old) = self.records.get(&key) {
            require(
                old == &digest,
                "stop operation immutable conflict; refuse/quarantine",
            )?;
            return Ok(JournalAdmission::Replay);
        }
        require(
            self.records.len() < 128,
            "stop journal record retention bound",
        )?;
        if let JournalEvidence::Recorded {
            revision,
            digest: journal,
        } = &evidence.journal
        {
            if let Some((old_revision, old_digest)) = self.heads.get(&subject) {
                require(
                    *revision > *old_revision
                        || (*revision == *old_revision && journal == old_digest),
                    "journal revision/digest conflict",
                )?;
            }
            self.heads.insert(subject, (*revision, journal.clone()));
        }
        self.records.insert(key, digest);
        Ok(JournalAdmission::Recorded)
    }
}
/// Separate additive publication: the original preparation schema is unchanged.
pub fn transport_schema() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(TransportDocument)).expect("schema");
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(o) => {
                if o.get("type") == Some(&serde_json::json!("string")) {
                    o.entry("maxLength").or_insert(4096.into());
                    o.insert("x-utf8-maxBytes".into(), o["maxLength"].clone());
                }
                if o.get("type") == Some(&serde_json::json!("array")) {
                    o.entry("maxItems").or_insert(128.into());
                }
                for child in o.values_mut() {
                    walk(child);
                }
            }
            serde_json::Value::Array(a) => {
                for child in a {
                    walk(child);
                }
            }
            _ => {}
        }
    }
    walk(&mut value);
    if let Some(defs) = value
        .get_mut("$defs")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(d) = defs
            .get_mut("Digest")
            .and_then(serde_json::Value::as_object_mut)
        {
            d.insert("pattern".into(), "^[0-9a-f]{64}$".into());
            d.insert("minLength".into(), 64.into());
            d.insert("maxLength".into(), 64.into());
            d.insert("x-utf8-maxBytes".into(), 64.into());
        }
        if let Some(label) = defs
            .get_mut("Label")
            .and_then(serde_json::Value::as_object_mut)
        {
            label.insert("maxLength".into(), 512.into());
            label.insert("x-utf8-maxBytes".into(), 512.into());
        }
        for (definition, body) in defs.iter_mut() {
            if let Some(properties) = body
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
            {
                for (name, schema) in properties {
                    if let Some(s) = schema.as_object_mut() {
                        if name == "schema_version" {
                            s.insert("const".into(), 1.into());
                        }
                        if name.ends_with("_revision")
                            || name == "revision"
                            || name == "inventory_sequence"
                        {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), i64::MAX.into());
                        }
                        if name.ends_with("_time_ms") || name == "expires_at_ms" {
                            s.insert("minimum".into(), 1.into());
                        }
                        if matches!(
                            name.as_str(),
                            "valid_for_ms" | "reservation_ttl_ms" | "reservation_remaining_ms"
                        ) {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), 120_000.into());
                        }
                        let bound = match name.as_str() {
                            "offers" => Some(8),
                            "cancellations" | "attempts" => Some(32),
                            "materials" => Some(16),
                            "capabilities" | "source_receipt_ids" | "pending_effect_ids" => {
                                Some(64)
                            }
                            "features" | "required_features" => Some(4),
                            _ => None,
                        };
                        if let Some(bound) = bound {
                            s.insert("maxItems".into(), bound.into());
                        }
                        if name == "limit" {
                            s.insert("minimum".into(), 1.into());
                            s.insert(
                                "maximum".into(),
                                if definition == "Poll" { 8 } else { 32 }.into(),
                            );
                        }
                        if name == "length" {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), 262_144.into());
                        }
                        if name == "bytes" {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), 1_073_741_824u64.into());
                        }
                        if definition == "ArtifactRef" && matches!(name.as_str(), "id" | "version")
                        {
                            s.insert("maxLength".into(), 512.into());
                            s.insert("x-utf8-maxBytes".into(), 512.into());
                        }
                    }
                }
            }
        }
    }
    value
}
