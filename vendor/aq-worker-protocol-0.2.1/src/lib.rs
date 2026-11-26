//! AQ execution protocol 2. Foundation enrollment/heartbeat/probe remain v1.
//! Rust types are the schema source. `examples/schema.rs` emits the checked-in schema.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const EXECUTION_PROTOCOL_VERSION: u32 = 2;
pub const MAX_BATCH: usize = 64;
pub const MAX_CAPABILITIES: usize = 32;
pub const MAX_TEXT: usize = 4096;
pub const MAX_GRANT_MS: u64 = 120_000;
pub const FAKE_HARNESS_CAPABILITY: &str = "deterministic-agent-v1";

macro_rules! wire {
    ($($name:ident { $($field:ident : $ty:ty),* $(,)? })*) => {$ (
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    )*};
}
#[cfg(feature = "native-git-author")]
pub mod git_author;
pub mod m4;
#[cfg(feature = "native-git-author")]
pub mod retained_git;
pub use m4::*;

wire! {
    NodeIdentity { node_id: Uuid, boot_id: Uuid }
    AttemptRef { attempt_id: Uuid, session_id: Uuid, generation: u64 }
    Resources { slots: u32, cpu_millis: u64, memory_bytes: u64, disk_bytes: u64 }
    RegisterRequest { identity: NodeIdentity, operation_id: Uuid, protocol_version: u32, worker_version: String, capabilities: Vec<String> }
    RegisterResponse { identity: NodeIdentity, protocol_version: u32, capabilities: Vec<String> }
    PollRequest { identity: NodeIdentity, operation_id: Uuid, inventory_sequence: u64, available: Resources, max_items: u32 }
    PollResponse { server_time_ms: u64, offers: Vec<Offer>, directives: Vec<Directive> }

    Offer { offer_id: Uuid, identity: NodeIdentity, attempt: AttemptRef, spec: ExecutionSpec, offer_expires_at_ms: u64, reservation_ttl_ms: u64 }
    AcceptRequest { identity: NodeIdentity, operation_id: Uuid, offer_id: Uuid, attempt: AttemptRef, resolved_spec_digest: String, reservation_id: Uuid, reservation_deadline_ms: u64, reservation_remaining_ms: u64 }
    Grant { identity: NodeIdentity, attempt: AttemptRef, offer_id: Uuid, reservation_id: Uuid, resolved_spec_digest: String, grant_sequence: u64, valid_for_ms: u64, reservation_deadline_ms: u64 }
    AcceptResponse { grant: Grant }
    DeclineRequest { identity: NodeIdentity, operation_id: Uuid, offer_id: Uuid, reason: String }
    RenewRequest { identity: NodeIdentity, operation_id: Uuid, attempt: AttemptRef, grant_sequence: u64 }
    RenewResponse { grant: Grant }
    Observation { identity: NodeIdentity, operation_id: Uuid, attempt: AttemptRef, sequence: u64, phase: Phase, stop_certainty: StopCertainty, detail: String }
    Ack { operation_id: Uuid }
    Directive { directive_id: Uuid, attempt: AttemptRef, kind: DirectiveKind }
    DirectiveAck { identity: NodeIdentity, operation_id: Uuid, directive_id: Uuid, attempt: AttemptRef, disposition: DirectiveDisposition }
    ArtifactRef { artifact_id: Uuid, digest: String, bytes: u64, central_receipt: Option<String> }
    ArtifactHandoffRequest { identity: NodeIdentity, operation_id: Uuid, attempt: AttemptRef, artifact: ArtifactRef, offset: u64, complete: bool }
    ArtifactHandoffResponse { operation_id: Uuid, acknowledged_offset: u64, central_receipt: Option<String> }
    ResultReport { identity: NodeIdentity, report_id: Uuid, supersedes_report_id: Option<Uuid>, attempt: AttemptRef, resolved_spec_digest: String, outcome: ProcessOutcome, stop_certainty: StopCertainty, claimed_success: bool, output: String, artifacts: Vec<ArtifactRef>, pending_operation_ids: Vec<Uuid> }
    ResultResponse { report_id: Uuid, disposition: ResultDisposition, retention_release: bool }
    ReconcileRequest { identity: NodeIdentity, operation_id: Uuid, attempts: Vec<ReconcileAttempt>, cursor: Option<String>, max_items: u32 }
    ReconcileAttempt { attempt: AttemptRef, previous_boot_id: Uuid, phase: Phase, launch_id: Option<Uuid>, stop_certainty: StopCertainty, report_id: Option<Uuid> }
    ReconcileResponse { attempts: Vec<ReconcileDisposition>, next_cursor: Option<String> }
    ReconcileDisposition { attempt: AttemptRef, action: ReconcileAction, grant: Option<Grant>, result: Option<ResultResponse> }
    ProtocolError { code: ErrorCode, message: String, retryable: bool, operation_id: Option<Uuid> }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpec {
    pub project_id: Uuid,
    pub harness: String,
    pub resolved_spec_digest: String,
    pub fixture_output: String,
    pub resources: Resources,
    pub required_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<ExecutionBindings>,
}
macro_rules! wire_enum {
    ($($name:ident { $($variant:ident),* $(,)? })*) => {$ (
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all="snake_case")]
        pub enum $name { $($variant),* }
    )*};
}
wire_enum! {
    Phase { Reserved, Authorized, Preparing, Running, Stopping, Collecting, Reporting, Acknowledged, Retired, LaunchUncertain }
    StopCertainty { NotStarted, Running, ConfirmedStopped, StopUncertain }
    ProcessOutcome { Success, Failed, Cancelled, Expired, Unknown }
    DirectiveKind { Cancel, Drain }
    DirectiveDisposition { Received, Stopped, StopUncertain }
    ResultDisposition { Accepted, RetainedCancelled, RetainedStale, Rejected }
    ReconcileAction { StopAndPreserve, AwaitReport, Acknowledged, Renewed, Unknown }
    ErrorCode { UnsupportedProtocol, UnsupportedCapability, InvalidRequest, InvalidCredential, CredentialRevoked, StaleBoot, StaleGeneration, IdConflict, OfferExpired, ReservationExpired, GrantExpired, Cancelled, CapacityUnavailable, NotFound, Overloaded, Internal }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid wire value: {0}")]
pub struct ValidationError(pub &'static str);
pub trait Validate {
    fn validate(&self) -> Result<(), ValidationError>;
}
fn check(ok: bool, reason: &'static str) -> Result<(), ValidationError> {
    if ok {
        Ok(())
    } else {
        Err(ValidationError(reason))
    }
}
fn text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max
}
fn digest(value: &str) -> bool {
    lower_hex(value, 64)
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
impl Validate for NodeIdentity {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.node_id.is_nil() && !self.boot_id.is_nil(),
            "nil node/boot identity",
        )
    }
}
impl Validate for AttemptRef {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.attempt_id.is_nil() && !self.session_id.is_nil() && self.generation > 0,
            "invalid attempt identity",
        )
    }
}
impl Validate for Resources {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.slots > 0 && self.slots <= 1024,
            "slots outside 1..1024",
        )
    }
}
fn capabilities(c: &[String]) -> bool {
    c.len() <= MAX_CAPABILITIES
        && c.iter().all(|x| text(x, 128))
        && c.iter().enumerate().all(|(i, x)| !c[..i].contains(x))
}
impl Validate for RegisterRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil()
                && self.protocol_version == EXECUTION_PROTOCOL_VERSION
                && text(&self.worker_version, 128)
                && capabilities(&self.capabilities),
            "invalid registration/version/capabilities",
        )
    }
}
impl Validate for PollRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil()
                && self.inventory_sequence > 0
                && self.max_items > 0
                && self.max_items as usize <= MAX_BATCH,
            "invalid poll bounds",
        )
    }
}
impl Validate for ExecutionSpec {
    fn validate(&self) -> Result<(), ValidationError> {
        self.resources.validate()?;
        if let Some(b) = &self.bindings {
            b.validate()?;
            check(
                b.graph.is_none()
                    || self
                        .required_capabilities
                        .iter()
                        .any(|c| c == GRAPH_ACCESS_CAPABILITY),
                "graph binding capability required",
            )?;
            check(
                b.source.is_none()
                    || self
                        .required_capabilities
                        .iter()
                        .any(|c| c == SOURCE_CAPABILITY),
                "source binding capability required",
            )?;
        }
        check(
            !self.project_id.is_nil()
                && text(&self.harness, 128)
                && digest(&self.resolved_spec_digest)
                && self.fixture_output.len() <= MAX_TEXT
                && capabilities(&self.required_capabilities),
            "invalid execution spec",
        )?;
        self.verify_digest()
    }
}
impl Validate for Offer {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        self.spec.validate()?;
        check(
            !self.offer_id.is_nil()
                && self.reservation_ttl_ms > 0
                && self.reservation_ttl_ms <= MAX_GRANT_MS,
            "invalid offer deadline",
        )
    }
}
impl Validate for AcceptRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.operation_id.is_nil()
                && !self.offer_id.is_nil()
                && !self.reservation_id.is_nil()
                && self.reservation_deadline_ms > 0
                && self.reservation_remaining_ms > 0
                && self.reservation_remaining_ms <= MAX_GRANT_MS
                && digest(&self.resolved_spec_digest),
            "invalid acceptance",
        )
    }
}
impl Validate for Grant {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.offer_id.is_nil()
                && !self.reservation_id.is_nil()
                && digest(&self.resolved_spec_digest)
                && self.reservation_deadline_ms > 0
                && self.grant_sequence > 0
                && self.valid_for_ms > 0
                && self.valid_for_ms <= MAX_GRANT_MS,
            "invalid grant",
        )
    }
}
impl Validate for Observation {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.operation_id.is_nil() && self.sequence > 0 && self.detail.len() <= MAX_TEXT,
            "invalid observation",
        )
    }
}
impl Validate for ResultReport {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.report_id.is_nil()
                && digest(&self.resolved_spec_digest)
                && self.output.len() <= MAX_TEXT
                && self.artifacts.len() <= MAX_BATCH
                && self.pending_operation_ids.len() <= MAX_BATCH,
            "invalid report bounds",
        )?;
        check(
            self.supersedes_report_id
                .is_none_or(|id| !id.is_nil() && id != self.report_id),
            "invalid superseded report identity",
        )?;
        check(
            self.pending_operation_ids
                .iter()
                .enumerate()
                .all(|(i, id)| !id.is_nil() && !self.pending_operation_ids[..i].contains(id)),
            "pending operation IDs must be nonnil and unique",
        )?;
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        Ok(())
    }
}
impl Validate for ArtifactRef {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.artifact_id.is_nil()
                && digest(&self.digest)
                && self.central_receipt.as_ref().is_none_or(|s| text(s, 256)),
            "invalid artifact",
        )
    }
}
impl Validate for ReconcileRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil()
                && self.attempts.len() <= MAX_BATCH
                && self.max_items > 0
                && self.max_items as usize <= MAX_BATCH
                && self.cursor.as_ref().is_none_or(|s| text(s, 256)),
            "invalid reconcile bounds",
        )?;
        for a in &self.attempts {
            a.validate()?;
        }
        Ok(())
    }
}
impl Validate for RenewRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.operation_id.is_nil() && self.grant_sequence > 0,
            "invalid renewal",
        )
    }
}
impl Validate for DirectiveAck {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        check(
            !self.operation_id.is_nil() && !self.directive_id.is_nil(),
            "invalid directive acknowledgement",
        )
    }
}
impl Validate for ArtifactHandoffRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        self.attempt.validate()?;
        self.artifact.validate()?;
        check(
            !self.operation_id.is_nil() && self.offset <= self.artifact.bytes,
            "invalid artifact offset",
        )
    }
}
impl Validate for DeclineRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            !self.operation_id.is_nil() && !self.offer_id.is_nil() && text(&self.reason, MAX_TEXT),
            "invalid decline",
        )
    }
}

