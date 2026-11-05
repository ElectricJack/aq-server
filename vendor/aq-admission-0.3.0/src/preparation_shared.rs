// One source of the bounded phase algorithms; profile-specific public types
// retain their own schema names and exact negotiation/hash domains.
macro_rules! phase_profile {
    (
        PhaseFeature: $PhaseFeature:literal,
        Negotiation: $Negotiation:literal,
        PhasePolicy: $PhasePolicy:literal,
        ChoicePins: $ChoicePins:literal,
        OwnerAuthority: $OwnerAuthority:literal,
        Proposal: $Proposal:literal,
        ResourceHold: $ResourceHold:literal,
        AcceptRequest: $AcceptRequest:literal,
        AcceptedPreparationLease: $AcceptedPreparationLease:literal,
        AcceptedReceipt: $AcceptedReceipt:literal,
        AcceptedDelivery: $AcceptedDelivery:literal,
        PrepareRequest: $PrepareRequest:literal,
        FreshConfirmation: $FreshConfirmation:literal,
        ExecutionReceipt: $ExecutionReceipt:literal,
        ExecutionDelivery: $ExecutionDelivery:literal,
        Document: $Document:literal,
        schema_prefix: $schema_prefix:literal,
        schema_urn: $schema_urn:literal,
        publication: $publication:literal,
        contract_id: $contract_id:literal,
        extra_features: [$($extra_feature:ident),* $(,)?],
        lease_fields: [$($lease_field:ident: $lease_type:ty),* $(,)?],
    ) => {
// Separately bounded proposal, accepted preparation and execution observations.
// Pure correlation/arithmetic only: no DTO, fact status or successful validation
// authenticates an owner, proves resources/files, or grants live permission.
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
pub const MAX_INTERACTIVE_EXECUTION_MS: u64 = 3_600_000;
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
    let domain = profile_domain(domain);
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
#[schemars(rename = $PhaseFeature)]
pub enum PhaseFeature {
    AcceptedPreparationV2,
    FreshInitialConfirmationV2,
    IndependentExecutionDeliveryV2,
    $($extra_feature,)*
}
pub fn required_features() -> Vec<PhaseFeature> {
    vec![
        PhaseFeature::AcceptedPreparationV2,
        PhaseFeature::FreshInitialConfirmationV2,
        PhaseFeature::IndependentExecutionDeliveryV2,
        $(PhaseFeature::$extra_feature,)*
    ]
}
/// Exact artifact negotiation is separate from frozen legacy feature enums.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $Negotiation)]
pub struct Negotiation {
    pub contract: ArtifactRef,
    #[schemars(length(max = 8))]
    pub features: Vec<PhaseFeature>,
}
impl Negotiation {
    pub fn validate(&self) -> Result<()> {
        require(
            self.contract == contract_artifact()? || self.contract == long_contract_artifact()?,
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
#[schemars(rename = $PhasePolicy)]
pub struct PhasePolicy {
    pub schema_version: u32,
    pub maximum_preparation_ms: u64,
    pub maximum_execution_lease_ms: u64,
    pub maximum_observation_ms: u64,
}
impl PhasePolicy {
    pub fn validate(&self) -> Result<()> {
        require(
            ((self.schema_version == 2 && (1..=120000).contains(&self.maximum_execution_lease_ms))
                || (self.schema_version == 3 && (1..=MAX_INTERACTIVE_EXECUTION_MS).contains(&self.maximum_execution_lease_ms)))
                && (1..=MAX_PREPARATION_MS).contains(&self.maximum_preparation_ms)
                && (1..=30000).contains(&self.maximum_observation_ms),
            "phase policy bounds",
        )
    }
    /// Policy3 is an explicit long interactive selection, never a wider
    /// interpretation of an old policy2. Data validation is not installation.
    pub fn validate_for_manifest(&self, manifest: &aq_session_protocol::LaunchManifest) -> Result<()> {
        self.validate()?;
        if self.schema_version == 3 {
            require(manifest.terminal.input_enabled
                && manifest.harness.harness.as_str() == "codex"
                && manifest.harness.cli_version.as_str() == "0.166.0"
                && manifest.harness.adapter.id.as_str() == "aq.codex-result-local-adapter",
                "long execution requires explicit interactive Codex manifest")?;
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        hash("aq.phase.policy.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $ChoicePins)]
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
        validate_registry_choice(self)?;
        hash("aq.phase.choice.v2", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $OwnerAuthority)]
pub struct OwnerAuthority {
    pub kind: FactKind,
    pub authority: ArtifactRef,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $Proposal)]
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
        require(self.negotiation.contract == policy_contract(&self.policy)?,
            "exact phase policy contract selection")?;
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
        self.policy.validate_for_manifest(m)?;
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
#[schemars(rename = $ResourceHold)]
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
#[schemars(rename = $AcceptRequest)]
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
#[schemars(rename = $AcceptedPreparationLease)]
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
    $(pub $lease_field: $lease_type,)*
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $AcceptedReceipt)]
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
        validate_registry_lease(l, &p.choice.registry)?;
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
#[schemars(rename = $AcceptedDelivery)]
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
#[schemars(rename = $PrepareRequest)]
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
#[schemars(rename = $FreshConfirmation)]
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
#[schemars(rename = $ExecutionReceipt)]
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
#[schemars(rename = $ExecutionDelivery)]
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
        if p.policy.schema_version == 3 {
            self.delivery.clock.validate_interactive_session()?;
            id(nonce)?; id(self.delivery.nonce)?;
            require(self.delivery.nonce == nonce
                && self.delivery.clock.expires_at_ms == self.receipt.grant.expires_at_ms,
                "interactive delivery nonce/expiry")?;
        } else {
            self.delivery.validate_for(nonce, self.receipt.grant.expires_at_ms)?;
        }
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
        let deadline = if p.policy.schema_version == 3 {
            self.delivery.clock.client_interactive_deadline_ns(sent, received)?
        } else { self.delivery.clock.client_deadline_ns(sent, received)? };
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
#[schemars(rename = $Document)]
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
                validate_registry_lease_shape(&v.lease)?;
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
                if v.receipt.negotiation.contract == long_contract_artifact()? {
                    v.delivery.clock.validate_interactive_session()?;
                } else { v.delivery.clock.validate()?; }
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
            if name.starts_with($schema_prefix)
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
    s["$id"] = serde_json::json!($schema_urn);
    s["$comment"] = serde_json::json!(
        "<=131072 bytes. Relational validators mandatory; no authentication, owner truth, physical proof or live authority follows from schema/DTO presence."
    );
    s
}
/// Additive interactive dialect; the original schema/artifact remain byte exact.
pub fn long_schema() -> Value {
    let mut value = schema();
    value["$id"] = serde_json::json!(format!("{}:interactive-long-v1", $schema_urn));
    value["$defs"][$PhasePolicy]["properties"]["schema_version"]["const"] = serde_json::json!(3);
    value["$defs"][$PhasePolicy]["properties"]["maximum_execution_lease_ms"]["maximum"] = serde_json::json!(MAX_INTERACTIVE_EXECUTION_MS);
    value["$defs"]["DeliveryClock"]["properties"]["valid_for_ms"]["maximum"] = serde_json::json!(MAX_INTERACTIVE_EXECUTION_MS);
    value
}
fn build_long_contract_artifact() -> Result<ArtifactRef> {
    let manifest = serde_json::json!({"schema_version":3,"publication":"aq-admission/0.1.11",
        "scope":"explicit-interactive-one-hour","schema_digest":Digest::of(&bytes(&long_schema())?),
        "required_features":required_features()});
    Ok(ArtifactRef { id:Text::new(format!("{}.interactive-long-v1",$contract_id)).map_err(|_| Invalid("long phase contract id"))?,
        version:Text::new("1").map_err(|_| Invalid("long phase contract version"))?,digest:hash("aq.phase.contract.v2",&manifest)? })
}
pub fn long_contract_artifact() -> Result<ArtifactRef> {
    static ARTIFACT: std::sync::OnceLock<ArtifactRef> = std::sync::OnceLock::new();
    Ok(ARTIFACT
        .get_or_init(|| build_long_contract_artifact().expect("static bounded interactive phase schema artifact"))
        .clone())
}
pub fn policy_contract(policy: &PhasePolicy) -> Result<ArtifactRef> {
    policy.validate()?;
    if policy.schema_version == 3 { long_contract_artifact() } else { contract_artifact() }
}
pub fn contract_manifest() -> Result<Value> {
    Ok(
        serde_json::json!({"schema_version":2,"publication":$publication,"schema_digest":Digest::of(&bytes(&schema())?),"required_features":required_features()}),
    )
}
fn build_contract_artifact() -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new($contract_id)
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

    };
}
pub(crate) use phase_profile;

// One source of the bounded phase algorithms; profile-specific public types
// retain their own schema names and exact negotiation/hash domains.
macro_rules! phase_facts {
    (
        FactKind: $FactKind:literal,
        AvailabilityState: $AvailabilityState:literal,
        InventoryState: $InventoryState:literal,
        GlobalCredentialSemantics: $GlobalCredentialSemantics:literal,
        RetentionState: $RetentionState:literal,
        FactValue: $FactValue:literal,
        FactBody: $FactBody:literal,
        OwnerFact: $OwnerFact:literal,
        registry_fields: [$($registry_field:ident: $registry_type:ty),* $(,)?],
    ) => {
// Mandatory typed provenance claims. Consumers must resolve authenticated owner
// records; matching status/digest fields alone never certify current fact truth.
use super::*;
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = $FactKind)]
pub enum FactKind {
    Project,
    Registry,
    Principal,
    Inventory,
    Availability,
    Budget,
    Context,
    Git,
    PrivateMaterialBacking,
}
impl FactKind {
    pub fn all() -> [Self; 9] {
        [
            Self::Project,
            Self::Registry,
            Self::Principal,
            Self::Inventory,
            Self::Availability,
            Self::Budget,
            Self::Context,
            Self::Git,
            Self::PrivateMaterialBacking,
        ]
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = $AvailabilityState)]
pub enum AvailabilityState {
    Ready,
    Unavailable,
    Cooldown,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = $InventoryState)]
pub enum InventoryState {
    Installed,
    Missing,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = $GlobalCredentialSemantics)]
