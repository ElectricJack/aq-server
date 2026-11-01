//! Separately bounded proposal, accepted preparation and execution observations.
//! Pure correlation/arithmetic only: no DTO, fact status or successful validation
//! authenticates an owner, proves resources/files, or grants live permission.
mod facts;
use crate::*;
use control::{DeliveryStamp, Versions};
use execution_spec::ProfiledExecutionSpec;
pub use facts::*;
use serde_json::Value;
use startup::NativeOffer;
use std::collections::BTreeSet;
use transport::{Accept, NoLaunchReservation, Prepare, WorkerIdentity};
pub const MAX_BYTES: usize = 131072;
pub const MAX_PREPARATION_MS: u64 = 120000;
fn same(a: &impl Serialize, b: &impl Serialize) -> Result<bool> {
    Ok(
        serde_json::to_value(a).map_err(|_| Invalid("phase encoding"))?
            == serde_json::to_value(b).map_err(|_| Invalid("phase encoding"))?,
    )
}
fn bytes(v: &impl Serialize) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(v).map_err(|_| Invalid("phase encoding"))?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| Invalid("phase encoding"))
}
fn hash(domain: &str, v: &impl Serialize) -> Result<Digest> {
    let data = bytes(v)?;
    require(data.len() <= MAX_BYTES, "phase digest byte bound")?;
    let mut out = vec![b'['];
    out.extend(serde_json::to_vec(domain).map_err(|_| Invalid("phase domain encoding"))?);
    out.push(b',');
    out.extend(data);
    out.push(b']');
    Ok(Digest::of(&out))
}
fn bound(v: &impl Serialize) -> Result<()> {
    require(bytes(v)?.len() <= MAX_BYTES, "phase byte bound")
}
fn reference(v: &ArtifactRef) -> Result<()> {
    artifact(v)
}
fn scope(identity: &WorkerIdentity, binding: &LaunchBinding) -> Result<()> {
    identity.validate()?;
    binding.validate().map_err(|_| Invalid("phase binding"))?;
    require(
        matches!(binding.session,SessionIdentity::Remote{node_id,boot_id,..} if node_id==identity.node_id && boot_id==identity.boot_id),
        "phase node/boot scope",
    )
}
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "PreparationV2PhaseFeature")]
pub enum PhaseFeature {
    AcceptedPreparationV2,
    FreshInitialConfirmationV2,
    IndependentExecutionDeliveryV2,
}
pub fn required_features() -> Vec<PhaseFeature> {
    vec![
        PhaseFeature::AcceptedPreparationV2,
        PhaseFeature::FreshInitialConfirmationV2,
        PhaseFeature::IndependentExecutionDeliveryV2,
    ]
}
/// Exact artifact negotiation is separate from frozen legacy feature enums.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2Negotiation")]
pub struct Negotiation {
    pub contract: ArtifactRef,
    #[schemars(length(max = 8))]
    pub features: Vec<PhaseFeature>,
}
impl Negotiation {
    pub fn validate(&self) -> Result<()> {
        require(
            self.contract == contract_artifact()?,
            "unsupported exact phase contract",
        )?;
        require(
            self.features.len() <= 8
                && self.features.iter().copied().collect::<BTreeSet<_>>().len()
                    == self.features.len()
                && required_features()
                    .iter()
                    .all(|f| self.features.contains(f)),
            "unsupported phase feature set",
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2PhasePolicy")]
pub struct PhasePolicy {
    pub schema_version: u32,
    pub maximum_preparation_ms: u64,
    pub maximum_execution_lease_ms: u64,
    pub maximum_observation_ms: u64,
}
impl PhasePolicy {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 2
                && (1..=MAX_PREPARATION_MS).contains(&self.maximum_preparation_ms)
                && (1..=120000).contains(&self.maximum_execution_lease_ms)
                && (1..=30000).contains(&self.maximum_observation_ms),
            "phase policy bounds",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        hash("aq.phase.policy.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2ChoicePins")]
pub struct ChoicePins {
    pub selection_digest: Digest,
    pub registry: ArtifactRef,
    pub principal: ArtifactRef,
    pub permission_digest: Digest,
    pub credential_profile: ArtifactRef,
    pub credential_revision: u64,
    pub account_reference: Text,
    pub endpoint_reference: Text,
    pub model: aq_session_protocol::ModelSettings,
    pub harness: aq_session_protocol::HarnessSettings,
    pub maximum_lineage_elapsed_ms: u64,
}
impl ChoicePins {
    pub fn digest(&self) -> Result<Digest> {
        require(
            self.harness.required_capabilities.len() <= 64,
            "phase choice capability bound",
        )?;
        for a in [
            &self.registry,
            &self.principal,
            &self.credential_profile,
            &self.harness.adapter,
            &self.harness.environment,
        ] {
            reference(a)?;
        }
        revision(self.credential_revision)?;
        revision(self.maximum_lineage_elapsed_ms)?;
        hash("aq.phase.choice.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2OwnerAuthority")]
pub struct OwnerAuthority {
    pub kind: FactKind,
    pub authority: ArtifactRef,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2Proposal")]
pub struct Proposal {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub policy: PhasePolicy,
    pub original: NativeOffer,
    pub spec: ProfiledExecutionSpec,
    pub choice: ChoicePins,
    pub full_context_digest: Digest,
    #[schemars(length(min = 9, max = 9))]
    pub owners: Vec<OwnerAuthority>,
}
impl Proposal {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 2, "phase proposal version")?;
        self.negotiation.validate()?;
        self.policy.validate()?;
        self.original.validate()?;
        self.spec.validate_retained(
            &self.original.offered.offer,
            &self.original.offered.artifacts,
            self.original.offered.clock.server_time_ms,
        )?;
        require(
            self.original.offered.clock.server_time_ms >= self.spec.source.observed_at_ms
                && self.original.offered.clock.expires_at_ms <= self.spec.source.expires_at_ms,
            "phase original source/proposal clocks",
        )?;
        self.choice.digest()?;
        let m = &self.original.offered.offer.manifest;
        require(
            self.choice.permission_digest == Digest::of(&bytes(&m.permissions)?),
            "phase original permission digest",
        )?;
        require(
            self.spec
                .source
                .resolved_object
                .hex
                .bytes()
                .any(|b| b != b'0'),
            "phase nonzero original source",
        )?;
        require(
            self.choice.selection_digest == self.spec.selection_digest
                && self.choice.model == m.model
                && self.choice.harness == m.harness
                && m.credentials.len() == 1
                && m.credentials[0].credential_profile == self.choice.credential_profile,
            "exact phase sealed choice",
        )?;
        let mut kinds = BTreeSet::new();
        require(self.owners.len() == 9, "phase owner set count")?;
        for o in &self.owners {
            reference(&o.authority)?;
            require(kinds.insert(o.kind), "duplicate phase owner")?;
        }
        require(
            kinds == FactKind::all().into_iter().collect(),
            "missing phase owner",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        hash("aq.phase.proposal.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2ResourceHold")]
pub struct ResourceHold {
    /// Explicit newly negotiated no-launch phase duration; the original proposal
    /// remaining time stays historical and is never extended/reinterpreted.
    #[schemars(range(min = 1, max = 120000))]
    pub maximum_preparation_ms: u64,
    pub reservation: NoLaunchReservation,
    pub resources: Resources,
    pub source_profile: ArtifactRef,
    pub source_disk_bytes: u64,
}
impl ResourceHold {
    fn validate(&self, p: &Proposal) -> Result<()> {
        id(self.reservation.reservation_id)?;
        id(self.reservation.reservation_operation_id)?;
        reference(&self.source_profile)?;
        let r = &p.original.offered.offer.manifest.resources;
        for n in [
            self.resources.memory_bytes,
            self.resources.cpu_millis,
            self.resources.disk_bytes,
            self.source_disk_bytes,
        ] {
            revision(n)?;
        }
        require(
            (1..=p.policy.maximum_preparation_ms).contains(&self.maximum_preparation_ms)
                && self.resources.slots == 1
                && self.resources.memory_bytes >= r.memory_bytes
                && self.resources.cpu_millis >= r.cpu_millis
                && self.resources.disk_bytes >= self.source_disk_bytes
                && self.source_disk_bytes > 0,
            "exact bounded resource hold",
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2AcceptRequest")]
pub struct AcceptRequest {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub proposal_digest: Digest,
    pub original: Accept,
    pub hold: ResourceHold,
}
impl AcceptRequest {
    pub fn validate_for(&self, p: &Proposal) -> Result<()> {
        p.validate()?;
        self.negotiation.validate()?;
        require(
            self.schema_version == 2
                && self.proposal_digest == p.digest()?
                && same(&self.negotiation, &p.negotiation)?,
            "phase Accept proposal",
        )?;
        transport::TransportDocument::Accept(self.original.clone()).validate()?;
        self.original
            .acceptance
            .validate_against(&p.original.offered.offer)
            .map_err(|_| Invalid("phase acceptance binding"))?;
        scope(&self.original.identity, &p.original.offered.offer.binding)?;
        self.hold.validate(p)?;
        require(
            same(&self.original.reservation, &self.hold.reservation)?,
            "phase exact hold identity",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        transport::TransportDocument::Accept(self.original.clone()).validate()?;
        self.negotiation.validate()?;
        hash("aq.phase.accept.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2AcceptedPreparationLease")]
pub struct AcceptedPreparationLease {
    pub lease_id: Uuid,
    pub revision: u64,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub hold: ResourceHold,
    pub proposal_digest: Digest,
    pub policy_digest: Digest,
    pub choice_digest: Digest,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    #[schemars(length(min = 1, max = 1))]
    pub materials: Vec<MaterialLease>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2AcceptedReceipt")]
pub struct AcceptedReceipt {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub operation_id: Uuid,
    pub request_digest: Digest,
    pub lease: AcceptedPreparationLease,
    pub versions: Versions,
}
impl AcceptedReceipt {
    pub fn validate_for(&self, r: &AcceptRequest, p: &Proposal) -> Result<()> {
        r.validate_for(p)?;
        self.negotiation.validate()?;
        self.versions.validate()?;
        let l = &self.lease;
        scope(&l.identity, &l.binding)?;
        for v in [self.operation_id, l.lease_id, l.guardian_id, l.launch_id] {
            id(v)?;
        }
        require(
            self.schema_version == 2
                && self.operation_id == r.original.operation_id
                && self.request_digest == r.digest()?
                && same(&self.negotiation, &r.negotiation)?
                && l.revision == 1
                && self.versions.grant_revision == 0,
            "phase accepted receipt identity",
        )?;
        require(
            l.identity == r.original.identity
                && l.binding == r.original.acceptance.binding
                && l.guardian_id == r.original.guardian_id
                && l.launch_id == r.original.launch_id
                && same(&l.hold, &r.hold)?
                && l.proposal_digest == r.proposal_digest
                && l.policy_digest == p.policy.digest()?
                && l.choice_digest == p.choice.digest()?,
            "phase original hold/pins",
        )?;
        require(
            p.original
                .offered
                .artifacts
                .iter()
                .all(|d| l.expires_at_ms <= d.retain_until_ms),
            "phase accepted lease exceeds original input retention",
        )?;
        let proposal = &p.original.offered.clock;
        require(
            l.issued_at_ms >= proposal.server_time_ms
                && l.issued_at_ms < proposal.expires_at_ms
                && l.expires_at_ms > l.issued_at_ms
                && l.expires_at_ms - l.issued_at_ms <= p.policy.maximum_preparation_ms
                && l.expires_at_ms - l.issued_at_ms <= r.hold.maximum_preparation_ms
                && l.expires_at_ms <= i64::MAX as u64,
            "timely Accept independent preparation lease",
        )?;
        require(l.materials.len() == 1, "phase material count")?;
        let m = &l.materials[0];
        m.validate()?;
        require(
            m.binding == l.binding
                && m.credential_profile == p.choice.credential_profile
                && m.credential_revision == p.choice.credential_revision
                && m.provider.as_str() == p.choice.model.provider.as_str()
                && m.account_reference.as_str() == p.choice.account_reference.as_str()
                && m.endpoint_reference.as_str() == p.choice.endpoint_reference.as_str()
                && m.purpose.as_str()
                    == p.original.offered.offer.manifest.credentials[0]
                        .purpose
                        .as_str()
                && m.issued_at_ms <= l.issued_at_ms
                && m.expires_at_ms >= l.expires_at_ms,
            "phase original material scope",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        require(
            self.lease.materials.len() == 1,
            "phase accepted material bound",
        )?;
        self.negotiation.validate()?;
        hash("aq.phase.accepted-receipt.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2AcceptedDelivery")]
pub struct AcceptedDelivery {
    pub schema_version: u32,
    pub receipt: AcceptedReceipt,
    pub delivery: DeliveryStamp,
}
impl AcceptedDelivery {
    pub fn validate_for(&self, r: &AcceptRequest, p: &Proposal, nonce: Uuid) -> Result<()> {
        self.receipt.validate_for(r, p)?;
        require(
            self.schema_version == 2
                && self.delivery.clock.server_time_ms >= self.receipt.lease.issued_at_ms,
            "phase accepted delivery",
        )?;
        self.delivery
            .validate_for(nonce, self.receipt.lease.expires_at_ms)?;
        bound(self)
    }
    /// Arithmetic only; this is not an authenticated preparation permit.
    pub fn sample(
        &self,
        r: &AcceptRequest,
        p: &Proposal,
        nonce: Uuid,
        sent: u64,
        received: u64,
        previous: Option<&DeadlineSample>,
    ) -> Result<DeadlineSample> {
        self.validate_for(r, p, nonce)?;
        sample(
            self.receipt.digest()?,
            self.delivery.clock.client_deadline_ns(sent, received)?,
            self.delivery.clock.client_deadline_ns(sent, received)?,
            received,
            previous,
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2PrepareRequest")]
pub struct PrepareRequest {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub accepted_receipt_digest: Digest,
    pub expected_preparation_revision: u64,
    pub original: Prepare,
}
impl PrepareRequest {
    pub fn validate_for(&self, a: &AcceptedReceipt, r: &AcceptRequest, p: &Proposal) -> Result<()> {
        a.validate_for(r, p)?;
        self.negotiation.validate()?;
        transport::TransportDocument::Prepare(self.original.clone()).validate()?;
        let l = &a.lease;
        let v = &self.original;
        require(
            self.schema_version == 2
                && same(&self.negotiation, &a.negotiation)?
                && self.accepted_receipt_digest == a.digest()?
                && self.expected_preparation_revision == l.revision
                && v.identity == l.identity
                && v.binding == l.binding
                && v.guardian_id == l.guardian_id
                && v.launch_id == l.launch_id
                && same(&v.reservation, &l.hold.reservation)?
                && v.journal_revision > r.original.journal_revision,
            "phase Prepare owner association",
        )?;
        require(v.materials.len() == 1, "phase installed material count")?;
        let m = &v.materials[0];
        m.validate_against(&l.materials[0], m.received_at_ms)?;
        require(
            m.received_at_ms >= l.issued_at_ms
                && m.private_binding.id.as_str() == format!("aq.private-material/{}", m.lease_id)
                && m.private_binding.version.as_str() == "1",
            "phase installed material pin",
        )?;
        let mut identities = BTreeSet::new();
        require(
            v.artifacts
                .iter()
                .all(|x| identities.insert((x.artifact.id.clone(), x.artifact.version.clone()))),
            "phase artifact identity conflict",
        )?;
        let offered = &p.original.offered.artifacts;
        require(
            v.artifacts.len() == offered.len() + 2 && v.artifacts.len() <= 128,
            "phase artifact coverage",
        )?;
        for d in offered {
            require(
                v.artifacts.iter().any(|x| {
                    x.artifact == d.artifact && x.digest == d.artifact.digest && x.bytes == d.bytes
                }),
                "phase original artifact missing",
            )?;
        }
        let extra: Vec<_> = v
            .artifacts
            .iter()
            .filter(|x| !offered.iter().any(|d| d.artifact == x.artifact))
            .collect();
        require(extra.len() == 2, "phase exactly two source extras")?;
        let manifest = extra
            .iter()
            .find(|x| x.artifact.id.as_str().starts_with("aq.source-manifest/"))
            .ok_or(Invalid("phase source manifest extra"))?;
        let transfer = manifest
            .artifact
            .id
            .as_str()
            .strip_prefix("aq.source-manifest/")
            .ok_or(Invalid("phase transfer id"))?;
        let transfer = Uuid::parse_str(transfer).map_err(|_| Invalid("phase transfer id"))?;
        id(transfer)?;
        require(
            manifest.artifact.id.as_str() == format!("aq.source-manifest/{transfer}"),
            "phase canonical manifest identity",
        )?;
        require(
            extra
                .iter()
                .filter(|x| x.artifact.id.as_str() == format!("aq.source-bundle/{transfer}"))
                .count()
                == 1
                && extra.iter().all(|x| x.artifact.version.as_str() == "1"),
            "phase exact linked source extras",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        transport::TransportDocument::Prepare(self.original.clone()).validate()?;
        self.negotiation.validate()?;
        hash("aq.phase.prepare.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2FreshConfirmation")]
pub struct FreshConfirmation {
    pub schema_version: u32,
    pub confirmation_id: Uuid,
    pub prepare_digest: Digest,
    pub accepted_receipt_digest: Digest,
    pub checked_at_ms: u64,
    #[schemars(length(min = 9, max = 9))]
    pub facts: Vec<OwnerFact>,
}
impl FreshConfirmation {
    pub fn validate_for(
        &self,
        request: &PrepareRequest,
        a: &AcceptedReceipt,
        r: &AcceptRequest,
        p: &Proposal,
    ) -> Result<()> {
        request.validate_for(a, r, p)?;
        id(self.confirmation_id)?;
        require(
            self.schema_version == 2
                && self.prepare_digest == request.digest()?
                && self.accepted_receipt_digest == a.digest()?
                && self.checked_at_ms >= a.lease.issued_at_ms
                && self.checked_at_ms < a.lease.expires_at_ms
                && self.facts.len() == 9,
            "phase fresh confirmation scope/time",
        )?;
        let mut kinds = BTreeSet::new();
        for f in &self.facts {
            f.validate_for(p, a, request, self.checked_at_ms)?;
            require(kinds.insert(f.body.value.kind()), "duplicate phase fact")?;
        }
        require(
            kinds == FactKind::all().into_iter().collect(),
            "missing mandatory phase fact",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        require(self.facts.len() == 9, "phase confirmation fact bound")?;
        for f in &self.facts {
            OwnerFact::record_digest(&f.body)?;
        }
        hash("aq.phase.confirmation.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2ExecutionReceipt")]
pub struct ExecutionReceipt {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub operation_id: Uuid,
    pub request_digest: Digest,
    pub accepted_receipt_digest: Digest,
    pub issued_at_ms: u64,
    pub grant: aq_session_protocol::ProfiledGrant,
    pub execution_deadline_ms: u64,
    pub confirmation: FreshConfirmation,
    pub versions: Versions,
}
impl ExecutionReceipt {
    pub fn validate_for(
        &self,
        q: &PrepareRequest,
        a: &AcceptedReceipt,
        r: &AcceptRequest,
        p: &Proposal,
    ) -> Result<()> {
        self.negotiation.validate()?;
        self.confirmation.validate_for(q, a, r, p)?;
        self.versions.validate_after(&a.versions)?;
        self.grant
            .validate()
            .map_err(|_| Invalid("phase execution grant"))?;
        require(
            self.schema_version == 2
                && same(&self.negotiation, &a.negotiation)?
                && self.operation_id == q.original.operation_id
                && self.request_digest == q.digest()?
                && self.accepted_receipt_digest == a.digest()?
                && self.grant.binding == a.lease.binding
                && self.issued_at_ms == self.confirmation.checked_at_ms
                && self.versions.grant_revision > 0
                && self.versions.authority_revision > a.versions.authority_revision,
            "phase exact execution issuance",
        )?;
        let budget = self
            .confirmation
            .facts
            .iter()
            .find_map(|f| {
                if let FactValue::Budget {
                    elapsed_ms: Some(elapsed),
                    maximum_lineage_elapsed_ms,
                    ..
                } = &f.body.value
                {
                    maximum_lineage_elapsed_ms.checked_sub(*elapsed)
                } else {
                    None
                }
            })
            .ok_or(Invalid("phase current budget unknown"))?;
        revision(self.execution_deadline_ms)?;
        require(
            p.original.offered.artifacts.iter().all(|d| {
                d.retained_at_ms <= self.issued_at_ms && self.issued_at_ms < d.retain_until_ms
            }),
            "phase original input retention expired",
        )?;
        let remaining = p
            .original
            .offered
            .offer
            .manifest
            .resources
            .wall_time_ms
            .min(budget);
        require(
            self.grant.expires_at_ms > self.issued_at_ms
                && self.grant.expires_at_ms - self.issued_at_ms
                    <= p.policy.maximum_execution_lease_ms
                && self.grant.expires_at_ms <= self.execution_deadline_ms
                && self.execution_deadline_ms
                    <= self
                        .issued_at_ms
                        .checked_add(remaining)
                        .ok_or(Invalid("phase execution deadline overflow"))?,
            "independent bounded execution expiry",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        self.confirmation.digest()?;
        self.negotiation.validate()?;
        hash("aq.phase.execution-receipt.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "PreparationV2ExecutionDelivery")]
pub struct ExecutionDelivery {
    pub schema_version: u32,
    pub receipt: ExecutionReceipt,
    pub delivery: DeliveryStamp,
}
impl ExecutionDelivery {
    pub fn validate_for(
        &self,
        q: &PrepareRequest,
        a: &AcceptedReceipt,
        r: &AcceptRequest,
        p: &Proposal,
        nonce: Uuid,
    ) -> Result<()> {
        self.receipt.validate_for(q, a, r, p)?;
        require(
            self.schema_version == 2
                && self.delivery.clock.server_time_ms >= self.receipt.issued_at_ms,
            "phase execution delivery time",
        )?;
        self.delivery
            .validate_for(nonce, self.receipt.grant.expires_at_ms)?;
        bound(self)
    }
    /// No live authorization is returned. Owners must authenticate and resolve the
    /// actual retained receipt/current authority before using these time samples.
    // Keep each immutable association and clock input explicit; none is a verified-owner wrapper.
    #[allow(clippy::too_many_arguments)]
    pub fn sample(
        &self,
        q: &PrepareRequest,
        a: &AcceptedReceipt,
        r: &AcceptRequest,
        p: &Proposal,
        nonce: Uuid,
        sent: u64,
        received: u64,
        previous: Option<&DeadlineSample>,
    ) -> Result<DeadlineSample> {
        self.validate_for(q, a, r, p, nonce)?;
        let deadline = self.delivery.clock.client_deadline_ns(sent, received)?;
        let hard = self
            .receipt
            .execution_deadline_ms
            .checked_sub(self.delivery.clock.server_time_ms)
            .and_then(|n| n.checked_mul(1000000))
            .and_then(|n| sent.checked_add(n))
            .filter(|end| received < *end)
            .ok_or(Invalid("phase local hard deadline"))?;
        sample(
            self.receipt.digest()?,
            deadline.min(hard),
            hard,
            received,
            previous,
        )
    }
}
/// Non-Serde arithmetic history, deliberately not a permission/host permit.
#[derive(Debug, Clone)]
pub struct DeadlineSample {
    receipt_digest: Digest,
    deadline_ns: u64,
    hard_deadline_ns: u64,
}
impl DeadlineSample {
    pub fn receipt_digest(&self) -> &Digest {
        &self.receipt_digest
    }
    pub fn deadline_ns(&self) -> u64 {
        self.deadline_ns
    }
    pub fn hard_deadline_ns(&self) -> u64 {
        self.hard_deadline_ns
    }
}

fn sample(
    digest: Digest,
    mut deadline: u64,
    mut hard: u64,
    received: u64,
    previous: Option<&DeadlineSample>,
) -> Result<DeadlineSample> {
    if let Some(old) = previous {
        require(
            old.receipt_digest == digest
                && received < old.deadline_ns
                && received < old.hard_deadline_ns,
            "phase expired or changed replay",
        )?;
        deadline = deadline.min(old.deadline_ns);
        hard = hard.min(old.hard_deadline_ns);
    }
    require(
        received < deadline && deadline <= hard,
        "phase delivery expired",
    )?;
    Ok(DeadlineSample {
        receipt_digest: digest,
        deadline_ns: deadline,
        hard_deadline_ns: hard,
    })
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[schemars(rename = "PreparationV2Document")]
pub enum Document {
    Proposal(Box<Proposal>),
    Accept(Box<AcceptRequest>),
    Accepted(Box<AcceptedReceipt>),
    AcceptedDelivery(Box<AcceptedDelivery>),
    Prepare(Box<PrepareRequest>),
    Confirmation(Box<FreshConfirmation>),
    Execution(Box<ExecutionReceipt>),
    ExecutionDelivery(Box<ExecutionDelivery>),
}
impl Document {
    /// Context-free structural checks only, never owner authentication.
    pub fn validate_shape(&self) -> Result<()> {
        let version = match self {
            Self::Proposal(v) => {
                v.validate()?;
                v.schema_version
            }
            Self::Accept(v) => {
                v.negotiation.validate()?;
                transport::TransportDocument::Accept(v.original.clone()).validate()?;
                v.schema_version
            }
            Self::Accepted(v) => {
                v.negotiation.validate()?;
                v.versions.validate()?;
                scope(&v.lease.identity, &v.lease.binding)?;
                id(v.lease.lease_id)?;
                require(v.lease.materials.len() == 1, "phase material bound")?;
                v.lease.materials[0].validate()?;
                v.schema_version
            }
            Self::AcceptedDelivery(v) => {
                Document::Accepted(Box::new(v.receipt.clone())).validate_shape()?;
                v.delivery.clock.validate()?;
                id(v.delivery.nonce)?;
                v.schema_version
            }
            Self::Prepare(v) => {
                v.negotiation.validate()?;
                transport::TransportDocument::Prepare(v.original.clone()).validate()?;
                v.schema_version
            }
            Self::Confirmation(v) => {
                id(v.confirmation_id)?;
                require(v.facts.len() == 9, "phase fact bound")?;
                let mut kinds = BTreeSet::new();
                for f in &v.facts {
                    OwnerFact::record_digest(&f.body)?;
                    require(kinds.insert(f.body.value.kind()), "duplicate phase fact")?;
                }
                v.schema_version
            }
            Self::Execution(v) => {
                v.negotiation.validate()?;
                Document::Confirmation(Box::new(v.confirmation.clone())).validate_shape()?;
                v.grant
                    .validate()
                    .map_err(|_| Invalid("phase grant shape"))?;
                v.versions.validate()?;
                v.schema_version
            }
            Self::ExecutionDelivery(v) => {
                Document::Execution(Box::new(v.receipt.clone())).validate_shape()?;
                v.delivery.clock.validate()?;
                id(v.delivery.nonce)?;
                v.schema_version
            }
        };
        require(version == 2, "phase document version")?;
        bound(self)
    }
}
pub fn decode(data: &[u8]) -> Result<Document> {
    require(data.len() <= MAX_BYTES, "phase decode byte bound")?;
    let value: Document =
        serde_json::from_slice(data).map_err(|_| Invalid("phase document shape"))?;
    value.validate_shape()?;
    Ok(value)
}

pub fn schema() -> Value {
    let mut s = serde_json::to_value(schemars::schema_for!(Document)).expect("phase schema");
    let base = contract_schema();
    if let Some(defs) = s.get_mut("$defs").and_then(Value::as_object_mut) {
        for (k, v) in defs {
            if let Some(b) = base.get("$defs").and_then(|d| d.get(k)) {
                *v = b.clone();
            }
        }
    }
    if let Some(defs) = s.get_mut("$defs").and_then(Value::as_object_mut) {
        for (name, def) in defs {
            if name.starts_with("PreparationV2")
                && let Some(props) = def.get_mut("properties").and_then(Value::as_object_mut)
            {
                if let Some(v) = props.get_mut("schema_version") {
                    v["const"] = serde_json::json!(2);
                }
                if let Some(v) = props.get_mut("features") {
                    v["uniqueItems"] = serde_json::json!(true);
                }
            }
        }
    }
    s["$id"] = serde_json::json!("urn:aq:admission:preparation-v2:1");
    s["$comment"] = serde_json::json!(
        "<=131072 bytes. Relational validators mandatory; no authentication, owner truth, physical proof or live authority follows from schema/DTO presence."
    );
    s
}
pub fn contract_manifest() -> Result<Value> {
    Ok(
        serde_json::json!({"schema_version":2,"publication":"aq-admission/0.1.9","schema_digest":Digest::of(&bytes(&schema())?),"required_features":required_features()}),
    )
}
fn build_contract_artifact() -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new("aq.admission.preparation-v2-contract")
            .map_err(|_| Invalid("phase contract id"))?,
        version: Text::new("1").map_err(|_| Invalid("phase contract version"))?,
        digest: hash("aq.phase.contract.v2", &contract_manifest()?)?,
    })
}
pub fn contract_artifact() -> Result<ArtifactRef> {
    static ARTIFACT: std::sync::OnceLock<ArtifactRef> = std::sync::OnceLock::new();
    Ok(ARTIFACT
        .get_or_init(|| build_contract_artifact().expect("static bounded phase schema artifact"))
        .clone())
}