/// Canonical wire digest: serialization of typed objects (ordered object keys).
/// Arrays retain order. Operation replay compares these canonical bytes, not raw JSON whitespace.
pub fn payload_digest<T: Serialize>(value: &T) -> String {
    let value = serde_json::to_value(value).expect("wire value must serialize");
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&value).expect("JSON must serialize"))
    )
}

/// Root union makes every message available in the single generated JSON schema.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "message", content = "body", rename_all = "snake_case")]
pub enum WireMessage {
    TransferManifest(TransferManifest),
    TransferChunkRequest(TransferChunkRequest),
    TransferStatusRequest(TransferStatusRequest),
    TransferStatus(TransferStatus),
    CentralRetentionReceipt(CentralRetentionReceipt),
    RegisterRequest(RegisterRequest),
    RegisterResponse(RegisterResponse),
    PollRequest(PollRequest),
    PollResponse(PollResponse),
    AcceptRequest(AcceptRequest),
    AcceptResponse(AcceptResponse),
    DeclineRequest(DeclineRequest),
    RenewRequest(RenewRequest),
    RenewResponse(RenewResponse),
    Observation(Observation),
    Ack(Ack),
    DirectiveAck(DirectiveAck),
    ArtifactHandoffRequest(ArtifactHandoffRequest),
    ArtifactHandoffResponse(ArtifactHandoffResponse),
    ResultReport(ResultReport),
    ResultResponse(ResultResponse),
    ReconcileRequest(ReconcileRequest),
    ReconcileResponse(ReconcileResponse),
    ProtocolError(ProtocolError),
}

