//! Historical granted-launch closure. Serialization/correlation checks are not
//! current authorization, fsync evidence, kernel attestation or resource release.
use crate::*;
use started::{GuardianIdentity, LaunchIntent, PaneIdentity};
use transport::GrantedLaunch;
pub const MAX_BYTES: usize = 32_768;
fn bounded(value: &impl Serialize) -> Result<()> {
    require(
        serde_json::to_vec(value)
            .map_err(|_| Invalid("closure encoding"))?
            .len()
            <= MAX_BYTES,
        "closure byte bound",
    )
}
fn domain_digest(domain: &str, value: &impl Serialize) -> Result<Digest> {
    let mut value =
        serde_json::to_value((domain, value)).map_err(|_| Invalid("closure canonical value"))?;
    value.sort_all_objects();
    Ok(Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Invalid("closure canonical encoding"))?,
    ))
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JournalCheckpoint {
    pub revision: u64,
    pub digest: Digest,
}
impl JournalCheckpoint {
    fn validate(&self) -> Result<()> {
        revision(self.revision)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuppressionRecord {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub suppression_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
    pub original_kernel_boot_id: Uuid,
    pub recording_kernel_boot_id: Uuid,
    pub previous: JournalCheckpoint,
    pub terminal_revision: u64,
}
impl SuppressionRecord {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "suppression version")?;
        self.binding
            .validate()
            .map_err(|_| Invalid("suppression binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote suppression owner",
        )?;
        for v in [
            self.operation_id,
            self.suppression_id,
            self.grant_id,
            self.guardian_id,
            self.launch_id,
            self.reservation_id,
            self.original_kernel_boot_id,
            self.recording_kernel_boot_id,
        ] {
            id(v)?;
        }
        self.previous.validate()?;
        revision(self.terminal_revision)?;
        require(
            self.terminal_revision > self.previous.revision,
            "terminal suppression progression",
        )?;
        bounded(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        domain_digest("aq.launch-suppression.v1", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriverIntent {
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub guardian_id: Uuid,
    pub reservation_id: Uuid,
    pub host: LaunchIntent,
    pub journal: JournalCheckpoint,
}
impl DriverIntent {
    fn validate(&self) -> Result<()> {
        self.binding
            .validate()
            .map_err(|_| Invalid("driver binding"))?;
        for v in [
            self.grant_id,
            self.guardian_id,
            self.reservation_id,
            self.host.launch_id,
        ] {
            id(v)?;
        }
        self.journal.validate()?;
        require(
            self.host.session == self.binding.session
                && self.host.manifest_digest == self.binding.launch_manifest_digest,
            "driver host intent binding",
        )
    }
}
fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
/// Exact host JSON shape with legitimately absent pre-start identities. No
/// placeholder pane/root status or invented positive reap count is permitted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoppedObservation {
    pub guardian: GuardianIdentity,
    pub phase: stopped::StoppedPhase,
    #[serde(deserialize_with = "required_nullable")]
    pub pane: Option<PaneIdentity>,
    pub permit_sequence: u64,
    pub permit_deadline_boot_ms: u64,
    pub observed_boot_ms: u64,
    pub reaped_count: u64,
    pub kernel_wait_echild: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub root_status: Option<i32>,
    #[serde(deserialize_with = "required_nullable")]
    pub stop_reason: Option<Label>,
}
impl StoppedObservation {
    pub fn validate(&self) -> Result<()> {
        id(self.guardian.intent.launch_id)?;
        id(self.guardian.nonce)?;
        id(self.guardian.machine_boot_id)?;
        self.guardian
            .intent
            .session
            .validate()
            .map_err(|_| Invalid("closed host session"))?;
        require(
            matches!(self.guardian.intent.session, SessionIdentity::Remote { .. }),
            "remote closed host",
        )?;
        require(
            self.guardian.process.pid > 0
                && self.guardian.process.pid <= i32::MAX as u32
                && self.guardian.process.start_ticks > 0
                && self.guardian.process.start_ticks <= i64::MAX as u64,
            "guardian birth",
        )?;
        revision(self.permit_sequence)?;
        revision(self.permit_deadline_boot_ms)?;
        revision(self.observed_boot_ms)?;
        require(
            self.kernel_wait_echild
                && self.reaped_count <= i64::MAX as u64
                && (self.root_status.is_none() || self.reaped_count > 0),
            "actual stopped ECHILD shape",
        )?;
        let namespace = self.guardian.pid_namespace.as_str();
        require(
            namespace
                .strip_prefix("pid:[")
                .and_then(|s| s.strip_suffix(']'))
                .is_some_and(|s| {
                    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
                }),
            "PID namespace identity",
        )?;
        if let Some(pane) = &self.pane {
            for p in [&pane.server, &pane.pane] {
                require(
                    p.pid > 0
                        && p.pid <= i32::MAX as u32
                        && p.start_ticks > 0
                        && p.start_ticks <= i64::MAX as u64,
                    "tmux process birth",
                )?;
            }
            require(
                pane.server.pid != self.guardian.process.pid
                    && pane.pane.pid != self.guardian.process.pid
                    && pane.server.pid != pane.pane.pid,
                "distinct owned processes",
            )?;
            require(
                pane.pane_id.as_str().len() <= 32
                    && pane
                        .pane_id
                        .as_str()
                        .strip_prefix('%')
                        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                    && pane.session_name.as_str().len() <= 128,
                "tmux pane identity",
            )?;
        }
        bounded(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostStoppedProof {
    pub observation: StoppedObservation,
}
impl HostStoppedProof {
    pub fn digest(&self) -> Result<Digest> {
        self.observation.validate()?;
        domain_digest("aq.granted-closure.host-proof.v1", self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evidence {
    Suppressed {
        record: SuppressionRecord,
        record_digest: Digest,
    },
    HostStopped {
        driver_intent: Box<DriverIntent>,
        proof: HostStoppedProof,
        proof_digest: Digest,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub journal: JournalCheckpoint,
    pub evidence: Evidence,
}
impl Request {
    /// Shape and internal correlation only. The authenticated owner separately
    /// proves current node authority, original history and absence of Started.
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "closure version")?;
        for v in [
            self.operation_id,
            self.grant_id,
            self.guardian_id,
            self.launch_id,
            self.reservation_id,
        ] {
            id(v)?;
        }
        self.binding
            .validate()
            .map_err(|_| Invalid("closure binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote closure",
        )?;
        artifact(&self.runtime)?;
        artifact(&self.qualification)?;
        self.journal.validate()?;
        match &self.evidence {
            Evidence::Suppressed {
                record,
                record_digest,
            } => {
                record.validate()?;
                require(
                    record.operation_id == self.operation_id
                        && record.binding == self.binding
                        && record.grant_id == self.grant_id
                        && record.guardian_id == self.guardian_id
                        && record.launch_id == self.launch_id
                        && record.reservation_id == self.reservation_id
                        && record.terminal_revision == self.journal.revision
                        && record.digest()? == *record_digest,
                    "suppression correlation",
                )?;
            }
            Evidence::HostStopped {
                driver_intent,
                proof,
                proof_digest,
            } => {
                driver_intent.validate()?;
                proof.observation.validate()?;
                require(
                    driver_intent.binding == self.binding
                        && driver_intent.grant_id == self.grant_id
                        && driver_intent.guardian_id == self.guardian_id
                        && driver_intent.reservation_id == self.reservation_id
                        && driver_intent.host.launch_id == self.launch_id
                        && proof.observation.guardian.intent == driver_intent.host
                        && driver_intent.journal.revision < self.journal.revision
                        && proof.digest()? == *proof_digest,
                    "stopped closure correlation",
                )?;
            }
        }
        bounded(self)
    }
    /// Trusted retained facts, not a context decoded from this request. This
    /// accepts historical expiry and never grants permission to resume execution.
    pub fn validate_against(&self, context: &GrantedContext) -> Result<()> {
        self.validate()?;
        context.grant.validate()?;
        artifact(&context.runtime)?;
        artifact(&context.qualification)?;
        context.prepared.validate()?;
        let original = &context.grant;
        require(
            self.binding == original.grant.binding
                && self.grant_id == original.grant.grant_id
                && self.guardian_id == original.guardian_id
                && self.launch_id == original.launch_id
                && self.reservation_id == original.reservation_id
                && self.runtime == context.runtime
                && self.qualification == context.qualification
                && self.journal.revision > context.prepared.revision,
            "original granted closure owner",
        )?;
        match &self.evidence {
            Evidence::Suppressed { record, .. } => require(
                record.previous.revision >= context.prepared.revision
                    && (record.previous.revision != context.prepared.revision
                        || record.previous.digest == context.prepared.digest),
                "suppression prepared history",
            ),
            Evidence::HostStopped { driver_intent, .. } => require(
                driver_intent.journal.revision > context.prepared.revision,
                "driver intent after Prepared",
            ),
        }
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        domain_digest("aq.granted-launch-closure.request.v1", self)
    }
}
/// No Serde implementation: callers construct from actual locked immutable
/// owners. Correlation is not current machine authentication or no-Started proof.
pub struct GrantedContext {
    pub grant: GrantedLaunch,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub prepared: JournalCheckpoint,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    LaunchClosedAwaitingSettlement,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    pub request_digest: Digest,
    pub recorded_at_ms: u64,
    pub authority_revision_at_record: u64,
    pub disposition: Disposition,
}
impl Receipt {
    pub fn validate_for(&self, request: &Request) -> Result<()> {
        request.validate()?;
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.binding == request.binding
                && self.grant_id == request.grant_id
                && self.request_digest == request.digest()?,
            "closure receipt binding",
        )?;
        revision(self.recorded_at_ms)?;
        revision(self.authority_revision_at_record)?;
        bounded(self)
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    require(bytes.len() <= MAX_BYTES, "closure request bound")?;
    let value: Request =
        serde_json::from_slice(bytes).map_err(|_| Invalid("closure request JSON"))?;
    value.validate()?;
    Ok(value)
}
pub fn decode_receipt(bytes: &[u8], request: &Request) -> Result<Receipt> {
    require(bytes.len() <= MAX_BYTES, "closure receipt bound")?;
    let value: Receipt =
        serde_json::from_slice(bytes).map_err(|_| Invalid("closure receipt JSON"))?;
    value.validate_for(request)?;
    Ok(value)
}
fn tighten(mut value: serde_json::Value) -> serde_json::Value {
    fn visit(v: &mut serde_json::Value) {
        if v.get("properties")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|p| {
                p.contains_key("guardian")
                    && p.contains_key("pane")
                    && p.contains_key("permit_sequence")
            })
        {
            let required = v
                .as_object_mut()
                .expect("schema object")
                .entry("required")
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .expect("required array");
            for name in ["pane", "root_status", "stop_reason"] {
                let value = serde_json::Value::String(name.into());
                if !required.contains(&value) {
                    required.push(value)
                }
            }
        }
        if let Some(properties) = v
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            for (name, schema) in properties {
                if name == "schema_version" {
                    schema["const"] = 1.into();
                }
                if name == "kernel_wait_echild" {
                    schema["const"] = true.into();
                }
                if matches!(
                    name.as_str(),
                    "revision"
                        | "terminal_revision"
                        | "permit_sequence"
                        | "permit_deadline_boot_ms"
                        | "observed_boot_ms"
                        | "start_ticks"
                        | "recorded_at_ms"
                        | "authority_revision_at_record"
                ) {
                    schema["minimum"] = 1.into();
                    schema["maximum"] = i64::MAX.into();
                }
                if name == "reaped_count" {
                    schema["maximum"] = i64::MAX.into();
                }
                if name == "pid" {
                    schema["minimum"] = 1.into();
                    schema["maximum"] = i32::MAX.into();
                }
            }
        }
        match v {
            serde_json::Value::Object(map) => {
                for child in map.values_mut() {
                    visit(child)
                }
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    visit(child)
                }
            }
            _ => {}
        }
    }
    visit(&mut value);
    value["x-max-encoded-bytes"] = MAX_BYTES.into();
    value
}
pub fn request_schema() -> serde_json::Value {
    tighten(serde_json::to_value(schemars::schema_for!(Request)).expect("closure request schema"))
}
pub fn receipt_schema() -> serde_json::Value {
    tighten(serde_json::to_value(schemars::schema_for!(Receipt)).expect("closure receipt schema"))
}
