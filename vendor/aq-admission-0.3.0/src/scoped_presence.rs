//! Typed inventory semantics and one-grant installed-material observations.
//! Validation is structural. Only an authenticated, qualified daemon owner can
//! record facts; no value in this module grants execution or global availability.
use crate::*;
use std::collections::BTreeSet;
use transport::{MaterialDelivery, WorkerIdentity};

pub const MAX_REQUEST_BYTES: usize = 65_536;
pub const MAX_RECEIPT_BYTES: usize = 4096;
pub const MAX_TTL_MS: u64 = 30_000;
pub const MAX_READ_MS: u64 = 2000;
pub const QUALIFICATION_PREFIX: &str = "aq.scoped-material-presence-qualification/";
fn canonical(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(value).map_err(|_| Invalid("presence encoding"))?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| Invalid("presence encoding"))
}
fn digest(value: &impl Serialize) -> Result<Digest> {
    Ok(Digest::of(&canonical(value)?))
}
fn bytes(value: &impl Serialize, max: usize) -> Result<()> {
    require(canonical(value)?.len() <= max, "presence byte bound")
}
fn scope(identity: &WorkerIdentity, binding: &LaunchBinding) -> Result<()> {
    identity.validate()?;
    binding
        .validate()
        .map_err(|_| Invalid("presence binding"))?;
    require(
        matches!(binding.session, SessionIdentity::Remote {node_id,boot_id,..} if node_id==identity.node_id && boot_id==identity.boot_id),
        "presence node/boot scope",
    )
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Protocol {
    #[serde(rename = "aq.scoped-material-presence.v1")]
    ScopedMaterialPresenceV1,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Negotiation {
    pub protocol: Protocol,
    pub qualification: ArtifactRef,
}
impl Negotiation {
    fn validate(&self) -> Result<()> {
        artifact(&self.qualification)?;
        require(
            self.qualification
                .id
                .as_str()
                .strip_prefix(QUALIFICATION_PREFIX)
                .is_some_and(|suffix| !suffix.is_empty()),
            "presence qualification namespace",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InventorySemantics {
    ProvisionableOnlyAndScopedPresenceV1,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservationProtocol {
    GrantIssuanceBoundedBoottimeV1,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Qualification {
    pub schema_version: u32,
    pub protocol: Protocol,
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    pub adapter: ArtifactRef,
    pub adapter_qualification: ArtifactRef,
    pub inventory_semantics: InventorySemantics,
    pub observation_protocol: ObservationProtocol,
    pub maximum_age_ms: u64,
    pub maximum_ttl_ms: u64,
}
impl Qualification {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "presence qualification version")?;
        for r in [
            &self.runtime,
            &self.runtime_qualification,
            &self.adapter,
            &self.adapter_qualification,
        ] {
            artifact(r)?;
        }
        require(
            (1000..=MAX_TTL_MS).contains(&self.maximum_age_ms)
                && (1000..=self.maximum_age_ms).contains(&self.maximum_ttl_ms),
            "presence qualification age/TTL",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        digest(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CredentialReference {
    pub profile: ArtifactRef,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventoryV2 {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    pub negotiation: Negotiation,
    pub sequence: u64,
    pub ttl_ms: u64,
    pub qualifications: Vec<ArtifactRef>,
    /// Explicitly NOT an installed or provider-availability assertion.
    pub provisionable_only: Vec<CredentialReference>,
}
impl InventoryV2 {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "inventory v2 version")?;
        id(self.operation_id)?;
        self.identity.validate()?;
        artifact(&self.runtime)?;
        artifact(&self.runtime_qualification)?;
        self.negotiation.validate()?;
        revision(self.sequence)?;
        require(
            (1000..=MAX_TTL_MS).contains(&self.ttl_ms)
                && !self.qualifications.is_empty()
                && self.qualifications.len() <= 16
                && self.provisionable_only.len() <= 32,
            "inventory v2 bounds",
        )?;
        let mut seen = BTreeSet::new();
        for r in &self.qualifications {
            artifact(r)?;
            require(
                seen.insert((r.id.clone(), r.version.clone())),
                "duplicate harness qualification",
            )?;
        }
        let mut seen = BTreeSet::new();
        for c in &self.provisionable_only {
            artifact(&c.profile)?;
            revision(c.revision)?;
            require(
                seen.insert((c.profile.id.clone(), c.profile.version.clone())),
                "duplicate credential reference",
            )?;
        }
        bytes(self, MAX_REQUEST_BYTES)
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        digest(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventoryIdentity {
    pub operation_id: Uuid,
    pub sequence: u64,
    pub revision: u64,
    pub request_digest: Digest,
    pub snapshot_digest: Digest,
}
impl InventoryIdentity {
    pub fn validate(&self) -> Result<()> {
        id(self.operation_id)?;
        revision(self.sequence)?;
        revision(self.revision)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventoryReceipt {
    pub schema_version: u32,
    pub identity: WorkerIdentity,
    pub negotiation: Negotiation,
    pub inventory: InventoryIdentity,
    pub recorded_at_ms: u64,
    pub expires_at_ms: u64,
}
impl InventoryReceipt {
    pub fn validate_for(&self, request: &InventoryV2) -> Result<()> {
        request.validate()?;
        self.inventory.validate()?;
        require(
            self.schema_version == 1
                && self.identity == request.identity
                && self.negotiation.protocol == request.negotiation.protocol
                && self.negotiation.qualification == request.negotiation.qualification
                && self.inventory.operation_id == request.operation_id
                && self.inventory.sequence == request.sequence
                && self.inventory.request_digest == request.digest()?,
            "inventory receipt scope",
        )?;
        time(self.recorded_at_ms, self.expires_at_ms, request.ttl_ms)?;
        bytes(self, MAX_RECEIPT_BYTES)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ClockSource {
    #[serde(rename = "CLOCK_BOOTTIME")]
    Boottime,
}
/// Exact mirror of the existing private installer's nonsecret canonical record.
/// Its hash must equal the ACTUAL retained MaterialReceipt.private_binding.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PrivateInstallationRecord {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub preparation_digest: Digest,
    pub worker: WorkerIdentity,
    pub kernel_boot_id: Uuid,
    pub clock_source: ClockSource,
    pub delivery: MaterialDelivery,
    pub sent_ns: u64,
    pub received_ns: u64,
    pub material_synced_ns: u64,
    pub retrieval_deadline_ns: u64,
}
impl PrivateInstallationRecord {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "private installation version")?;
        id(self.operation_id)?;
        id(self.kernel_boot_id)?;
        self.worker.validate()?;
        self.delivery.lease.validate()?;
        self.delivery.clock.validate()?;
        scope(&self.worker, &self.delivery.lease.binding)?;
        require(
            self.delivery.clock.server_time_ms >= self.delivery.lease.issued_at_ms
                && self.delivery.clock.expires_at_ms == self.delivery.lease.expires_at_ms
                && self.sent_ns > 0
                && self.received_ns >= self.sent_ns
                && self.material_synced_ns >= self.received_ns
                && self.material_synced_ns < self.retrieval_deadline_ns,
            "private installation intervals",
        )?;
        let verified = self
            .delivery
            .clock
            .client_deadline_ns(self.sent_ns, self.received_ns)?;
        require(
            verified == self.retrieval_deadline_ns,
            "private installation deadline",
        )?;
        bytes(self, 8192)
    }
    pub fn receipt(&self) -> Result<MaterialReceipt> {
        self.validate()?;
        Ok(MaterialReceipt {
            schema_version: 1,
            operation_id: self.operation_id,
            lease_id: self.delivery.lease.lease_id,
            binding: self.delivery.lease.binding.clone(),
            material_digest: self.delivery.lease.material_digest.clone(),
            private_binding: ArtifactRef {
                id: Text::new(format!(
                    "aq.private-material/{}",
                    self.delivery.lease.lease_id
                ))
                .map_err(|_| Invalid("private binding identity"))?,
                version: Text::new("1").map_err(|_| Invalid("private binding version"))?,
                digest: digest(self)?,
            },
            received_at_ms: self.delivery.clock.server_time_ms,
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityKind {
    InitialGrant,
    Renewal,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthorityAnchor {
    pub kind: AuthorityKind,
    pub operation_id: Uuid,
    pub grant_revision: u64,
    pub expires_at_ms: u64,
}
impl AuthorityAnchor {
    pub fn validate(&self) -> Result<()> {
        id(self.operation_id)?;
        revision(self.grant_revision)?;
        revision(self.expires_at_ms)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub kernel_boot_id: Uuid,
    pub clock_source: ClockSource,
    pub permit_acquired_ns: u64,
    pub permit_deadline_ns: u64,
    pub read_started_ns: u64,
    pub read_completed_ns: u64,
    pub sent_ns: u64,
}
impl Observation {
    pub fn validate(&self) -> Result<()> {
        id(self.kernel_boot_id)?;
        require(
            self.permit_acquired_ns > 0
                && self.permit_acquired_ns <= self.read_started_ns
                && self.read_started_ns <= self.read_completed_ns
                && self.read_completed_ns <= self.sent_ns
                && self.permit_deadline_ns > self.permit_acquired_ns
                && self.sent_ns - self.read_started_ns <= MAX_READ_MS * 1_000_000,
            "presence local clock/liveness",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MissingReason {
    Missing,
    Changed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnknownReason {
    Unreadable,
    BootUnknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Presence {
    Present {
        installation: Box<PrivateInstallationRecord>,
        observed_material_digest: Digest,
        observed_material_bytes: u64,
    },
    Missing {
        reason: MissingReason,
    },
    Unknown {
        reason: UnknownReason,
    },
}
impl Presence {
    pub fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresenceReport {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub runtime: ArtifactRef,
    pub runtime_qualification: ArtifactRef,
    pub negotiation: Negotiation,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
    pub lease_id: Uuid,
    pub installation_receipt_digest: Digest,
    pub private_binding: ArtifactRef,
    pub inventory: InventoryIdentity,
    pub authority: AuthorityAnchor,
    pub sequence: u64,
    pub ttl_ms: u64,
    pub observation: Observation,
    pub presence: Presence,
}
impl PresenceReport {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "presence report version")?;
        for x in [
            self.operation_id,
            self.grant_id,
            self.guardian_id,
            self.launch_id,
            self.reservation_id,
            self.lease_id,
        ] {
            id(x)?;
        }
        scope(&self.identity, &self.binding)?;
        artifact(&self.runtime)?;
        artifact(&self.runtime_qualification)?;
        self.negotiation.validate()?;
        artifact(&self.private_binding)?;
        require(
            self.private_binding.id.as_str() == format!("aq.private-material/{}", self.lease_id)
                && self.private_binding.version.as_str() == "1",
            "private binding namespace",
        )?;
        self.inventory.validate()?;
        self.authority.validate()?;
        revision(self.sequence)?;
        self.observation.validate()?;
        require((1000..=MAX_TTL_MS).contains(&self.ttl_ms), "presence TTL")?;
        if let Presence::Present {
            installation,
            observed_material_digest,
            observed_material_bytes,
        } = &self.presence
        {
            require(
                self.observation.sent_ns < self.observation.permit_deadline_ns,
                "positive presence needs live local permit",
            )?;
            installation.validate()?;
            require(
                *observed_material_digest == installation.delivery.lease.material_digest
                    && *observed_material_bytes == installation.delivery.lease.material_bytes,
                "fresh private byte observation mismatch",
            )?;
            require(
                installation.worker == self.identity
                    && installation.kernel_boot_id == self.observation.kernel_boot_id
                    && installation.delivery.lease.binding == self.binding
                    && installation.delivery.lease.lease_id == self.lease_id
                    && installation.material_synced_ns <= self.observation.read_started_ns
                    && installation.receipt()?.private_binding == self.private_binding,
                "presence private installation scope",
            )?;
        }
        bytes(self, MAX_REQUEST_BYTES)
    }
    pub fn validate_against(&self, lease: &MaterialLease, receipt: &MaterialReceipt) -> Result<()> {
        self.validate()?;
        lease.validate()?;
        receipt.validate_against(lease, receipt.received_at_ms)?;
        require(
            lease.binding == self.binding
                && lease.lease_id == self.lease_id
                && digest(receipt)? == self.installation_receipt_digest
                && receipt.private_binding == self.private_binding,
            "presence retained lease/receipt scope",
        )?;
        if let Presence::Present { installation, .. } = &self.presence {
            require(
                digest(&installation.delivery.lease)? == digest(lease)?
                    && digest(&installation.receipt()?)? == digest(receipt)?,
                "presence original private record",
            )?;
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        digest(self)
    }
}
/// This computes a bound, not authority. The daemon must verify the ACTUAL
/// retained anchor, current grant revision/liveness, scope and qualifications.
pub fn positive_expiry(
    issued_at_ms: u64,
    received_at_ms: u64,
    ttl_ms: u64,
    maximum_age_ms: u64,
    hard_deadline_ms: u64,
) -> Result<u64> {
    require(
        issued_at_ms > 0
            && received_at_ms >= issued_at_ms
            && (1000..=MAX_TTL_MS).contains(&ttl_ms)
            && (1000..=MAX_TTL_MS).contains(&maximum_age_ms),
        "presence issuance/receive bounds",
    )?;
    let expiry = received_at_ms
        .checked_add(ttl_ms)
        .ok_or(Invalid("presence TTL overflow"))?
        .min(
            issued_at_ms
                .checked_add(maximum_age_ms)
                .ok_or(Invalid("presence age overflow"))?,
        )
        .min(hard_deadline_ms);
    require(
        expiry > received_at_ms,
        "presence observation already too old",
    )?;
    Ok(expiry)
}
pub fn decode_inventory(bytes: &[u8]) -> Result<InventoryV2> {
    require(bytes.len() <= MAX_REQUEST_BYTES, "inventory input bytes")?;
    let value: InventoryV2 =
        serde_json::from_slice(bytes).map_err(|_| Invalid("inventory input"))?;
    value.validate()?;
    Ok(value)
}
pub fn decode_report(bytes: &[u8]) -> Result<PresenceReport> {
    require(bytes.len() <= MAX_REQUEST_BYTES, "presence input bytes")?;
    let value: PresenceReport =
        serde_json::from_slice(bytes).map_err(|_| Invalid("presence input"))?;
    value.validate()?;
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Recorded,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PresenceReceipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub negotiation: Negotiation,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub lease_id: Uuid,
    pub sequence: u64,
    pub request_digest: Digest,
    pub inventory_at_record: InventoryIdentity,
    pub recorded_at_ms: u64,
    pub expires_at_ms: u64,
    pub disposition: Disposition,
}
impl PresenceReceipt {
    pub fn validate_for(&self, report: &PresenceReport) -> Result<()> {
        report.validate()?;
        require(
            self.schema_version == 1
                && self.operation_id == report.operation_id
                && self.identity == report.identity
                && self.negotiation.protocol == report.negotiation.protocol
                && self.negotiation.qualification == report.negotiation.qualification
                && self.binding == report.binding
                && self.grant_id == report.grant_id
                && self.lease_id == report.lease_id
                && self.sequence == report.sequence
                && self.request_digest == report.digest()?
                && digest(&self.inventory_at_record)? == digest(&report.inventory)?,
            "presence receipt scope/digest",
        )?;
        time(self.recorded_at_ms, self.expires_at_ms, report.ttl_ms)?;
        bytes(self, MAX_RECEIPT_BYTES)
    }
}
/// Independent schemas; relational checks and actual owner authorization remain
/// mandatory in the decoder/daemon even when a JSON schema validates.
pub fn inventory_schema() -> serde_json::Value {
    bounded_schema(
        serde_json::to_value(schemars::schema_for!(InventoryV2)).expect("inventory schema"),
    )
}
pub fn report_schema() -> serde_json::Value {
    bounded_schema(
        serde_json::to_value(schemars::schema_for!(PresenceReport)).expect("presence schema"),
    )
}
pub fn receipt_schema() -> serde_json::Value {
    let mut value = bounded_schema(
        serde_json::to_value(schemars::schema_for!(PresenceReceipt))
            .expect("presence receipt schema"),
    );
    value["x-max-document-bytes"] = MAX_RECEIPT_BYTES.into();
    value
}
pub fn inventory_receipt_schema() -> serde_json::Value {
    let mut value = bounded_schema(
        serde_json::to_value(schemars::schema_for!(InventoryReceipt))
            .expect("inventory receipt schema"),
    );
    value["x-max-document-bytes"] = MAX_RECEIPT_BYTES.into();
    value
}
pub fn qualification_schema() -> serde_json::Value {
    let mut value = bounded_schema(
        serde_json::to_value(schemars::schema_for!(Qualification))
            .expect("presence qualification schema"),
    );
    value["x-max-document-bytes"] = 8192.into();
    value
}
fn bounded_schema(mut value: serde_json::Value) -> serde_json::Value {
    fn visit(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(item)
                }
            }
            serde_json::Value::Object(object) => {
                for (_, v) in object.iter_mut() {
                    visit(v)
                }
                if let Some(defs) = object
                    .get_mut("$defs")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    if let Some(text) = defs
                        .get_mut("Text")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        text.insert("minLength".into(), 1.into());
                        text.insert("maxLength".into(), 512.into());
                        text.insert("x-utf8-byte-limit".into(), 512.into());
                    }
                    if let Some(label) = defs
                        .get_mut("Label")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        label.insert("x-utf8-byte-limit".into(), 512.into());
                    }
                    if let Some(hash) = defs
                        .get_mut("Digest")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        hash.insert("pattern".into(), "^[0-9a-f]{64}$".into());
                        hash.insert("minLength".into(), 64.into());
                        hash.insert("maxLength".into(), 64.into());
                        hash.insert("x-utf8-byte-limit".into(), 64.into());
                    }
                }
                if let Some(props) = object
                    .get_mut("properties")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    for (name, property) in props {
                        if let Some(p) = property.as_object_mut() {
                            if name.ends_with("_ns") || name.ends_with("_at_ms") {
                                p.insert("minimum".into(), 1.into());
                                p.insert("maximum".into(), u64::MAX.into());
                            }
                            match name.as_str() {
                                "schema_version" => {
                                    p.insert("const".into(), 1.into());
                                }
                                "ttl_ms" | "maximum_ttl_ms" | "maximum_age_ms" => {
                                    p.insert("minimum".into(), 1000.into());
                                    p.insert("maximum".into(), MAX_TTL_MS.into());
                                }
                                "sequence" | "revision" | "grant_revision" => {
                                    p.insert("minimum".into(), 1.into());
                                    p.insert("maximum".into(), (i64::MAX as u64).into());
                                }
                                "observed_material_bytes" => {
                                    p.insert("minimum".into(), 1.into());
                                    p.insert("maximum".into(), 65_536.into());
                                }
                                "qualification" => {
                                    p.insert("properties".into(),serde_json::json!({"id":{"pattern":"^aq[.]scoped-material-presence-qualification/.+"}}));
                                }
                                "qualifications" => {
                                    p.insert("minItems".into(), 1.into());
                                    p.insert("maxItems".into(), 16.into());
                                }
                                "provisionable_only" => {
                                    p.insert("maxItems".into(), 32.into());
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    visit(&mut value);
    value["x-max-document-bytes"] = MAX_REQUEST_BYTES.into();
    value["x-relational-validation-required"] = true.into();
    value
}