pub fn schema() -> String {
    // The executable validator remains authoritative for byte-length, digest
    // binding and cross-field rules. Add portable shape bounds to the schema.
    let mut schema = serde_json::to_value(schemars::schema_for!(WireMessage)).unwrap();
    for (name, definition) in schema["$defs"].as_object_mut().unwrap() {
        if let Some(properties) = definition
            .get_mut("properties")
            .and_then(|value| value.as_object_mut())
        {
            for (field, value) in properties {
                constrain_property(name, field, value);
            }
        }
    }
    serde_json::to_string_pretty(&schema).unwrap() + "\n"
}

fn constrain_property(name: &str, field: &str, value: &mut serde_json::Value) {
    use serde_json::json;
    if field.ends_with("_id") {
        value["not"] = json!({"const": Uuid::nil()});
    }
    let constraints = match field {
        "resolved_spec_digest" | "digest" => json!({"pattern":"^[a-f0-9]{64}$"}),
        "generation" | "grant_sequence" | "sequence" | "inventory_sequence" => json!({"minimum":1}),
        "valid_for_ms" | "reservation_ttl_ms" | "reservation_remaining_ms" => {
            json!({"minimum":1, "maximum":MAX_GRANT_MS})
        }
        "max_items" => json!({"minimum":1, "maximum":MAX_BATCH}),
        "slots" if name == "Resources" => json!({"minimum":0, "maximum":1024}),
        "offers" | "directives" | "attempts" | "artifacts" => json!({"maxItems":MAX_BATCH}),
        "pending_operation_ids" => {
            if let Some(items) = value
                .get_mut("items")
                .and_then(|items| items.as_object_mut())
            {
                items.insert("not".into(), json!({"const":Uuid::nil()}));
            }
            json!({"maxItems":MAX_BATCH, "uniqueItems":true})
        }
        "capabilities" | "required_capabilities" => {
            json!({"maxItems":MAX_CAPABILITIES,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":128}})
        }
        "output" | "fixture_output" | "detail" | "reason" | "message" => {
            json!({"maxLength":MAX_TEXT})
        }
        "worker_version" | "harness" => json!({"minLength":1,"maxLength":128}),
        _ => return,
    };
    for (key, constraint) in constraints.as_object().unwrap() {
        value[key] = constraint.clone();
    }
}

impl ExecutionSpec {
    /// Digest of the entire typed spec except the digest field itself.
    pub fn computed_digest(&self) -> String {
        let mut value = serde_json::to_value(self).expect("spec serialization");
        value
            .as_object_mut()
            .unwrap()
            .remove("resolved_spec_digest");
        payload_digest(&value)
    }
    pub fn seal(mut self) -> Self {
        self.resolved_spec_digest = self.computed_digest();
        self
    }
    pub fn verify_digest(&self) -> Result<(), ValidationError> {
        check(
            self.resolved_spec_digest == self.computed_digest(),
            "execution spec digest mismatch",
        )
    }
}
impl Validate for RegisterResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        self.identity.validate()?;
        check(
            self.protocol_version == EXECUTION_PROTOCOL_VERSION && capabilities(&self.capabilities),
            "invalid negotiated response",
        )
    }
}
impl Validate for PollResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.offers.len() <= MAX_BATCH && self.directives.len() <= MAX_BATCH,
            "poll response too large",
        )?;
        for o in &self.offers {
            o.validate()?;
        }
        for d in &self.directives {
            d.validate()?;
        }
        Ok(())
    }
}
impl Validate for AcceptResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        self.grant.validate()
    }
}
impl Validate for RenewResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        self.grant.validate()
    }
}
impl Validate for Directive {
    fn validate(&self) -> Result<(), ValidationError> {
        self.attempt.validate()?;
        check(!self.directive_id.is_nil(), "nil directive ID")
    }
}
impl Validate for Ack {
    fn validate(&self) -> Result<(), ValidationError> {
        check(!self.operation_id.is_nil(), "nil ack ID")
    }
}
impl Validate for ResultResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        check(!self.report_id.is_nil(), "nil report ID")
    }
}
impl Validate for ArtifactHandoffResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            !self.operation_id.is_nil()
                && self.central_receipt.as_ref().is_none_or(|s| text(s, 256)),
            "invalid artifact receipt",
        )
    }
}
impl Validate for ReconcileResponse {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.attempts.len() <= MAX_BATCH
                && self.next_cursor.as_ref().is_none_or(|s| text(s, 256)),
            "invalid reconcile response bounds",
        )?;
        for a in &self.attempts {
            a.attempt.validate()?;
            if let Some(g) = &a.grant {
                g.validate()?;
                check(g.attempt == a.attempt, "reconcile grant mismatch")?;
            }
            if let Some(r) = &a.result {
                r.validate()?;
            }
        }
        Ok(())
    }
}
impl Validate for ProtocolError {
    fn validate(&self) -> Result<(), ValidationError> {
        check(
            text(&self.message, MAX_TEXT) && self.operation_id.is_none_or(|x| !x.is_nil()),
            "invalid protocol error",
        )
    }
}

