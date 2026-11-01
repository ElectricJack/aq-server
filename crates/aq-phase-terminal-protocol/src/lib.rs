//! Structural correlation only. No DTO authenticates an owner, proves a kernel
//! observation, permits execution, renews a lease, or releases retained resources.
use aq_admission::{
    ArtifactRef, Digest, Text, granted_closure, preparation_v2 as phase, started, stopped,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
pub const MAX_BYTES: usize = 65_536;
pub const PROTOCOL: &str = "aq.phase-terminal.v1";
#[derive(Debug, thiserror::Error)]
#[error("invalid phase terminal contract: {0}")]
pub struct Invalid(pub &'static str);
pub type Result<T> = std::result::Result<T, Invalid>;
fn require(ok: bool, why: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(Invalid(why)) }
}
fn original<T>(r: aq_admission::Result<T>) -> Result<T> {
    r.map_err(|_| Invalid("original admission contract"))
}
fn canonical(v: &impl Serialize) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(v).map_err(|_| Invalid("encoding"))?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| Invalid("encoding"))
}
fn bound(v: &impl Serialize) -> Result<()> {
    require(
        canonical(v)?.len() <= MAX_BYTES,
        "inclusive envelope byte bound",
    )
}
fn hash(domain: &str, v: &impl Serialize) -> Result<Digest> {
    bound(v)?;
    Ok(Digest::of(&canonical(&serde_json::json!([domain, v]))?))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    StartedStoppedClosureV1,
    OriginalLeaseNoRenewalV1,
}
pub fn required_features() -> Vec<Feature> {
    vec![
        Feature::StartedStoppedClosureV1,
        Feature::OriginalLeaseNoRenewalV1,
    ]
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Negotiation {
    pub protocol: String,
    pub contract: ArtifactRef,
    #[schemars(length(min = 2, max = 2))]
    pub features: Vec<Feature>,
}
impl Negotiation {
    pub fn current() -> Result<Self> {
        Ok(Self {
            protocol: PROTOCOL.into(),
            contract: contract_artifact()?,
            features: required_features(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        require(
            self.protocol == PROTOCOL
                && self.contract == contract_artifact()?
                && self.features == required_features(),
            "unsupported exact negotiation",
        )
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OriginKind {
    PhaseExecutionV2,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub kind: OriginKind,
    pub execution_receipt_digest: Digest,
    pub accepted_receipt_digest: Digest,
}
impl Origin {
    pub fn for_context(c: &Context<'_>) -> Result<Self> {
        c.validate()?;
        Ok(Self {
            kind: OriginKind::PhaseExecutionV2,
            execution_receipt_digest: original(c.execution.digest())?,
            accepted_receipt_digest: original(c.accepted.digest())?,
        })
    }
    fn validate_for(&self, c: &Context<'_>) -> Result<()> {
        require(*self == Self::for_context(c)?, "phase origin mismatch")
    }
}
/// Borrow actual retained owner records. This view is deliberately not Serde.
/// Validation is historical structural correlation, never current authorization.
pub struct Context<'a> {
    pub execution: &'a phase::ExecutionReceipt,
    pub prepare: &'a phase::PrepareRequest,
    pub accepted: &'a phase::AcceptedReceipt,
    pub acceptance: &'a phase::AcceptRequest,
    pub proposal: &'a phase::Proposal,
    pub runtime: &'a ArtifactRef,
    pub runtime_qualification: &'a ArtifactRef,
    pub prepared: &'a granted_closure::JournalCheckpoint,
}
impl Context<'_> {
    pub fn validate(&self) -> Result<()> {
        original(self.execution.validate_for(
            self.prepare,
            self.accepted,
            self.acceptance,
            self.proposal,
        ))?;
        require(
            self.prepared.revision > 0
                && self.prepared.revision <= i64::MAX as u64
                && self.prepared.revision == self.prepare.original.journal_revision
                && self.prepared.digest == self.prepare.original.journal_digest,
            "exact retained server Prepared checkpoint",
        )?;
        // ArtifactRef's strings have validated constructors/strict deserializers.
        require(
            !self.runtime.id.as_str().is_empty()
                && !self.runtime.version.as_str().is_empty()
                && !self.runtime_qualification.id.as_str().is_empty()
                && !self.runtime_qualification.version.as_str().is_empty(),
            "runtime references",
        )
    }
}
macro_rules! request {
    ($name:ident,$body:ty,$domain:literal) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub schema_version: u32,
            pub negotiation: Negotiation,
            pub origin: Origin,
            pub original: $body,
        }
        impl $name {
            pub fn validate(&self) -> Result<()> {
                require(self.schema_version == 1, "envelope version")?;
                self.negotiation.validate()?;
                original(self.original.validate())?;
                bound(self)
            }
            pub fn digest(&self) -> Result<Digest> {
                self.validate()?;
                hash($domain, self)
            }
        }
    };
}
request!(
    StartedRequest,
    started::Request,
    "aq.phase-terminal.started.request.v1"
);
request!(
    StoppedRequest,
    stopped::Request,
    "aq.phase-terminal.stopped.request.v1"
);
request!(
    ClosureRequest,
    granted_closure::Request,
    "aq.phase-terminal.closure.request.v1"
);
impl StartedRequest {
    pub fn validate_for(&self, c: &Context<'_>) -> Result<()> {
        self.validate()?;
        self.origin.validate_for(c)?;
        let q = &self.original;
        let a = &c.accepted.lease;
        require(
            q.binding == c.execution.grant.binding
                && q.grant_id == c.execution.grant.grant_id
                && q.guardian_id == a.guardian_id
                && q.launch_id == a.launch_id
                && q.reservation_id == a.hold.reservation.reservation_id
                && q.runtime == *c.runtime
                && q.qualification == *c.runtime_qualification
                && q.journal_revision > c.prepared.revision,
            "phase Started owner/prepared mismatch",
        )
    }
}
impl StoppedRequest {
    pub fn validate_for(&self, c: &Context<'_>, previous: &StartedRequest) -> Result<()> {
        self.validate()?;
        self.origin.validate_for(c)?;
        previous.validate_for(c)?;
        require(
            self.origin == previous.origin && self.negotiation == previous.negotiation,
            "Stopped origin mismatch",
        )?;
        original(self.original.validate_after(&previous.original))
    }
}
impl ClosureRequest {
    /// Direct phase correlation. Absence of Started, current credentials, opaque
    /// suppression/host proof and newer acknowledged counters remain owner checks.
    pub fn validate_for(&self, c: &Context<'_>) -> Result<()> {
        self.validate()?;
        self.origin.validate_for(c)?;
        let q = &self.original;
        let a = &c.accepted.lease;
        require(
            q.binding == c.execution.grant.binding
                && q.grant_id == c.execution.grant.grant_id
                && q.guardian_id == a.guardian_id
                && q.launch_id == a.launch_id
                && q.reservation_id == a.hold.reservation.reservation_id
                && q.runtime == *c.runtime
                && q.qualification == *c.runtime_qualification
                && q.journal.revision > c.prepared.revision,
            "phase closure owner/prepared mismatch",
        )?;
        match &q.evidence {
            granted_closure::Evidence::Suppressed { record, .. } => require(
                record.previous.revision >= c.prepared.revision
                    && (record.previous.revision != c.prepared.revision
                        || record.previous.digest == c.prepared.digest),
                "suppression Prepared checkpoint",
            ),
            granted_closure::Evidence::HostStopped { driver_intent, .. } => require(
                driver_intent.journal.revision > c.prepared.revision,
                "driver intent after Prepared",
            ),
        }
    }
}
macro_rules! receipt {
    ($name:ident,$body:ty,$request:ty,$domain:literal) => {
        /// Historical acknowledgment only; never a delivery clock or permit.
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            pub schema_version: u32,
            pub negotiation: Negotiation,
            pub origin: Origin,
            /// Binds the complete wrapper as well as the original body's digest.
            pub request_digest: Digest,
            pub original: $body,
        }
        impl $name {
            pub fn validate_for(&self, q: &$request) -> Result<()> {
                q.validate()?;
                self.negotiation.validate()?;
                require(
                    self.schema_version == 1
                        && self.negotiation == q.negotiation
                        && self.origin == q.origin
                        && self.request_digest == q.digest()?,
                    "receipt envelope binding",
                )?;
                original(self.original.validate_for(&q.original))?;
                bound(self)
            }
            pub fn digest_for(&self, q: &$request) -> Result<Digest> {
                self.validate_for(q)?;
                hash($domain, self)
            }
        }
    };
}
receipt!(
    StartedReceipt,
    started::Receipt,
    StartedRequest,
    "aq.phase-terminal.started.receipt.v1"
);
receipt!(
    StoppedReceipt,
    stopped::Receipt,
    StoppedRequest,
    "aq.phase-terminal.stopped.receipt.v1"
);
receipt!(
    ClosureReceipt,
    granted_closure::Receipt,
    ClosureRequest,
    "aq.phase-terminal.closure.receipt.v1"
);
/// Exactly one retained historical acknowledgment, fetched under independent
/// current inspection authorization. No delivery or permission semantics.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "receipt",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HistoricalReadback {
    Started(StartedReceipt),
    Stopped(StoppedReceipt),
    Closure(ClosureReceipt),
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    require(bytes.len() <= MAX_BYTES, "wire byte bound")?;
    serde_json::from_slice(bytes).map_err(|_| Invalid("strict JSON document"))
}
macro_rules! decode_request {
    ($name:ident,$ty:ty) => {
        pub fn $name(bytes: &[u8]) -> Result<$ty> {
            let q: $ty = decode(bytes)?;
            q.validate()?;
            Ok(q)
        }
    };
}
decode_request!(decode_started_request, StartedRequest);
decode_request!(decode_stopped_request, StoppedRequest);
decode_request!(decode_closure_request, ClosureRequest);
macro_rules! decode_receipt {
    ($name:ident,$ty:ty,$req:ty) => {
        pub fn $name(bytes: &[u8], q: &$req) -> Result<$ty> {
            let r: $ty = decode(bytes)?;
            r.validate_for(q)?;
            Ok(r)
        }
    };
}
decode_receipt!(decode_started_receipt, StartedReceipt, StartedRequest);
decode_receipt!(decode_stopped_receipt, StoppedReceipt, StoppedRequest);
decode_receipt!(decode_closure_receipt, ClosureReceipt, ClosureRequest);
#[derive(JsonSchema)]
#[allow(dead_code)]
struct Documents {
    started_request: StartedRequest,
    stopped_request: StoppedRequest,
    closure_request: ClosureRequest,
    started_receipt: StartedReceipt,
    stopped_receipt: StoppedReceipt,
    closure_receipt: ClosureReceipt,
    historical_readback: HistoricalReadback,
    terminal_request: TerminalRequest,
    history_request: HistoryRequest,
    history_receipt: HistoryReceipt,
}
pub fn schema() -> serde_json::Value {
    let mut v = serde_json::to_value(schemars::schema_for!(Documents)).expect("static schema");
    v["$id"] = serde_json::json!("urn:aq:phase-terminal:1");
    v["$comment"] = serde_json::json!(
        "Every wire document <=65536 bytes inclusive. Relational validation mandatory. Structural correlation is not authorization, kernel proof, live authority, renewal or resource release."
    );
    for name in [
        "StartedRequest",
        "StoppedRequest",
        "ClosureRequest",
        "StartedReceipt",
        "StoppedReceipt",
        "ClosureReceipt",
        "HistoryRequest",
        "HistoryReceipt",
    ] {
        v["$defs"][name]["properties"]["schema_version"]["const"] = serde_json::json!(1);
    }
    v["$defs"]["HistoryReceipt"]["required"]
        .as_array_mut()
        .expect("static required")
        .push(serde_json::json!("identity"));
    v["$defs"]["Negotiation"]["properties"]["protocol"]["const"] = serde_json::json!(PROTOCOL);
    v["$defs"]["Negotiation"]["properties"]["features"]["const"] =
        serde_json::json!(required_features());
    v
}
pub fn contract_manifest() -> Result<serde_json::Value> {
    Ok(
        serde_json::json!({"schema_version":1,"publication":"aq-phase-terminal-protocol/0.1.0","admission":"0.1.9","protocol":PROTOCOL,"schema_digest":Digest::of(&canonical(&schema())?),"max_bytes":MAX_BYTES,"required_features":required_features()}),
    )
}
pub fn contract_artifact() -> Result<ArtifactRef> {
    static A: std::sync::OnceLock<ArtifactRef> = std::sync::OnceLock::new();
    Ok(A.get_or_init(|| ArtifactRef {
        id: Text::new("aq.phase-terminal.contract").expect("static id"),
        version: Text::new("1").expect("static version"),
        digest: hash(
            "aq.phase-terminal.contract.v1",
            &contract_manifest().expect("static manifest"),
        )
        .expect("static digest"),
    })
    .clone())
}
/// Lookup by keys known even when the first execution delivery was lost.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryRequest {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub operation_id: uuid::Uuid,
    pub binding: aq_admission::LaunchBinding,
    pub accepted_receipt_digest: Digest,
    pub prepare_operation_id: uuid::Uuid,
}
impl HistoryRequest {
    pub fn validate(&self) -> Result<()> {
        require(
            self.schema_version == 1
                && !self.operation_id.is_nil()
                && !self.prepare_operation_id.is_nil(),
            "history request identity",
        )?;
        self.negotiation.validate()?;
        // All binding identities are checked by the frozen contract validator.
        use aq_session_protocol::Validate;
        self.binding
            .validate()
            .map_err(|_| Invalid("history binding"))?;
        require(
            matches!(
                self.binding.session,
                aq_admission::SessionIdentity::Remote { .. }
            ),
            "remote history",
        )?;
        bound(self)
    }
    pub fn digest(&self) -> Result<Digest> {
        self.validate()?;
        hash("aq.phase-terminal.history.request.v1", self)
    }
}
/// Historical identity only, deliberately omitting delivery/remaining/local clock
/// samples. It cannot reconstruct an opaque worker execution observation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoricalGrantIdentity {
    pub origin: Origin,
    pub binding: aq_admission::LaunchBinding,
    pub grant_id: uuid::Uuid,
    pub guardian_id: uuid::Uuid,
    pub launch_id: uuid::Uuid,
    pub reservation_id: uuid::Uuid,
    pub runtime: ArtifactRef,
    pub qualification: ArtifactRef,
    pub prepared: granted_closure::JournalCheckpoint,
    pub execution_operation_id: uuid::Uuid,
    pub prepare_request_digest: Digest,
    pub versions: aq_admission::control::Versions,
    pub execution_deadline_ms: u64,
}
impl HistoricalGrantIdentity {
    pub fn for_context(c: &Context<'_>) -> Result<Self> {
        c.validate()?;
        Ok(Self {
            origin: Origin::for_context(c)?,
            binding: c.execution.grant.binding.clone(),
            grant_id: c.execution.grant.grant_id,
            guardian_id: c.accepted.lease.guardian_id,
            launch_id: c.accepted.lease.launch_id,
            reservation_id: c.accepted.lease.hold.reservation.reservation_id,
            runtime: c.runtime.clone(),
            qualification: c.runtime_qualification.clone(),
            prepared: c.prepared.clone(),
            execution_operation_id: c.execution.operation_id,
            prepare_request_digest: c.execution.request_digest.clone(),
            versions: c.execution.versions.clone(),
            execution_deadline_ms: c.execution.execution_deadline_ms,
        })
    }
    pub fn validate_for(&self, q: &HistoryRequest) -> Result<()> {
        q.validate()?;
        original(self.versions.validate())?;
        require(
            self.origin.accepted_receipt_digest == q.accepted_receipt_digest
                && self.binding == q.binding
                && self.execution_operation_id == q.prepare_operation_id
                && self.versions.grant_revision > 0
                && self.execution_deadline_ms > 0
                && self.execution_deadline_ms <= i64::MAX as u64
                && self.prepared.revision > 0
                && self.prepared.revision <= i64::MAX as u64,
            "historical identity binding",
        )?;
        for id in [
            self.grant_id,
            self.guardian_id,
            self.launch_id,
            self.reservation_id,
            self.execution_operation_id,
        ] {
            require(!id.is_nil(), "historical nil identity")?;
        }
        bound(self)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HistoryReceipt {
    pub schema_version: u32,
    pub negotiation: Negotiation,
    pub request_digest: Digest,
    /// Server projection sample only; never delivery/live authority.
    pub observed_at_ms: u64,
    /// Read snapshots may advance None to Some for the same lookup request.
    /// Null is never proof that a grant was not issued.
    #[serde(deserialize_with = "required_nullable")]
    pub identity: Option<HistoricalGrantIdentity>,
}
fn required_nullable<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}
impl HistoryReceipt {
    pub fn validate_for(&self, q: &HistoryRequest) -> Result<()> {
        q.validate()?;
        self.negotiation.validate()?;
        require(
            self.schema_version == 1
                && self.negotiation == q.negotiation
                && self.request_digest == q.digest()?
                && self.observed_at_ms > 0
                && self.observed_at_ms <= i64::MAX as u64,
            "history receipt binding",
        )?;
        if let Some(v) = &self.identity {
            v.validate_for(q)?;
        }
        bound(self)
    }
    pub fn digest_for(&self, q: &HistoryRequest) -> Result<Digest> {
        self.validate_for(q)?;
        hash("aq.phase-terminal.history.receipt.v1", self)
    }
}
decode_request!(decode_history_request, HistoryRequest);
decode_receipt!(decode_history_receipt, HistoryReceipt, HistoryRequest);
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalRequest {
    Started(StartedRequest),
    Stopped(StoppedRequest),
    Closure(ClosureRequest),
}
impl HistoricalReadback {
    pub fn validate_for(&self, q: &TerminalRequest) -> Result<()> {
        match (self, q) {
            (Self::Started(r), TerminalRequest::Started(q)) => r.validate_for(q)?,
            (Self::Stopped(r), TerminalRequest::Stopped(q)) => r.validate_for(q)?,
            (Self::Closure(r), TerminalRequest::Closure(q)) => r.validate_for(q)?,
            _ => return Err(Invalid("historical receipt kind mismatch")),
        }
        bound(self)
    }
    pub fn digest_for(&self, q: &TerminalRequest) -> Result<Digest> {
        self.validate_for(q)?;
        hash("aq.phase-terminal.readback.v1", self)
    }
}
pub fn decode_historical_readback(bytes: &[u8], q: &TerminalRequest) -> Result<HistoricalReadback> {
    let r: HistoricalReadback = decode(bytes)?;
    r.validate_for(q)?;
    Ok(r)
}
