//! Pure preparation contracts. Validation never establishes authorization,
//! retention, secret ownership, a capacity reservation, or an execution grant.
pub mod execution_spec;
mod placement;
pub mod started;
pub mod stopped;
pub mod transport;
use aq_session_protocol::Validate;
pub use aq_session_protocol::{
    ArtifactRef, Digest, LaunchBinding, ProfiledOffer, SessionIdentity, Text,
};
pub use placement::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, thiserror::Error)]
#[error("invalid admission contract: {0}")]
pub struct Invalid(pub &'static str);
pub type Result<T> = std::result::Result<T, Invalid>;
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(Invalid(message)) }
}
fn id(value: Uuid) -> Result<()> {
    require(!value.is_nil(), "nil identity")
}
fn revision(value: u64) -> Result<()> {
    require(value > 0 && value <= i64::MAX as u64, "revision range")
}
fn time(issued: u64, expires: u64, max: u64) -> Result<()> {
    require(
        issued > 0 && expires > issued && expires - issued <= max,
        "time interval",
    )
}
fn artifact(value: &ArtifactRef) -> Result<()> {
    require(
        value.id.as_str().len() <= 512 && value.version.as_str().len() <= 512,
        "artifact reference byte bound",
    )?;
    value
        .id
        .validate()
        .map_err(|_| Invalid("artifact identity"))?;
    value
        .version
        .validate()
        .map_err(|_| Invalid("artifact version"))
}
fn current(issued: u64, expires: u64, now: u64) -> Result<()> {
    require(issued <= now && now < expires, "expired/future evidence")
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Label(#[schemars(length(min = 1, max = 512))] String);
impl Label {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        require(
            !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
            "logical reference",
        )?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TaskRevision {
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub version: u64,
    pub content_revision: u64,
}
impl TaskRevision {
    pub fn validate(&self) -> Result<()> {
        id(self.workspace_id)?;
        id(self.task_id)?;
        revision(self.version)?;
        revision(self.content_revision)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectId {
    pub format: ObjectFormat,
    pub hex: String,
}
impl ObjectId {
    pub fn validate(&self) -> Result<()> {
        require(
            self.hex.len()
                == match self.format {
                    ObjectFormat::Sha1 => 40,
                    ObjectFormat::Sha256 => 64,
                }
                && self.hex.bytes().any(|b| b != b'0')
                && self
                    .hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "object ID format",
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceSelection {
    Immutable { object: ObjectId },
    NamedRef { reference: Label },
}
impl SourceSelection {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Immutable { object } => object.validate(),
            Self::NamedRef { reference } => {
                let s = reference.as_str();
                require(
                    (s.starts_with("refs/heads/") || s.starts_with("refs/tags/"))
                        && !s.ends_with('/')
                        && !s.ends_with('.')
                        && !s.contains("..")
                        && !s.contains("@{")
                        && !s
                            .chars()
                            .any(|c| c.is_whitespace() || "~^:?*[\\".contains(c))
                        && s.split('/')
                            .all(|p| !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock")),
                    "explicit safe Git ref",
                )
            }
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceAdmission {
    pub schema_version: u32,
    pub project_id: Uuid,
    pub project_revision: u64,
    pub repository_id: Uuid,
    pub repository_binding_generation: u64,
    pub policy: ArtifactRef,
    pub selection: SourceSelection,
    pub resolved_object: ObjectId,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
}
impl SourceAdmission {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "source version")?;
        id(self.project_id)?;
        id(self.repository_id)?;
        revision(self.project_revision)?;
        revision(self.repository_binding_generation)?;
        artifact(&self.policy)?;
        self.selection.validate()?;
        self.resolved_object.validate()?;
        if let SourceSelection::Immutable { object } = &self.selection {
            require(object == &self.resolved_object, "immutable source mismatch")?;
        }
        time(self.observed_at_ms, self.expires_at_ms, 30_000)
    }
    pub fn at(&self, now: u64) -> Result<()> {
        self.validate()?;
        current(self.observed_at_ms, self.expires_at_ms, now)
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Source,
    Context,
    Outcome,
    Composition,
    Selection,
    Definition,
}
impl ArtifactKind {
    fn limit(self) -> u64 {
        match self {
            Self::Source => 1_073_741_824,
            Self::Context | Self::Composition | Self::Definition => 2_097_152,
            Self::Outcome => 65_536,
            Self::Selection => 1_048_576,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifact {
    pub artifact: ArtifactRef,
    pub kind: ArtifactKind,
    pub bytes: u64,
    pub retention_receipt: ArtifactRef,
    pub retained_at_ms: u64,
    pub retain_until_ms: u64,
}
impl RetainedArtifact {
    pub fn validate(&self) -> Result<()> {
        artifact(&self.artifact)?;
        artifact(&self.retention_receipt)?;
        require(
            self.bytes > 0 && self.bytes <= self.kind.limit(),
            "artifact byte bound",
        )?;
        time(self.retained_at_ms, self.retain_until_ms, 31_536_000_000)
    }
    pub fn verify(&self, bytes: &[u8], now: u64) -> Result<()> {
        self.validate()?;
        current(self.retained_at_ms, self.retain_until_ms, now)?;
        require(
            bytes.len() as u64 == self.bytes && Digest::of(bytes) == self.artifact.digest,
            "artifact content mismatch",
        )
    }
}
/// Private authenticated lease metadata; not a public prompt/profile artifact.
/// The fingerprint binds exact bytes, never a claim inferred from key presence.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialLease {
    pub schema_version: u32,
    pub lease_id: Uuid,
    pub binding: LaunchBinding,
    pub credential_profile: ArtifactRef,
    pub credential_revision: u64,
    pub provider: Label,
    pub account_reference: Label,
    pub endpoint_reference: Label,
    pub purpose: Label,
    pub material_digest: Digest,
    pub material_bytes: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}
impl MaterialLease {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "material version")?;
        id(self.lease_id)?;
        self.binding
            .validate()
            .map_err(|_| Invalid("launch binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote material lease requires remote authority",
        )?;
        artifact(&self.credential_profile)?;
        revision(self.credential_revision)?;
        require(
            self.material_bytes > 0 && self.material_bytes <= 65_536,
            "material byte bound",
        )?;
        time(self.issued_at_ms, self.expires_at_ms, 120_000)
    }
    pub fn verify_material(&self, binding: &LaunchBinding, bytes: &[u8], now: u64) -> Result<()> {
        self.validate()?;
        current(self.issued_at_ms, self.expires_at_ms, now)?;
        require(binding == &self.binding, "material binding mismatch")?;
        require(
            bytes.len() as u64 == self.material_bytes && Digest::of(bytes) == self.material_digest,
            "material digest/size mismatch",
        )
    }
    pub fn validate_offer(&self, offer: &ProfiledOffer, now: u64) -> Result<()> {
        self.validate()?;
        offer.validate().map_err(|_| Invalid("profiled offer"))?;
        current(self.issued_at_ms, self.expires_at_ms, now)?;
        require(
            self.binding == offer.binding
                && self.provider.as_str() == offer.manifest.model.provider.as_str()
                && offer.manifest.credentials.iter().any(|c| {
                    c.credential_profile == self.credential_profile
                        && c.purpose.as_str() == self.purpose.as_str()
                }),
            "material offer scope mismatch",
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MaterialReceipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub lease_id: Uuid,
    pub binding: LaunchBinding,
    pub material_digest: Digest,
    pub private_binding: ArtifactRef,
    pub received_at_ms: u64,
}
impl MaterialReceipt {
    pub fn validate_against(&self, lease: &MaterialLease, now: u64) -> Result<()> {
        lease.validate()?;
        require(self.schema_version == 1, "receipt version")?;
        id(self.operation_id)?;
        artifact(&self.private_binding)?;
        current(lease.issued_at_ms, lease.expires_at_ms, now)?;
        require(
            self.lease_id == lease.lease_id
                && self.binding == lease.binding
                && self.material_digest == lease.material_digest
                && self.received_at_ms >= lease.issued_at_ms
                && self.received_at_ms <= now
                && self.received_at_ms < lease.expires_at_ms,
            "material receipt binding/time",
        )
    }
}
/// Check every manifest content/definition reference has a current descriptor.
/// The caller separately authenticates its owner and verifies actual retention.
pub fn verify_offer_artifacts(
    offer: &ProfiledOffer,
    artifacts: &[RetainedArtifact],
    now: u64,
) -> Result<()> {
    offer.validate().map_err(|_| Invalid("profiled offer"))?;
    require(artifacts.len() <= 128, "artifact cardinality")?;
    let mut seen = std::collections::BTreeSet::new();
    for a in artifacts {
        a.validate()?;
        current(a.retained_at_ms, a.retain_until_ms, now)?;
        require(
            seen.insert((a.artifact.id.clone(), a.artifact.version.clone())),
            "duplicate artifact identity/version",
        )?;
    }
    let m = &offer.manifest;
    for (reference, kind) in [
        (&m.bindings.source, ArtifactKind::Source),
        (&m.bindings.outcome, ArtifactKind::Outcome),
        (&m.selection_explanation, ArtifactKind::Selection),
    ] {
        require(
            artifacts
                .iter()
                .any(|a| a.artifact == *reference && a.kind == kind),
            "artifact role mismatch",
        )?;
    }
    let mut expected = vec![
        &m.catalog,
        &m.agent_profile,
        &m.intelligence_profile,
        &m.execution_profile,
        &m.bindings.source,
        &m.bindings.outcome,
        &m.bindings.graph_access_profile,
        &m.selection_explanation,
        &m.harness.adapter,
        &m.harness.environment,
    ];
    expected.extend(&m.modules);
    expected.extend(m.context.references.iter().map(|c| &c.artifact));
    expected.extend(m.credentials.iter().map(|c| &c.credential_profile));
    for reference in expected {
        require(
            artifacts.iter().any(|a| a.artifact == *reference),
            "required retained artifact absent",
        )?;
    }
    for context in &m.context.references {
        require(
            artifacts
                .iter()
                .any(|a| a.artifact == context.artifact && a.bytes == context.bytes),
            "context descriptor byte mismatch",
        )?;
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContractDocument {
    SourceAdmission(SourceAdmission),
    RetainedArtifact(RetainedArtifact),
    MaterialLease(MaterialLease),
    MaterialReceipt(MaterialReceipt),
    PlacementInput(PlacementInput),
    PlacementOutput(PlacementOutput),
}
/// Decode typed JSON first so duplicate known mutation fields are rejected.
pub fn decode(bytes: &[u8]) -> Result<ContractDocument> {
    require(bytes.len() <= 524_288, "document byte bound")?;
    let value: ContractDocument =
        serde_json::from_slice(bytes).map_err(|_| Invalid("JSON shape"))?;
    match &value {
        ContractDocument::SourceAdmission(v) => v.validate()?,
        ContractDocument::RetainedArtifact(v) => v.validate()?,
        ContractDocument::MaterialLease(v) => v.validate()?,
        ContractDocument::MaterialReceipt(v) => {
            id(v.operation_id)?;
            id(v.lease_id)?;
            v.binding
                .validate()
                .map_err(|_| Invalid("receipt binding"))?;
            require(
                v.schema_version == 1 && v.received_at_ms > 0,
                "receipt version/time",
            )?;
            artifact(&v.private_binding)?;
        }
        ContractDocument::PlacementInput(v) => v.validate()?,
        ContractDocument::PlacementOutput(v) => v.validate()?,
    };
    Ok(value)
}
pub fn contract_schema() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(ContractDocument)).expect("schema");
    fn walk(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(o) => {
                if o.get("type") == Some(&serde_json::json!("string")) {
                    o.entry("maxLength").or_insert(512.into());
                    o.insert("x-utf8-maxBytes".into(), o["maxLength"].clone());
                }
                if o.get("type") == Some(&serde_json::json!("array")) {
                    o.entry("maxItems").or_insert(256.into());
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
            label.insert("allOf".into(),serde_json::json!([{"pattern":"\\S"},{"not":{"pattern":"[\\u0000-\\u001f\\u007f]"}}]));
        }
        for definition in defs.values_mut() {
            if let Some(properties) = definition
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
            {
                for (name, schema) in properties.iter_mut() {
                    if let Some(s) = schema.as_object_mut() {
                        if name == "schema_version" {
                            s.insert("const".into(), 1.into());
                        }
                        if name == "authority_admission" {
                            s.insert("const".into(), false.into());
                        }
                        if (name.ends_with("_revision")
                            || name == "revision"
                            || name == "version"
                            || name.ends_with("_generation"))
                            && s.get("type") == Some(&serde_json::json!("integer"))
                        {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), i64::MAX.into());
                        }
                        if name == "hex" {
                            s.insert("pattern".into(), "^([0-9a-f]{40}|[0-9a-f]{64})$".into());
                            s.insert("minLength".into(), 40.into());
                            s.insert("maxLength".into(), 64.into());
                            s.insert("x-utf8-maxBytes".into(), 64.into());
                        }
                        if name == "material_bytes" {
                            s.insert("minimum".into(), 1.into());
                            s.insert("maximum".into(), 65_536.into());
                        }
                        if name.ends_with("_at_ms") || name.ends_with("_until_ms") {
                            s.insert("minimum".into(), 1.into());
                        }
                        if name == "reason_codes" {
                            s.insert("maxItems".into(), 16.into());
                        }
                        if name == "nodes" || name == "proposed" {
                            s.insert("maxItems".into(), 128.into());
                        }
                    }
                }
            }
        }
    }
    value
}

pub mod outcome;

pub mod result_record;

pub mod startup;

pub mod control;

pub mod granted_closure;

pub mod preparation_v2;

pub mod scoped_presence;

pub mod preparation_refresh_v1;
mod preparation_shared;

pub mod sandbox;