#[cfg(feature = "test-support")]
pub mod testing;

impl Validate for ReconcileAttempt {
    fn validate(&self) -> Result<(), ValidationError> {
        self.attempt.validate()?;
        check(
            !self.previous_boot_id.is_nil()
                && self.launch_id.is_none_or(|id| !id.is_nil())
                && self.report_id.is_none_or(|id| !id.is_nil()),
            "invalid reconciliation identity",
        )
    }
}
impl Validate for WireMessage {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::TransferManifest(x) => x.validate(),
            Self::TransferChunkRequest(x) => x.validate(),
            Self::TransferStatusRequest(x) => x.validate(),
            Self::TransferStatus(x) => x.validate(),
            Self::CentralRetentionReceipt(x) => x.validate(),
            Self::RegisterRequest(x) => x.validate(),
            Self::RegisterResponse(x) => x.validate(),
            Self::PollRequest(x) => x.validate(),
            Self::PollResponse(x) => x.validate(),
            Self::AcceptRequest(x) => x.validate(),
            Self::AcceptResponse(x) => x.validate(),
            Self::DeclineRequest(x) => x.validate(),
            Self::RenewRequest(x) => x.validate(),
            Self::RenewResponse(x) => x.validate(),
            Self::Observation(x) => x.validate(),
            Self::Ack(x) => x.validate(),
            Self::DirectiveAck(x) => x.validate(),
            Self::ArtifactHandoffRequest(x) => x.validate(),
            Self::ArtifactHandoffResponse(x) => x.validate(),
            Self::ResultReport(x) => x.validate(),
            Self::ResultResponse(x) => x.validate(),
            Self::ReconcileRequest(x) => x.validate(),
            Self::ReconcileResponse(x) => x.validate(),
            Self::ProtocolError(x) => x.validate(),
        }
    }
}
