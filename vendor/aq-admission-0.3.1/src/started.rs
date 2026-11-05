//! Additive host-start report. Structural validation is not kernel attestation.
//! The qualified worker owner must persist the actual Running observation and
//! its mapping to the pre-grant logical reservation before transmitting it.
use crate::*;

pub const MAX_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessEvidence {
    pub pid: u32,
    pub start_ticks: u64,
}
impl ProcessEvidence {
    fn validate(&self) -> Result<()> {
        require(
            self.pid > 0 && self.start_ticks > 0,
            "host process identity",
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchIntent {
    pub launch_id: Uuid,
    pub session: SessionIdentity,
    pub manifest_digest: Digest,
    pub configuration_digest: Digest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuardianIdentity {
    pub intent: LaunchIntent,
    /// Physical guardian nonce allocated by aq-session-host after permission to
    /// launch. This is distinct from Request.guardian_id's pre-grant owner.
    pub nonce: Uuid,
    pub process: ProcessEvidence,
    pub machine_boot_id: Uuid,
    pub pid_namespace: Label,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PaneIdentity {
    pub server: ProcessEvidence,
    pub pane: ProcessEvidence,
    pub pane_id: Label,
    pub session_name: Label,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunningPhase {
    Running,
}
/// The exact public HostObservation JSON shape, restricted to a genuine live
/// Running snapshot. Unknown/starting/stopped reports cannot decode as this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunningObservation {
    pub guardian: GuardianIdentity,
    pub phase: RunningPhase,
    pub pane: PaneIdentity,
    pub permit_sequence: u64,
    pub permit_deadline_boot_ms: u64,
    pub observed_boot_ms: u64,
    pub reaped_count: u64,
    pub kernel_wait_echild: bool,
    pub root_status: Option<i32>,
    pub stop_reason: Option<Label>,
}
impl RunningObservation {
    pub fn validate(&self) -> Result<()> {
        id(self.guardian.intent.launch_id)?;
        self.guardian
            .intent
            .session
            .validate()
            .map_err(|_| Invalid("host session"))?;
        id(self.guardian.nonce)?;
        id(self.guardian.machine_boot_id)?;
        self.guardian.process.validate()?;
        self.pane.server.validate()?;
        self.pane.pane.validate()?;
        revision(self.permit_sequence)?;
        require(
            self.observed_boot_ms > 0
                && self.observed_boot_ms < self.permit_deadline_boot_ms
                && self.permit_deadline_boot_ms <= i64::MAX as u64
                && !self.kernel_wait_echild
                && self.root_status.is_none()
                && self.stop_reason.is_none(),
            "live host observation required",
        )?;
        let pane = self.pane.pane_id.as_str();
        require(
            pane.len() <= 32
                && pane
                    .strip_prefix('%')
                    .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                && self.pane.session_name.as_str().len() <= 128
                && self.guardian.pid_namespace.as_str().starts_with("pid:[")
                && self.guardian.pid_namespace.as_str().ends_with(']')
                && self.guardian.process.pid != self.pane.server.pid
                && self.guardian.process.pid != self.pane.pane.pid
                && self.pane.server.pid != self.pane.pane.pid,
            "host guardian/tmux identity",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        canonical_digest(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    /// The original durable started operation, never a new ID on retry/renewal.
    pub operation_id: Uuid,
    pub binding: LaunchBinding,
    pub grant_id: Uuid,
    /// Logical launch/reservation owner established before the grant.
    pub guardian_id: Uuid,
    pub launch_id: Uuid,
    pub reservation_id: Uuid,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub journal_revision: u64,
    pub journal_digest: Digest,
    pub observation: RunningObservation,
    pub observation_digest: Digest,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "started version")?;
        for value in [
            self.operation_id,
            self.grant_id,
            self.guardian_id,
            self.launch_id,
            self.reservation_id,
        ] {
            id(value)?;
        }
        self.binding
            .validate()
            .map_err(|_| Invalid("started binding"))?;
        require(
            matches!(self.binding.session, SessionIdentity::Remote { .. }),
            "remote started report",
        )?;
        artifact(&self.runtime)?;
        artifact(&self.qualification)?;
        revision(self.journal_revision)?;
        self.observation.validate()?;
        require(
            self.observation.guardian.intent.launch_id == self.launch_id
                && self.observation.guardian.intent.session == self.binding.session
                && self.observation.guardian.intent.manifest_digest
                    == self.binding.launch_manifest_digest
                && self.observation.digest()? == self.observation_digest,
            "started host/binding/digest mismatch",
        )?;
        require(
            serde_json::to_vec(self)
                .map_err(|_| Invalid("started encoding"))?
                .len()
                <= MAX_BYTES,
            "started byte bound",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        canonical_digest(self)
    }
}
/// Immutable historical acknowledgment. It grants no execution, renews no
/// deadline and does not assert that the host is still running on replay.
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
}
impl Receipt {
    pub fn validate_for(&self, request: &Request) -> Result<()> {
        require(
            self.schema_version == 1
                && self.operation_id == request.operation_id
                && self.binding == request.binding
                && self.grant_id == request.grant_id
                && self.request_digest == request.digest()?
                && self.recorded_at_ms > 0
                && self.recorded_at_ms <= i64::MAX as u64,
            "started receipt binding",
        )?;
        revision(self.authority_revision_at_record)
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    require(bytes.len() <= MAX_BYTES, "started request bound")?;
    let value: Request =
        serde_json::from_slice(bytes).map_err(|_| Invalid("started request JSON"))?;
    value.validate()?;
    Ok(value)
}
pub fn decode_receipt(bytes: &[u8], request: &Request) -> Result<Receipt> {
    require(bytes.len() <= MAX_BYTES, "started receipt bound")?;
    let value: Receipt =
        serde_json::from_slice(bytes).map_err(|_| Invalid("started receipt JSON"))?;
    value.validate_for(request)?;
    Ok(value)
}
pub fn request_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Request)).expect("started schema")
}

fn canonical_digest(value: &impl Serialize) -> Result<Digest> {
    let mut value =
        serde_json::to_value(value).map_err(|_| Invalid("started canonical encoding"))?;
    value.sort_all_objects();
    Ok(Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Invalid("started canonical encoding"))?,
    ))
}
