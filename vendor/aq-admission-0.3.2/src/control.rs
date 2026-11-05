//! Native control delivery, separate from immutable operation receipts. Pure
//! validation proves neither authorization nor actual installed material/hosts.
use crate::*;
use startup::NativeOffer;
use transport::{
    Accept, DeliveryClock, GrantedLaunch, Poll, PreparationPlan, Prepare, Reconcile, Renew,
    TransportDocument, WorkerIdentity,
};
pub const MAX_REPLY_BYTES: usize = 4_194_304;
fn bounded(value: &impl Serialize) -> Result<()> {
    require(
        serde_json::to_vec(value)
            .map_err(|_| Invalid("control encoding"))?
            .len()
            <= MAX_REPLY_BYTES,
        "control reply byte bound",
    )
}
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(
        serde_json::to_value(a).map_err(|_| Invalid("control encoding"))?
            == serde_json::to_value(b).map_err(|_| Invalid("control encoding"))?,
    )
}
fn scope(identity: &WorkerIdentity, binding: &LaunchBinding) -> Result<()> {
    identity.validate()?;
    binding.validate().map_err(|_| Invalid("control binding"))?;
    require(
        matches!(binding.session,SessionIdentity::Remote {node_id,boot_id,..} if node_id==identity.node_id && boot_id==identity.boot_id),
        "control node/boot scope",
    )
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Versions {
    pub record_revision: u64,
    pub authority_revision: u64,
    pub grant_revision: u64,
}
impl Versions {
    pub fn validate(&self) -> Result<()> {
        revision(self.record_revision)?;
        revision(self.authority_revision)?;
        require(
            self.grant_revision <= self.authority_revision
                && self.authority_revision <= self.record_revision,
            "authority revision order",
        )
    }
    pub fn validate_after(&self, previous: &Self) -> Result<()> {
        self.validate()?;
        previous.validate()?;
        require(
            (self.grant_revision == previous.grant_revision
                || self.authority_revision > previous.authority_revision)
                && (self.authority_revision == previous.authority_revision
                    || self.record_revision > previous.record_revision),
            "derived revision transition",
        )?;
        require(
            self.record_revision >= previous.record_revision
                && self.authority_revision >= previous.authority_revision
                && self.grant_revision >= previous.grant_revision,
            "authority revision regression",
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveryStamp {
    /// Echo of the UUID aq-delivery-nonce on this physical HTTP transmission.
    /// This is outside the immutable mutation request/receipt digest.
    pub nonce: Uuid,
    pub clock: DeliveryClock,
}
impl DeliveryStamp {
    pub fn validate_for(&self, nonce: Uuid, expires_at_ms: u64) -> Result<()> {
        id(nonce)?;
        id(self.nonce)?;
        self.clock.validate()?;
        revision(self.clock.server_time_ms)?;
        revision(self.clock.expires_at_ms)?;
        require(
            self.nonce == nonce && self.clock.expires_at_ms == expires_at_ms,
            "delivery nonce/expiry",
        )
    }
    /// Charge this nonce's send→receive interval. An earlier known local
    /// terminal deadline for this SAME authority is preserved. For a new
    /// renewal revision use RenewedAuthority::client_renewal_deadline_ns: the
    /// prior permit must still be live, but its old expiry does not cap extension.
    pub fn client_deadline_ns(
        &self,
        sent_ns: u64,
        received_ns: u64,
        known_terminal_ns: Option<u64>,
    ) -> Result<u64> {
        self.clock.validate()?;
        id(self.nonce)?;
        let deadline = self.clock.client_deadline_ns(sent_ns, received_ns)?;
        if let Some(terminal) = known_terminal_ns {
            require(received_ns < terminal, "local terminal deadline expired")?;
            Ok(deadline.min(terminal))
        } else {
            Ok(deadline)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PreparationDelivery {
    pub schema_version: u32,
    pub original: PreparationPlan,
    pub versions: Versions,
    pub delivery: DeliveryStamp,
}
impl PreparationDelivery {
    pub fn validate_for(&self, request: &Accept, nonce: Uuid) -> Result<()> {
        TransportDocument::Accept(request.clone()).validate()?;
        self.original.validate()?;
        self.versions.validate()?;
        require(
            self.schema_version == 1 && self.versions.grant_revision == 0,
            "preparation delivery version/phase",
        )?;
        let p = &self.original;
        require(
            p.operation_id == request.operation_id
                && p.binding == request.acceptance.binding
                && p.guardian_id == request.guardian_id
                && p.launch_id == request.launch_id
                && same(&p.reservation, &request.reservation)?,
            "original preparation correlation",
        )?;
        self.delivery.validate_for(nonce, p.clock.expires_at_ms)?;
        require(
            self.delivery.clock.server_time_ms >= p.clock.server_time_ms,
            "historical preparation clock",
        )?;
        bounded(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitialGrantDelivery {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub original: GrantedLaunch,
    pub execution_deadline_ms: u64,
    pub versions: Versions,
    pub delivery: DeliveryStamp,
}
impl InitialGrantDelivery {
    pub fn validate_for(
        &self,
        request: &Prepare,
        plan: &PreparationPlan,
        offer: &NativeOffer,
        nonce: Uuid,
    ) -> Result<()> {
        TransportDocument::Prepare(request.clone()).validate()?;
        plan.validate()?;
        offer.validate()?;
        self.original.validate()?;
        self.versions.validate()?;
        let g = &self.original;
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.versions.grant_revision > 0,
            "initial grant phase/version",
        )?;
        require(
            g.grant.binding == request.binding
                && g.grant.binding == plan.binding
                && g.grant.binding == offer.offered.offer.binding
                && g.guardian_id == request.guardian_id
                && g.guardian_id == plan.guardian_id
                && g.launch_id == request.launch_id
                && g.launch_id == plan.launch_id
                && g.reservation_id == request.reservation.reservation_id
                && same(&request.reservation, &plan.reservation)?
                && same(&g.materials, &plan.materials)?,
            "original grant correlation",
        )?;
        require(
            offer.offered.offer.manifest.credentials.len() == g.materials.len()
                && offer
                    .offered
                    .offer
                    .manifest
                    .credentials
                    .iter()
                    .all(|secret| {
                        g.materials.iter().any(|lease| {
                            secret.credential_profile == lease.credential_profile
                                && secret.purpose.as_str() == lease.purpose.as_str()
                        })
                    }),
            "complete manifest credential coverage",
        )?;
        require(
            g.materials.iter().all(|lease| {
                lease.provider.as_str() == offer.offered.offer.manifest.model.provider.as_str()
                    && offer
                        .offered
                        .offer
                        .manifest
                        .credentials
                        .iter()
                        .any(|secret| {
                            secret.credential_profile == lease.credential_profile
                                && secret.purpose.as_str() == lease.purpose.as_str()
                        })
            }),
            "material profile/provider/purpose matches manifest",
        )?;
        require(
            g.materials.len() == request.materials.len()
                && g.materials.iter().all(|lease| {
                    request.materials.iter().any(|receipt| {
                        receipt.lease_id == lease.lease_id
                            && receipt.binding == lease.binding
                            && receipt.material_digest == lease.material_digest
                    })
                }),
            "original installed material receipts",
        )?;
        revision(self.execution_deadline_ms)?;
        require(
            g.grant.expires_at_ms <= plan.clock.expires_at_ms
                && g.grant.expires_at_ms <= self.execution_deadline_ms
                && g.clock
                    .server_time_ms
                    .checked_add(offer.offered.offer.manifest.resources.wall_time_ms)
                    .is_some_and(|upper| self.execution_deadline_ms <= upper),
            "initial hard execution deadline",
        )?;
        self.delivery.validate_for(nonce, g.grant.expires_at_ms)?;
        require(
            self.delivery.clock.server_time_ms >= g.clock.server_time_ms,
            "historical initial grant clock",
        )?;
        bounded(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenewedAuthority {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub reservation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub journal_revision: u64,
    pub execution_deadline_ms: u64,
    pub versions: Versions,
    pub delivery: DeliveryStamp,
}
impl RenewedAuthority {
    pub fn validate_for(
        &self,
        request: &Renew,
        initial: &InitialGrantDelivery,
        nonce: Uuid,
    ) -> Result<()> {
        TransportDocument::Renew(request.clone()).validate()?;
        ControlReply::Renewed(self.clone()).validate_shape()?;
        ControlReply::InitialGrant(initial.clone()).validate_shape()?;
        self.versions.validate_after(&initial.versions)?;
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.binding == request.binding
                && self.binding == initial.original.grant.binding
                && self.grant_id == request.grant_id
                && self.grant_id == initial.original.grant.grant_id
                && self.reservation_id == request.reservation_id
                && self.reservation_id == initial.original.reservation_id
                && self.guardian_id == request.guardian_id
                && self.guardian_id == initial.original.guardian_id
                && self.launch_id == request.launch_id
                && self.launch_id == initial.original.launch_id
                && self.journal_revision == request.journal_revision
                && self.execution_deadline_ms == initial.execution_deadline_ms
                && self.versions.grant_revision > 0,
            "renewal exact original authority",
        )?;
        self.delivery
            .validate_for(nonce, self.delivery.clock.expires_at_ms)?;
        if self.versions.grant_revision == initial.versions.grant_revision {
            require(
                self.delivery.clock.expires_at_ms == initial.original.grant.expires_at_ms,
                "same initial revision changed grant expiry",
            )?;
        }
        require(
            self.delivery.clock.expires_at_ms <= self.execution_deadline_ms
                && self.delivery.clock.server_time_ms >= initial.original.clock.server_time_ms,
            "renewal immutable hard deadline",
        )?;
        bounded(self)
    }
    /// A new revision may extend a still-live prior permit, bounded by the
    /// immutable local hard deadline. A retransmission of the same revision
    /// retains its earlier local deadline. Neither path resurrects expiry.
    pub fn client_renewal_deadline_ns(
        &self,
        sent_ns: u64,
        received_ns: u64,
        prior_grant_deadline_ns: u64,
        hard_deadline_ns: u64,
        prior_grant_revision: u64,
    ) -> Result<u64> {
        require(
            received_ns < prior_grant_deadline_ns && received_ns < hard_deadline_ns,
            "prior renewal permit expired",
        )?;
        require(
            self.versions.grant_revision >= prior_grant_revision,
            "local renewal revision regression",
        )?;
        let cap = if self.versions.grant_revision == prior_grant_revision {
            prior_grant_deadline_ns.min(hard_deadline_ns)
        } else {
            hard_deadline_ns
        };
        self.delivery
            .client_deadline_ns(sent_ns, received_ns, Some(cap))
    }
    /// Stateful consumer fence. Delivery stamps differ on retransmission; the
    /// same grant revision cannot change its absolute expiry or authority.
    pub fn validate_after(&self, previous: &Self) -> Result<()> {
        self.versions.validate_after(&previous.versions)?;
        require(
            self.binding == previous.binding
                && self.grant_id == previous.grant_id
                && self.reservation_id == previous.reservation_id
                && self.guardian_id == previous.guardian_id
                && self.launch_id == previous.launch_id
                && self.execution_deadline_ms == previous.execution_deadline_ms
                && self.journal_revision >= previous.journal_revision,
            "renewal owner/journal regression",
        )?;
        if self.versions.grant_revision == previous.versions.grant_revision {
            require(
                self.delivery.clock.expires_at_ms == previous.delivery.clock.expires_at_ms,
                "same revision changed grant expiry",
            )?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NativePollPage {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub server_time_ms: u64,
    #[schemars(length(max = 8))]
    pub offers: Vec<NativeOffer>,
    #[schemars(length(max = 32))]
    pub cancellations: Vec<transport::Cancellation>,
    pub next_after: Option<Uuid>,
}
impl NativePollPage {
    pub fn validate_for(&self, request: &Poll) -> Result<()> {
        TransportDocument::Poll(request.clone()).validate()?;
        revision(self.server_time_ms)?;
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.identity == request.identity
                && self.server_time_ms > 0
                && self.offers.len() <= request.limit as usize
                && self.offers.len() <= 8
                && self.cancellations.len() <= 32,
            "native poll correlation/bound",
        )?;
        if let Some(next) = self.next_after {
            id(next)?;
        }
        let mut attempts = std::collections::BTreeSet::new();
        let mut offers = std::collections::BTreeSet::new();
        for offer in &self.offers {
            offer.validate()?;
            scope(&self.identity, &offer.offered.offer.binding)?;
            require(
                offer.offered.clock.server_time_ms == self.server_time_ms
                    && attempts.insert(offer.offered.offer.binding.attempt_id)
                    && offers.insert(offer.offered.offer.binding.offer_id),
                "native poll duplicate/clock",
            )?;
        }
        let mut cancellations = std::collections::BTreeSet::new();
        for cancel in &self.cancellations {
            id(cancel.cancellation_id)?;
            revision(cancel.authority_revision)?;
            scope(&self.identity, &cancel.binding)?;
            require(
                attempts.insert(cancel.binding.attempt_id)
                    && cancellations.insert(cancel.cancellation_id),
                "contradictory poll cancellation",
            )?;
        }
        bounded(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchOwner {
    pub reservation_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
}
impl LaunchOwner {
    fn validate(&self) -> Result<()> {
        id(self.reservation_id)?;
        id(self.guardian_id)?;
        id(self.launch_id)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    AuthenticationRequired,
    InvalidRequest,
    UnknownOrNotOwned,
    Conflict,
    Expired,
    Cancelled,
    InputsChanged,
    CredentialRevoked,
    RecoveryRequired,
    Unavailable,
    Capacity,
}
/// An error is never a live authority stamp or another session's metadata.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub schema_version: u32,
    pub code: FailureCode,
    pub operation_id: Option<Uuid>,
}
impl Failure {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "control failure version")?;
        if let Some(operation) = self.operation_id {
            id(operation)?;
        }
        Ok(())
    }
}
/// Read-only current state. In particular Granted has no DeliveryClock and
/// cannot be supplied as a launch/renew permit. Actual owners resolve receipts.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CurrentPhase {
    Offered {
        expires_at_ms: u64,
    },
    Preparing {
        owner: LaunchOwner,
        preparation_operation_id: Uuid,
        preparation_digest: Digest,
        expires_at_ms: u64,
    },
    Granted {
        owner: LaunchOwner,
        grant_id: Uuid,
        expires_at_ms: u64,
        execution_deadline_ms: u64,
        started_operation_id: Option<Uuid>,
    },
    StopRequired {
        owner: Option<LaunchOwner>,
        last_grant_id: Option<Uuid>,
        execution_deadline_ms: Option<u64>,
        reason: FailureCode,
    },
    Stopped {
        owner: LaunchOwner,
        grant_id: Uuid,
        stopped_receipt: ArtifactRef,
    },
    NeverGranted {
        closure: ArtifactRef,
    },
    Released {
        release: ArtifactRef,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CurrentAuthority {
    pub binding: LaunchBinding,
    pub versions: Versions,
    pub phase: CurrentPhase,
}
impl CurrentAuthority {
    pub fn validate(&self, now: u64) -> Result<()> {
        self.binding
            .validate()
            .map_err(|_| Invalid("current authority binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "worker current authority",
        )?;
        self.versions.validate()?;
        revision(now)?;
        match &self.phase {
            CurrentPhase::Offered { expires_at_ms } => {
                revision(*expires_at_ms)?;
                require(
                    *expires_at_ms > now && self.versions.grant_revision == 0,
                    "current offered expiry/phase",
                )?;
            }
            CurrentPhase::Preparing {
                owner,
                preparation_operation_id,
                expires_at_ms,
                ..
            } => {
                owner.validate()?;
                id(*preparation_operation_id)?;
                revision(*expires_at_ms)?;
                require(
                    *expires_at_ms > now && self.versions.grant_revision == 0,
                    "current preparing expiry/phase",
                )?;
            }
            CurrentPhase::Granted {
                owner,
                grant_id,
                expires_at_ms,
                execution_deadline_ms,
                started_operation_id,
            } => {
                owner.validate()?;
                id(*grant_id)?;
                revision(*expires_at_ms)?;
                revision(*execution_deadline_ms)?;
                if let Some(started) = started_operation_id {
                    id(*started)?;
                }
                require(
                    self.versions.grant_revision > 0
                        && *expires_at_ms > now
                        && expires_at_ms <= execution_deadline_ms,
                    "current granted expiry/phase",
                )?;
            }
            CurrentPhase::StopRequired {
                owner,
                last_grant_id,
                execution_deadline_ms,
                ..
            } => {
                if let Some(owner) = owner {
                    owner.validate()?;
                }
                if let Some(grant) = last_grant_id {
                    id(*grant)?;
                    require(
                        self.versions.grant_revision > 0,
                        "stop required grant phase",
                    )?;
                }
                if let Some(deadline) = execution_deadline_ms {
                    revision(*deadline)?;
                }
                require(
                    last_grant_id.is_some() == execution_deadline_ms.is_some()
                        && last_grant_id.is_some() == (self.versions.grant_revision > 0)
                        && (last_grant_id.is_none() || owner.is_some()),
                    "stop required grant/deadline/owner",
                )?;
            }
            CurrentPhase::Stopped {
                owner,
                grant_id,
                stopped_receipt,
            } => {
                owner.validate()?;
                id(*grant_id)?;
                artifact(stopped_receipt)?;
                require(
                    self.versions.grant_revision > 0
                        && stopped_receipt.id.as_str()
                            == format!("aq.native-stopped/{}", self.binding.attempt_id)
                        && stopped_receipt.version.as_str() == "1",
                    "current stopped owner",
                )?;
            }
            CurrentPhase::NeverGranted { closure } => {
                artifact(closure)?;
                require(self.versions.grant_revision == 0, "never granted revision")?;
            }
            CurrentPhase::Released { release } => {
                artifact(release)?;
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconcileEntry {
    Current { authority: Box<CurrentAuthority> },
    Refused { attempt_id: Uuid, code: FailureCode },
}
impl ReconcileEntry {
    fn attempt_id(&self) -> Uuid {
        match self {
            Self::Current { authority } => authority.binding.attempt_id,
            Self::Refused { attempt_id, .. } => *attempt_id,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconcilePage {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub delivery_nonce: Uuid,
    pub server_time_ms: u64,
    #[schemars(length(max = 32))]
    pub attempts: Vec<ReconcileEntry>,
    pub next_after: Option<Uuid>,
}
impl ReconcilePage {
    pub fn validate_for(&self, request: &Reconcile, nonce: Uuid) -> Result<()> {
        TransportDocument::Reconcile(request.clone()).validate()?;
        id(nonce)?;
        id(self.delivery_nonce)?;
        revision(self.server_time_ms)?;
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.identity == request.identity
                && self.delivery_nonce == nonce
                && self.attempts.len() <= 32
                && self.attempts.len() <= request.limit as usize,
            "reconcile delivery correlation/bound",
        )?;
        if let Some(next) = self.next_after {
            id(next)?;
        }
        // Explicit request batches are complete. Empty batches discover only
        // authenticated owned attempts using the bounded stable cursor.
        if !request.attempts.is_empty() {
            require(
                request.attempts.len() <= request.limit as usize
                    && self.attempts.len() == request.attempts.len()
                    && self.next_after.is_none(),
                "complete explicit reconciliation batch",
            )?;
        }
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.attempts {
            let attempt = entry.attempt_id();
            id(attempt)?;
            require(seen.insert(attempt), "duplicate reconcile attempt")?;
            let requested = request
                .attempts
                .iter()
                .find(|a| a.binding.attempt_id == attempt);
            if !request.attempts.is_empty() {
                require(requested.is_some(), "unrequested reconciliation result")?;
            }
            match entry {
                ReconcileEntry::Current { authority } => {
                    authority.validate(self.server_time_ms)?;
                    scope(&self.identity, &authority.binding)?;
                    if let Some(expected) = requested {
                        require(
                            authority.binding == expected.binding,
                            "reconciliation exact requested binding",
                        )?;
                    }
                }
                ReconcileEntry::Refused { .. } => require(
                    requested.is_some(),
                    "refusal requires caller-listed attempt",
                )?,
            }
        }
        bounded(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ControlReply {
    Poll(NativePollPage),
    Preparation(PreparationDelivery),
    InitialGrant(InitialGrantDelivery),
    Renewed(RenewedAuthority),
    Reconciled(ReconcilePage),
    Failure(Failure),
}
/// Typed parse rejects duplicate/unknown fields before request-dependent
/// validation. The caller must select the expected variant and validator.
pub fn decode_reply(bytes: &[u8]) -> Result<ControlReply> {
    require(bytes.len() <= MAX_REPLY_BYTES, "control frame byte bound")?;
    let reply: ControlReply =
        serde_json::from_slice(bytes).map_err(|_| Invalid("control reply JSON shape"))?;
    reply.validate_shape()?;
    Ok(reply)
}

impl ControlReply {
    pub fn validate_shape(&self) -> Result<()> {
        match self {
            Self::Preparation(v) => {
                require(
                    v.schema_version == 1 && v.versions.grant_revision == 0,
                    "preparation delivery phase",
                )?;
                v.original.validate()?;
                v.versions.validate()?;
                v.delivery
                    .validate_for(v.delivery.nonce, v.original.clock.expires_at_ms)?;
                require(
                    v.delivery.clock.server_time_ms >= v.original.clock.server_time_ms,
                    "preparation delivery time",
                )?;
            }
            Self::InitialGrant(v) => {
                require(
                    v.schema_version == 1 && v.versions.grant_revision > 0,
                    "initial delivery phase",
                )?;
                id(v.operation_id)?;
                v.original.validate()?;
                v.versions.validate()?;
                revision(v.execution_deadline_ms)?;
                v.delivery
                    .validate_for(v.delivery.nonce, v.original.grant.expires_at_ms)?;
                require(
                    v.original.grant.expires_at_ms <= v.execution_deadline_ms
                        && v.delivery.clock.server_time_ms >= v.original.clock.server_time_ms,
                    "initial delivery time/hard deadline",
                )?;
            }
            Self::Renewed(v) => {
                require(
                    v.schema_version == 1 && v.versions.grant_revision > 0,
                    "renewed phase",
                )?;
                v.binding
                    .validate()
                    .map_err(|_| Invalid("renewal binding"))?;
                require(
                    matches!(v.binding.session, SessionIdentity::Remote { .. }),
                    "worker renewed authority",
                )?;
                for value in [
                    v.operation_id,
                    v.grant_id,
                    v.reservation_id,
                    v.guardian_id,
                    v.launch_id,
                ] {
                    id(value)?;
                }
                revision(v.journal_revision)?;
                revision(v.execution_deadline_ms)?;
                v.versions.validate()?;
                v.delivery
                    .validate_for(v.delivery.nonce, v.delivery.clock.expires_at_ms)?;
                require(
                    v.delivery.clock.expires_at_ms <= v.execution_deadline_ms,
                    "renewal hard deadline",
                )?;
            }
            Self::Poll(v) => {
                require(
                    v.schema_version == 1 && v.offers.len() <= 8 && v.cancellations.len() <= 32,
                    "native poll bound",
                )?;
                id(v.operation_id)?;
                v.identity.validate()?;
                revision(v.server_time_ms)?;
                if let Some(next) = v.next_after {
                    id(next)?;
                }
                let mut seen = std::collections::BTreeSet::new();
                for offer in &v.offers {
                    offer.validate()?;
                    scope(&v.identity, &offer.offered.offer.binding)?;
                    require(
                        offer.offered.clock.server_time_ms == v.server_time_ms
                            && seen.insert(offer.offered.offer.binding.attempt_id),
                        "poll clock/duplicate",
                    )?;
                }
                let mut ids = std::collections::BTreeSet::new();
                for cancellation in &v.cancellations {
                    id(cancellation.cancellation_id)?;
                    revision(cancellation.authority_revision)?;
                    scope(&v.identity, &cancellation.binding)?;
                    require(
                        seen.insert(cancellation.binding.attempt_id)
                            && ids.insert(cancellation.cancellation_id),
                        "poll cancellation conflict",
                    )?;
                }
            }
            Self::Reconciled(v) => {
                require(
                    v.schema_version == 1 && v.attempts.len() <= 32,
                    "reconcile bound",
                )?;
                id(v.operation_id)?;
                id(v.delivery_nonce)?;
                v.identity.validate()?;
                revision(v.server_time_ms)?;
                if let Some(next) = v.next_after {
                    id(next)?;
                }
                let mut seen = std::collections::BTreeSet::new();
                for entry in &v.attempts {
                    id(entry.attempt_id())?;
                    require(seen.insert(entry.attempt_id()), "duplicate reconcile entry")?;
                    if let ReconcileEntry::Current { authority } = entry {
                        authority.validate(v.server_time_ms)?;
                        scope(&v.identity, &authority.binding)?;
                    }
                }
            }
            Self::Failure(v) => v.validate()?,
        }
        bounded(self)
    }
}

pub fn reply_schema() -> serde_json::Value {
    let mut schema =
        serde_json::to_value(schemars::schema_for!(ControlReply)).expect("control reply schema");
    fn bound(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("type") == Some(&serde_json::json!("string")) {
                    map.entry("maxLength").or_insert(4096.into());
                    map.insert("x-utf8-maxBytes".into(), map["maxLength"].clone());
                }
                if map.get("type") == Some(&serde_json::json!("array")) {
                    map.entry("maxItems").or_insert(128.into());
                }
                for child in map.values_mut() {
                    bound(child);
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    bound(child);
                }
            }
            _ => {}
        }
    }
    bound(&mut schema);
    if let Some(digest) = schema
        .get_mut("$defs")
        .and_then(|v| v.get_mut("Digest"))
        .and_then(serde_json::Value::as_object_mut)
    {
        digest.insert("minLength".into(), 64.into());
        digest.insert("maxLength".into(), 64.into());
        digest.insert("x-utf8-maxBytes".into(), 64.into());
        digest.insert("pattern".into(), "^[0-9a-f]{64}$".into());
    }
    schema["$comment"] = serde_json::json!(
        "Maximum wire body4MiB. Typed validation additionally checks all nonnil IDs, exact request/nonce/node/boot/binding/owner correlation, immutable nested0.1.6 payload, checked current/hard expiry, revision order and same-revision equality, actual material receipt set, complete explicit reconciliation or bounded owned discovery, and actual encoded byte bounds. aq-delivery-nonce belongs to one physical HTTP send and is outside the immutable mutation digest; current-state observations have no live authority clock. Actual authorization, installed-binding/fingerprint freshness, source/effect/receipt ownership and host state remain daemon/qualified-worker checks. Required nested frozen cardinality and artifact byte bounds are enforced in Rust as documented by the original publications."
    );
    schema
}