pub enum GlobalCredentialSemantics {
    ProvisionableOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = $RetentionState)]
pub enum RetentionState {
    Active,
    Released,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[schemars(rename = $FactValue)]
pub enum FactValue {
    Project {
        project_id: Uuid,
        revision: u64,
        catalog: ArtifactRef,
        permissions_digest: Digest,
    },
    Registry {
        $($registry_field: $registry_type,)*
    },
    Principal {
        principal: ArtifactRef,
        scope: ArtifactRef,
    },
    Inventory {
        node_id: Uuid,
        boot_id: Uuid,
        sequence: u64,
        model: aq_session_protocol::ModelSettings,
        harness: aq_session_protocol::HarnessSettings,
        #[schemars(length(max = 64))]
        capabilities: Vec<aq_session_protocol::Capability>,
        state: InventoryState,
        credential_semantics: GlobalCredentialSemantics,
        credential_profile: ArtifactRef,
        credential_revision: u64,
    },
    Availability {
        provider: Text,
        account_reference: Text,
        endpoint_reference: Text,
        state: AvailabilityState,
        provider_observation: ArtifactRef,
    },
    Budget {
        lineage_id: Uuid,
        elapsed_ms: Option<u64>,
        maximum_lineage_elapsed_ms: u64,
        remaining_cost_microunits: Option<u64>,
    },
    Context {
        task: TaskRevision,
        context: ArtifactRef,
        full_context_digest: Digest,
        graph_policy: ArtifactRef,
    },
    Git {
        repository_id: Uuid,
        binding_generation: u64,
        base: ObjectId,
        policy: ArtifactRef,
        retention: ArtifactRef,
        state: RetentionState,
        manifest: transport::ArtifactReceipt,
        bundle: transport::ArtifactReceipt,
    },
    PrivateMaterialBacking {
        lease_id: Uuid,
        backing_digest: Digest,
        installation_receipt_digest: Digest,
    },
}
impl FactValue {
    pub fn kind(&self) -> FactKind {
        match self {
            Self::Project { .. } => FactKind::Project,
            Self::Registry { .. } => FactKind::Registry,
            Self::Principal { .. } => FactKind::Principal,
            Self::Inventory { .. } => FactKind::Inventory,
            Self::Availability { .. } => FactKind::Availability,
            Self::Budget { .. } => FactKind::Budget,
            Self::Context { .. } => FactKind::Context,
            Self::Git { .. } => FactKind::Git,
            Self::PrivateMaterialBacking { .. } => FactKind::PrivateMaterialBacking,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $FactBody)]
pub struct FactBody {
    pub authority: ArtifactRef,
    pub binding: LaunchBinding,
    pub choice_digest: Digest,
    pub revision: u64,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub value: FactValue,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = $OwnerFact)]
pub struct OwnerFact {
    pub record: ArtifactRef,
    pub body: FactBody,
}
impl OwnerFact {
    pub fn record_digest(body: &FactBody) -> Result<Digest> {
        match &body.value {
            FactValue::Inventory {
                capabilities,
                harness,
                ..
            } => require(
                capabilities.len() <= 64 && harness.required_capabilities.len() <= 64,
                "phase fact capability bound",
            )?,
            FactValue::Git { base, .. } => base.validate()?,
            _ => {}
        }
        validate_registry_record_shape(body)?;
        hash("aq.phase.owner-fact.v2", body)
    }
    pub fn validate_for(
        &self,
        p: &Proposal,
        a: &AcceptedReceipt,
        q: &PrepareRequest,
        at: u64,
    ) -> Result<()> {
        reference(&self.record)?;
        reference(&self.body.authority)?;
        revision(self.body.revision)?;
        require(
            self.record.digest == Self::record_digest(&self.body)?
                && self.record.version.as_str() == self.body.revision.to_string()
                && self.body.binding == a.lease.binding
                && self.body.choice_digest == a.lease.choice_digest
                && self.body.observed_at_ms > 0
                && self.body.observed_at_ms <= at
                && at < self.body.expires_at_ms
                && self.body.expires_at_ms <= i64::MAX as u64
                && self
                    .body
                    .expires_at_ms
                    .checked_sub(self.body.observed_at_ms)
                    .is_some_and(|ms| ms <= p.policy.maximum_observation_ms),
            "phase fact provenance/time/binding",
        )?;
        require(
            p.owners
                .iter()
                .any(|o| o.kind == self.body.value.kind() && o.authority == self.body.authority),
            "phase fact owner association",
        )?;
        let c = &p.choice;
        let m = &p.original.offered.offer.manifest;
        match &self.body.value {
            FactValue::Project {
                project_id,
                revision: rev,
                catalog,
                permissions_digest,
            } => require(
                *project_id == p.spec.lifecycle.project_id
                    && *rev == p.spec.project_revision
                    && *catalog == m.catalog
                    && *permissions_digest == c.permission_digest,
                "phase current project/pins",
            )?,
            FactValue::Registry { .. } => validate_registry_value(self, p, a)?,
            FactValue::Principal { principal, scope } => require(
                *principal == c.principal && *scope == p.spec.graph_access_profile,
                "phase current principal/scope",
            )?,
            FactValue::Inventory {
                node_id,
                boot_id,
                sequence,
                model,
                harness,
                capabilities,
                state,
                credential_profile,
                credential_revision,
                ..
            } => {
                revision(*sequence)?;
                require(
                    *node_id == a.lease.identity.node_id
                        && *boot_id == a.lease.identity.boot_id
                        && *model == c.model
                        && *harness == c.harness
                        && *state == InventoryState::Installed
                        && *credential_profile == c.credential_profile
                        && *credential_revision == c.credential_revision,
                    "phase exact installed harness/provisionable credential",
                )?;
                aq_session_protocol::require_capabilities(
                    &p.original.offered.offer.required_capabilities,
                    capabilities,
                )
                .map_err(|_| Invalid("phase inventory capabilities"))?;
                aq_session_protocol::require_capabilities(
                    &m.harness.required_capabilities,
                    capabilities,
                )
                .map_err(|_| Invalid("phase harness capabilities"))?;
            }
            FactValue::Availability {
                provider,
                account_reference,
                endpoint_reference,
                state,
                provider_observation,
            } => {
                reference(provider_observation)?;
                require(
                    *provider == c.model.provider
                        && *account_reference == c.account_reference
                        && *endpoint_reference == c.endpoint_reference
                        && *state == AvailabilityState::Ready,
                    "phase initial availability must be fresh known ready",
                )?;
            }
            FactValue::Budget {
                lineage_id,
                elapsed_ms,
                maximum_lineage_elapsed_ms,
                remaining_cost_microunits,
            } => {
                let elapsed = elapsed_ms.ok_or(Invalid("phase initial budget unknown"))?;
                require(
                    *lineage_id == p.spec.lifecycle.lineage_id
                        && *maximum_lineage_elapsed_ms == c.maximum_lineage_elapsed_ms
                        && elapsed < *maximum_lineage_elapsed_ms,
                    "phase current remaining lineage",
                )?;
                if let Some(limit) = &m.model.spending_budget {
                    require(
                        remaining_cost_microunits
                            .is_some_and(|n| n > 0 && n <= limit.limit_microunits),
                        "phase current cost budget unknown/exhausted",
                    )?;
                }
            }
            FactValue::Context {
                task,
                context,
                full_context_digest,
                graph_policy,
            } => require(
                same(task, &p.spec.task)?
                    && *context == p.spec.task_context
                    && *full_context_digest == p.full_context_digest
                    && *graph_policy == p.spec.graph_access_profile,
                "phase full original task context",
            )?,
            FactValue::Git {
                repository_id,
                binding_generation,
                base,
                policy,
                retention,
                state,
                manifest,
                bundle,
            } => {
                base.validate()?;
                reference(retention)?;
                require(
                    *repository_id == p.spec.source.repository_id
                        && *binding_generation == p.spec.source.repository_binding_generation
                        && same(base, &p.spec.source.resolved_object)?
                        && base.hex.bytes().any(|b| b != b'0')
                        && *policy == p.spec.source.policy
                        && *state == RetentionState::Active,
                    "phase original retained base/policy",
                )?;
                let prefix = "aq.source-manifest/";
                let transfer = manifest
                    .artifact
                    .id
                    .as_str()
                    .strip_prefix(prefix)
                    .ok_or(Invalid("phase manifest member identity"))?;
                let id =
                    Uuid::parse_str(transfer).map_err(|_| Invalid("phase transfer identity"))?;
                crate::id(id)?;
                require(
                    manifest.artifact.id.as_str() == format!("aq.source-manifest/{id}"),
                    "phase canonical transfer member",
                )?;
                require(
                    bundle.artifact.id.as_str() == format!("aq.source-bundle/{id}")
                        && manifest.artifact.version.as_str() == "1"
                        && bundle.artifact.version.as_str() == "1"
                        && manifest.bytes > 0
                        && bundle.bytes > 0
                        && manifest.digest == manifest.artifact.digest
                        && bundle.digest == bundle.artifact.digest
                        && q.original
                            .artifacts
                            .iter()
                            .any(|x| same(x, manifest).unwrap_or(false))
                        && q.original
                            .artifacts
                            .iter()
                            .any(|x| same(x, bundle).unwrap_or(false)),
                    "phase exact linked transfer members",
                )?;
            }
            FactValue::PrivateMaterialBacking {
                lease_id,
                backing_digest,
                installation_receipt_digest,
            } => {
                let lease = &a.lease.materials[0];
                let installed = &q.original.materials[0];
                require(
                    *lease_id == lease.lease_id
                        && *backing_digest == lease.material_digest
                        && *installation_receipt_digest == Digest::of(&bytes(installed)?),
                    "phase original installation/current backing",
                )?;
            }
        }
        bound(self)
    }
}

    };
}
pub(crate) use phase_facts;
