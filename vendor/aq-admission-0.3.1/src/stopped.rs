//! Additive stopped-host report, scoped initially to a recorded Started owner.
//! Decoding is not kernel attestation: the qualified worker must transmit the
//! actual opaque aq-session-host StoppedProof after persisting its own journal.
use crate::*;
use started::{GuardianIdentity, PaneIdentity};
use transport::{JournalEvidence, StopDisposition, WorkerStopEvidence};

pub const MAX_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StoppedPhase {
    Stopped,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoppedObservation {
    pub guardian: GuardianIdentity,
    pub phase: StoppedPhase,
    pub pane: PaneIdentity,
    pub permit_sequence: u64,
    pub permit_deadline_boot_ms: u64,
    pub observed_boot_ms: u64,
    pub reaped_count: u64,
    pub kernel_wait_echild: bool,
    pub root_status: i32,
    pub stop_reason: Option<Label>,
}
impl StoppedObservation {
    pub fn validate(&self) -> Result<()> {
        id(self.guardian.intent.launch_id)?;
        self.guardian
            .intent
            .session
            .validate()
            .map_err(|_| Invalid("stopped host session"))?;
        id(self.guardian.nonce)?;
        id(self.guardian.machine_boot_id)?;
        for p in [&self.guardian.process, &self.pane.server, &self.pane.pane] {
            require(p.pid > 0 && p.start_ticks > 0, "stopped process identity")?;
        }
        revision(self.permit_sequence)?;
        revision(self.permit_deadline_boot_ms)?;
        revision(self.observed_boot_ms)?;
        revision(self.reaped_count)?;
        let pane = self.pane.pane_id.as_str();
        require(
            self.kernel_wait_echild
                && pane.len() <= 32
                && pane
                    .strip_prefix('%')
                    .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                && self.pane.session_name.as_str().len() <= 128
                && self.guardian.pid_namespace.as_str().starts_with("pid:[")
                && self.guardian.pid_namespace.as_str().ends_with(']')
                && self.guardian.process.pid != self.pane.server.pid
                && self.guardian.process.pid != self.pane.pane.pid
                && self.pane.server.pid != self.pane.pane.pid,
            "exact stopped guardian/pane and ECHILD required",
        )
    }
}
/// Exact serialized public host StoppedProof shape, narrowed to the previously
/// Running case. A pre-start stop belongs to a separate no-start recovery path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostStoppedProof {
    pub observation: StoppedObservation,
}
impl HostStoppedProof {
    pub fn digest(&self) -> Result<Digest> {
        self.observation.validate()?;
        canonical_digest(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub evidence: WorkerStopEvidence,
    pub reservation_id: Uuid,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub proof: HostStoppedProof,
}
impl Request {
    pub fn grant_id(&self) -> Result<Uuid> {
        match self.evidence.disposition {
            StopDisposition::Stopped { grant_id, .. } => Ok(grant_id),
            _ => Err(Invalid("actual stopped evidence required")),
        }
    }
    pub fn journal(&self) -> Result<(u64, &Digest)> {
        match &self.evidence.journal {
            JournalEvidence::Recorded { revision, digest } => Ok((*revision, digest)),
            _ => Err(Invalid("durable stopped journal required")),
        }
    }
    pub fn validate(&self) -> Result<()> {
        require(self.schema_version == 1, "stopped version")?;
        self.evidence.validate()?;
        id(self.reservation_id)?;
        artifact(&self.runtime)?;
        artifact(&self.qualification)?;
        let (r, _) = self.journal()?;
        revision(r)?;
        let StopDisposition::Stopped {
            stop_receipt_digest,
            ..
        } = &self.evidence.disposition
        else {
            return Err(Invalid("actual stopped evidence required"));
        };
        let o = &self.proof.observation;
        require(
            o.guardian.intent.launch_id == self.evidence.launch_id
                && o.guardian.intent.session == self.evidence.binding.session
                && o.guardian.intent.manifest_digest
                    == self.evidence.binding.launch_manifest_digest
                && &self.proof.digest()? == stop_receipt_digest,
            "stopped host binding/proof mismatch",
        )?;
        require(
            serde_json::to_vec(self)
                .map_err(|_| Invalid("stopped encoding"))?
                .len()
                <= MAX_BYTES,
            "stopped byte bound",
        )
    }
    /// Validate linkage to the actual immutable started record, not a worker-
    /// supplied previous observation. This still requires owner authentication.
    pub fn validate_after(&self, previous: &started::Request) -> Result<()> {
        self.validate()?;
        previous.validate()?;
        let now = &self.proof.observation;
        let before = &previous.observation;
        require(
            self.evidence.binding == previous.binding
                && self.evidence.guardian_id == previous.guardian_id
                && self.evidence.launch_id == previous.launch_id
                && self.reservation_id == previous.reservation_id
                && self.grant_id()? == previous.grant_id
                && self.runtime == previous.runtime
                && self.qualification == previous.qualification
                && self.journal()?.0 > previous.journal_revision
                && now.guardian == before.guardian
                && now.pane == before.pane
                && now.permit_sequence >= before.permit_sequence
                && (now.permit_sequence != before.permit_sequence
                    || now.permit_deadline_boot_ms == before.permit_deadline_boot_ms)
                && now.observed_boot_ms >= before.observed_boot_ms
                && now.reaped_count >= before.reaped_count,
            "stopped proof differs from original started owner or regresses",
        )
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        canonical_digest(self)
    }
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
}
impl Receipt {
    pub fn validate_for(&self, request: &Request) -> Result<()> {
        require(
            self.schema_version == 1
                && self.operation_id == request.evidence.stop_operation_id
                && self.binding == request.evidence.binding
                && self.grant_id == request.grant_id()?
                && self.request_digest == request.digest()?,
            "stopped receipt binding",
        )?;
        revision(self.recorded_at_ms)?;
        revision(self.authority_revision_at_record)
    }
    pub fn artifact(&self) -> Result<ArtifactRef> {
        Ok(ArtifactRef {
            id: Text::new(format!("aq.native-stopped/{}", self.binding.attempt_id))
                .map_err(|_| Invalid("stopped receipt identity"))?,
            version: Text::new("1").map_err(|_| Invalid("stopped receipt version"))?,
            digest: canonical_digest(self)?,
        })
    }
}
pub fn decode_request(bytes: &[u8]) -> Result<Request> {
    require(bytes.len() <= MAX_BYTES, "stopped request bound")?;
    let request: Request =
        serde_json::from_slice(bytes).map_err(|_| Invalid("stopped request JSON"))?;
    request.validate()?;
    Ok(request)
}
pub fn decode_receipt(bytes: &[u8], request: &Request) -> Result<Receipt> {
    require(bytes.len() <= MAX_BYTES, "stopped receipt bound")?;
    let receipt: Receipt =
        serde_json::from_slice(bytes).map_err(|_| Invalid("stopped receipt JSON"))?;
    receipt.validate_for(request)?;
    Ok(receipt)
}
pub fn request_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Request)).expect("stopped schema")
}
fn canonical_digest(value: &impl Serialize) -> Result<Digest> {
    let mut value =
        serde_json::to_value(value).map_err(|_| Invalid("stopped canonical encoding"))?;
    value.sort_all_objects();
    Ok(Digest::of(
        &serde_json::to_vec(&value).map_err(|_| Invalid("stopped canonical encoding"))?,
    ))
}
