//! Independent v1 contracts only; no process, transport or tmux qualification.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_INPUT_BYTES: usize = 4096;
pub const MAX_PENDING_INPUT_BYTES: usize = 32768;
pub const MAX_FRAME_BYTES: usize = 65536;
pub const MAX_REPLAY_BYTES: usize = 1048576;
/// Numeric JSON byte arrays have up to four wire bytes per byte.
pub const MAX_SNAPSHOT_BYTES: usize = 196608;
pub const MAX_BATCH_BYTES: usize = 1048576;
pub const PROFILED_LAUNCH_CAPABILITY: &str = "aq.profiled-launch";
pub const TERMINAL_CAPABILITY: &str = "aq.terminal";
/// Harness-native confinement: the harness's own sandbox confines the agent to the worktree.
pub const NATIVE_CONFINEMENT_CAPABILITY: &str = "aq.harness.confined-worktree";
/// Container confinement (0005 SBX19): the harness sandbox is disabled and the backend
/// sandbox is the boundary. Admission still requires explicit profile permission.
pub const CONTAINER_CONFINEMENT_CAPABILITY: &str = "aq.harness.container-confinement";
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid contract: {0}")]
    Invalid(&'static str),
    #[error("identity or digest conflict")]
    Conflict,
    #[error("stale authority or identity")]
    Stale,
    #[error("authority or command expired")]
    Expired,
    #[error("delivery is unknown; explicit resolution required")]
    DeliveryUnknown,
    #[error("unsupported required capability")]
    Unsupported,
    #[error("JSON: {0}")]
    Json(String),
}
pub trait Validate {
    fn validate(&self) -> Result<(), ContractError>;
}
fn ensure(ok: bool, why: &'static str) -> Result<(), ContractError> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::Invalid(why))
    }
}
/// Nonempty UTF-8 text, at most 512 bytes; no control characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Text(#[schemars(length(min = 1, max = 512))] String);
impl Text {
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let s = Self(value.into());
        s.validate()?;
        Ok(s)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Validate for Text {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            !self.0.is_empty() && self.0.len() <= 512 && !self.0.chars().any(char::is_control),
            "text bounds",
        )
    }
}
impl TryFrom<&str> for Text {
    type Error = ContractError;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}
impl<'de> Deserialize<'de> for Text {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct Digest(#[schemars(regex(pattern = "^[0-9a-f]{64}$"))] String);
impl Digest {
    pub fn of(bytes: &[u8]) -> Self {
        Self(format!("{:x}", Sha256::digest(bytes)))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            Ok(Self(s))
        } else {
            Err(serde::de::Error::custom("lowercase SHA256 required"))
        }
    }
}
/// Typed canonical JSON: UTF-8, compact, recursively sorted object keys, integer units.
pub fn canonical_bytes<T: Serialize + Validate>(value: &T) -> Result<Vec<u8>, ContractError> {
    value.validate()?;
    {
        let mut json =
            serde_json::to_value(value).map_err(|e| ContractError::Json(e.to_string()))?;
        json.sort_all_objects();
        serde_json::to_vec(&json).map_err(|e| ContractError::Json(e.to_string()))
    }
}
pub fn canonical_digest<T: Serialize + Validate>(value: &T) -> Result<Digest, ContractError> {
    Ok(Digest::of(&canonical_bytes(value)?))
}
pub fn decode<T: serde::de::DeserializeOwned + Validate>(bytes: &[u8]) -> Result<T, ContractError> {
    ensure(bytes.len() <= MAX_BATCH_BYTES, "document bounds")?;
    let v: T = serde_json::from_slice(bytes).map_err(|e| ContractError::Json(e.to_string()))?;
    v.validate()?;
    Ok(v)
}
macro_rules! record {($name:ident {$($field:ident:$ty:ty),* $(,)?})=>{#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize,JsonSchema)] #[serde(deny_unknown_fields)] pub struct $name {$(pub $field:$ty),*}}}
record!(ArtifactRef {
    id: Text,
    version: Text,
    digest: Digest
});
record!(Capability {
    id: Text,
    version: u32
});
impl Validate for Capability {
    fn validate(&self) -> Result<(), ContractError> {
        self.id.validate()?;
        ensure(self.version > 0, "capability version")
    }
}
/// Required capability versions must match exactly. Unknown advertised extensions confer no support.
pub fn require_capabilities(
    required: &[Capability],
    advertised: &[Capability],
) -> Result<(), ContractError> {
    ensure(
        required.len() <= 64 && advertised.len() <= 64,
        "capability count",
    )?;
    for list in [required, advertised] {
        let mut ids = BTreeSet::new();
        for c in list {
            c.validate()?;
            ensure(ids.insert(c.id.clone()), "duplicate capability")?;
        }
    }
    for c in required {
        if c.version != 1
            || !matches!(
                c.id.as_str(),
                "aq.profiled-launch"
                    | "aq.terminal"
                    | "aq.harness.startup-context"
                    | "aq.harness.confined-worktree"
                    | "aq.harness.container-confinement"
                    | "aq.harness.stop"
                    | "aq.harness.native-message"
                    | "aq.harness.resume"
                    | "aq.harness.structured-usage"
                    | "aq.harness.transcript"
                    | "aq.budget.task-cost"
            )
            || !advertised.contains(c)
        {
            return Err(ContractError::Unsupported);
        }
    }
    Ok(())
}
record!(SpendingBudget {
    currency: Text,
    basis: BudgetBasis,
    limit_microunits: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetBasis {
    TaskCumulativeEstimatedCost,
    TaskCumulativeEnforcedCost,
}
record!(ModelSettings {
    provider: Text,
    model: Text,
    reasoning: Text,
    max_output_tokens: u64,
    spending_budget: Option<SpendingBudget>
});
record!(HarnessSettings { harness:Text, cli_version:Text, adapter:ArtifactRef, environment:ArtifactRef, required_capabilities:Vec<Capability> });
record!(SecretRef {
    credential_profile: ArtifactRef,
    purpose: Text
});
record!(Scope {
    operation: Text,
    resource: Text
});
record!(PermissionPlan { requested:Vec<Scope>, effective:Vec<Scope>, required:Vec<Scope>, omissions:Vec<Text> });
record!(ContextRef {
    artifact: ArtifactRef,
    provenance: Text,
    delivery: ContextDelivery,
    bytes: u64,
    tokens: u64,
    required: bool
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextDelivery {
    Startup,
    OnDemand,
}
record!(ContextPlan { references:Vec<ContextRef>, startup_token_budget:u64, startup_byte_budget:u64, reserved_output_tokens:u64, context_window_tokens:u64 });
record!(LaunchBindings {
    session: SessionIdentity,
    project_id: Uuid,
    project_revision: u64,
    graph_id: Uuid,
    task_id: Uuid,
    task_revision: u64,
    attempt_id: Uuid,
    assignment_generation: u64,
    source: ArtifactRef,
    outcome: ArtifactRef,
    graph_access_profile: ArtifactRef
});
record!(ResourceLimits {
    memory_bytes: u64,
    cpu_millis: u64,
    process_limit: u32,
    wall_time_ms: u64,
    stop_grace_ms: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalHost {
    Tmux,
}
record!(TerminalPolicy {
    host: TerminalHost,
    input_enabled: bool,
    max_input_bytes: u32,
    max_pending_input_bytes: u32,
    max_frame_bytes: u32,
    replay_bytes: u32
});
record!(LaunchManifest { schema_version:u32, catalog:ArtifactRef, modules:Vec<ArtifactRef>, agent_profile:ArtifactRef, intelligence_profile:ArtifactRef, execution_profile:ArtifactRef, bindings:LaunchBindings, model:ModelSettings, harness:HarnessSettings, credentials:Vec<SecretRef>, permissions:PermissionPlan, context:ContextPlan, resources:ResourceLimits, terminal:TerminalPolicy, selection_explanation:ArtifactRef });
impl Validate for LaunchManifest {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(self.schema_version == 1, "schema version")?;
        self.bindings.session.validate()?;
        ensure(
            [
                self.bindings.project_id,
                self.bindings.graph_id,
                self.bindings.task_id,
                self.bindings.attempt_id,
            ]
            .iter()
            .all(|id| !id.is_nil()),
            "nil launch identity",
        )?;
        if let SessionIdentity::Remote {
            attempt_id,
            assignment_generation,
            ..
        } = self.bindings.session
        {
            ensure(
                attempt_id == self.bindings.attempt_id
                    && assignment_generation == self.bindings.assignment_generation,
                "remote launch identity",
            )?;
        }
        ensure(
            self.modules.len() <= 64
                && self.credentials.len() <= 16
                && self.context.references.len() <= 128,
            "manifest cardinality",
        )?;
        let declared = |id: &str| {
            self.harness
                .required_capabilities
                .iter()
                .any(|c| c.id.as_str() == id && c.version == 1)
        };
        ensure(
            declared(NATIVE_CONFINEMENT_CAPABILITY) != declared(CONTAINER_CONFINEMENT_CAPABILITY),
            "exactly one harness confinement mode",
        )?;
        for id in ["aq.harness.startup-context", "aq.harness.stop"] {
            ensure(
                self.harness.required_capabilities.contains(&Capability {
                    id: Text::new(id)?,
                    version: 1,
                }),
                "mandatory harness capability",
            )?;
        }
        let mut modules = BTreeSet::new();
        for m in &self.modules {
            ensure(modules.insert(m.id.clone()), "duplicate module identity")?;
        }
        require_capabilities(
            &self.harness.required_capabilities,
            &self.harness.required_capabilities,
        )?;
        if self
            .model
            .spending_budget
            .as_ref()
            .is_some_and(|b| b.basis == BudgetBasis::TaskCumulativeEnforcedCost)
        {
            ensure(
                self.harness.required_capabilities.contains(&Capability {
                    id: Text::new("aq.budget.task-cost")?,
                    version: 1,
                }),
                "enforced spending requires native capability",
            )?;
        }
        let p = &self.permissions;
        ensure(
            p.requested.len() <= 128
                && p.effective.len() <= 128
                && p.required.len() <= 128
                && p.omissions.len() <= 128,
            "permission cardinality",
        )?;
        for scope in &p.effective {
            ensure(
                p.requested.contains(scope),
                "effective permission exceeds request",
            )?;
        }
        for scope in &p.required {
            ensure(p.effective.contains(scope), "required permission denied")?;
        }
        let c = &self.context;
        let mut tokens = 0u64;
        let mut bytes = 0u64;
        for r in &c.references {
            ensure(
                r.bytes <= 1048576 && r.tokens <= 1048576,
                "context reference bounds",
            )?;
            if r.delivery == ContextDelivery::Startup {
                bytes = bytes
                    .checked_add(r.bytes)
                    .ok_or(ContractError::Invalid("context byte overflow"))?;
                tokens = tokens
                    .checked_add(r.tokens)
                    .ok_or(ContractError::Invalid("context overflow"))?;
            } else {
                ensure(!r.required, "required context deferred")?;
            }
        }
        ensure(
            bytes <= c.startup_byte_budget
                && c.startup_byte_budget <= 1048576
                && tokens <= c.startup_token_budget
                && c.startup_token_budget
                    .checked_add(c.reserved_output_tokens)
                    .is_some_and(|n| n <= c.context_window_tokens),
            "context budget",
        )?;
        ensure(
            self.model.max_output_tokens > 0
                && self.model.max_output_tokens <= c.reserved_output_tokens,
            "model output budget",
        )?;
        let r = &self.resources;
        ensure(
            r.memory_bytes > 0
                && r.cpu_millis > 0
                && r.process_limit > 0
                && r.wall_time_ms > 0
                && r.stop_grace_ms <= r.wall_time_ms,
            "resource limits",
        )?;
        let t = &self.terminal;
        ensure(
            t.max_input_bytes > 0
                && t.max_input_bytes <= 4096
                && t.max_pending_input_bytes >= t.max_input_bytes
                && t.max_pending_input_bytes <= 32768
                && t.max_frame_bytes > 0
                && t.max_frame_bytes <= 65536
                && t.replay_bytes >= t.max_frame_bytes
                && t.replay_bytes <= 1048576,
            "terminal limits",
        )?;
        ensure(
            self.bindings.assignment_generation > 0
                && self.bindings.project_revision > 0
                && self.bindings.task_revision > 0,
            "binding revisions",
        )
    }
}
// A new profiled execution extension, never an old unbound execution envelope.
record!(LaunchBinding {
    session: SessionIdentity,
    schema_version: u32,
    offer_id: Uuid,
    attempt_id: Uuid,
    assignment_generation: u64,
    execution_spec_digest: Digest,
    launch_manifest_digest: Digest,
    graph_access_profile_digest: Digest
});
impl Validate for LaunchBinding {
    fn validate(&self) -> Result<(), ContractError> {
        self.session.validate()?;
        if let SessionIdentity::Remote {
            attempt_id,
            assignment_generation,
            ..
        } = self.session
        {
            ensure(
                attempt_id == self.attempt_id
                    && assignment_generation == self.assignment_generation,
                "remote binding identity",
            )?;
        }
        ensure(
            self.schema_version == 1
                && self.assignment_generation > 0
                && !self.offer_id.is_nil()
                && !self.attempt_id.is_nil(),
            "launch binding",
        )
    }
}
record!(ProfiledOffer { binding:LaunchBinding, manifest:LaunchManifest, required_capabilities:Vec<Capability> });
impl Validate for ProfiledOffer {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        self.manifest.validate()?;
        ensure(
            self.manifest.bindings.session == self.binding.session
                && canonical_digest(&self.manifest)? == self.binding.launch_manifest_digest
                && self.manifest.bindings.attempt_id == self.binding.attempt_id
                && self.manifest.bindings.assignment_generation
                    == self.binding.assignment_generation
                && self.manifest.bindings.graph_access_profile.digest
                    == self.binding.graph_access_profile_digest,
            "manifest binding",
        )?;
        ensure(
            self.required_capabilities.contains(&Capability {
                id: Text::new(PROFILED_LAUNCH_CAPABILITY)?,
                version: 1,
            }),
            "profiled negotiation required",
        )?;
        ensure(
            self.required_capabilities.contains(&Capability {
                id: Text::new(TERMINAL_CAPABILITY)?,
                version: 1,
            }),
            "mandatory terminal capability",
        )?;
        require_capabilities(
            &self.manifest.harness.required_capabilities,
            &self.required_capabilities,
        )?;
        require_capabilities(&self.required_capabilities, &self.required_capabilities)
    }
}
record!(ProfiledAcceptance { binding:LaunchBinding, advertised_capabilities:Vec<Capability> });
record!(ProfiledGrant {
    binding: LaunchBinding,
    grant_id: Uuid,
    expires_at_ms: u64
});
record!(ProfiledJournalRecord {
    binding: LaunchBinding,
    grant_id: Uuid
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionIdentity {
    Remote {
        node_id: Uuid,
        boot_id: Uuid,
        attempt_id: Uuid,
        assignment_generation: u64,
        session_id: Uuid,
    },
    Central {
        authority_id: Uuid,
        session_id: Uuid,
        generation: u64,
    },
}
impl Validate for SessionIdentity {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            match self {
                Self::Remote {
                    node_id,
                    boot_id,
                    attempt_id,
                    assignment_generation,
                    session_id,
                } => {
                    *assignment_generation > 0
                        && [node_id, boot_id, attempt_id, session_id]
                            .iter()
                            .all(|id| !id.is_nil())
                }
                Self::Central {
                    authority_id,
                    session_id,
                    generation,
                } => *generation > 0 && !authority_id.is_nil() && !session_id.is_nil(),
            },
            "session generation",
        )
    }
}
record!(TerminalIdentity {
    session: SessionIdentity,
    terminal_id: Uuid,
    epoch: u64
});
impl Validate for TerminalIdentity {
    fn validate(&self) -> Result<(), ContractError> {
        self.session.validate()?;
        ensure(
            self.epoch > 0 && !self.terminal_id.is_nil(),
            "terminal epoch",
        )
    }
}
record!(ControllerLease {
    terminal: TerminalIdentity,
    principal_id: Text,
    lease_id: Uuid,
    generation: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
    execution_expires_at_ms: u64
});
impl Validate for ControllerLease {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.generation > 0
                && !self.lease_id.is_nil()
                && self.expires_at_ms > self.issued_at_ms
                && self.expires_at_ms - self.issued_at_ms <= 15000
                && self.expires_at_ms <= self.execution_expires_at_ms,
            "controller lease",
        )
    }
}
record!(InputCommand { operation_id:Uuid, lease:ControllerLease, issued_at_ms:u64, deadline_ms:u64, bytes:Vec<u8> });
impl Validate for InputCommand {
    fn validate(&self) -> Result<(), ContractError> {
        self.lease.validate()?;
        ensure(
            !self.operation_id.is_nil()
                && !self.bytes.is_empty()
                && self.bytes.len() <= MAX_INPUT_BYTES,
            "input bytes",
        )?;
        ensure(
            self.issued_at_ms >= self.lease.issued_at_ms
                && self.deadline_ms > self.issued_at_ms
                && self.deadline_ms - self.issued_at_ms <= 2000
                && self.deadline_ms <= self.lease.expires_at_ms,
            "input deadline",
        )
    }
}
record!(ResizeCommand {
    operation_id: Uuid,
    lease: ControllerLease,
    sequence: u64,
    issued_at_ms: u64,
    deadline_ms: u64,
    columns: u16,
    rows: u16
});
impl Validate for ResizeCommand {
    fn validate(&self) -> Result<(), ContractError> {
        self.lease.validate()?;
        ensure(
            !self.operation_id.is_nil()
                && self.sequence > 0
                && self.columns > 0
                && self.columns <= 1000
                && self.rows > 0
                && self.rows <= 1000,
            "resize bounds",
        )?;
        ensure(
            self.issued_at_ms >= self.lease.issued_at_ms
                && self.deadline_ms > self.issued_at_ms
                && self.deadline_ms - self.issued_at_ms <= 2000
                && self.deadline_ms <= self.lease.expires_at_ms,
            "resize deadline",
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputDisposition {
    Queued,
    Received,
    Injecting,
    Applied,
    DeliveryUnknown,
    Expired,
    Rejected,
}
record!(InputReceipt {
    operation_id: Uuid,
    command_digest: Digest,
    terminal: TerminalIdentity,
    disposition: InputDisposition
});
record!(OutputFrame { terminal:TerminalIdentity, sequence:u64, bytes:Vec<u8>, byte_length:u32, digest:Digest });
impl Validate for OutputFrame {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.sequence > 0
                && !self.bytes.is_empty()
                && self.bytes.len() <= MAX_FRAME_BYTES
                && self.byte_length as usize == self.bytes.len()
                && Digest::of(&self.bytes) == self.digest,
            "output frame",
        )
    }
}
// Opaque renderer-specific state. This contract does not qualify any renderer.
record!(Snapshot { snapshot_id:Uuid,capture_request_id:Uuid,terminal:TerminalIdentity, through_sequence:u64, columns:u16, rows:u16, renderer:ArtifactRef, state:Vec<u8>, digest:Digest });
impl Validate for Snapshot {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            !self.snapshot_id.is_nil()
                && !self.capture_request_id.is_nil()
                && self.columns > 0
                && self.columns <= 1000
                && self.rows > 0
                && self.rows <= 1000
                && self.state.len() <= MAX_SNAPSHOT_BYTES
                && Digest::of(&self.state) == self.digest,
            "snapshot bounds",
        )
    }
}
record!(OutputGap {
    terminal: TerminalIdentity,
    first_missing: u64,
    last_missing: u64
});
impl Validate for OutputGap {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(
            self.first_missing > 0 && self.last_missing >= self.first_missing,
            "gap range",
        )
    }
}
record!(WatchSubscription {
    subscription_id: Uuid,
    terminal: TerminalIdentity,
    principal_id: Text,
    expires_at_ms: u64
});
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalCommand {
    Input(InputCommand),
    Resize(ResizeCommand),
    Controller(ControllerLease),
    Watch(WatchSubscription),
}
impl Validate for TerminalCommand {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Input(v) => v.validate(),
            Self::Resize(v) => v.validate(),
            Self::Controller(v) => v.validate(),
            Self::Watch(v) => {
                v.terminal.validate()?;
                ensure(
                    !v.subscription_id.is_nil() && v.expires_at_ms > 0,
                    "subscription identity/expiry",
                )
            }
        }
    }
}
record!(NodeTerminalPoll {
    schema_version: u32,
    node_id: Uuid,
    boot_id: Uuid,
    cursor: u64,
    hold_ms: u32,
    active_subscriptions: u32
});
impl Validate for NodeTerminalPoll {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            self.schema_version == 1
                && !self.node_id.is_nil()
                && !self.boot_id.is_nil()
                && self.hold_ms <= 20000
                && self.active_subscriptions <= 32
                && (self.active_subscriptions > 0 || self.hold_ms == 0),
            "poll bounds",
        )
    }
}
record!(NodeCommandBatch { schema_version:u32, node_id:Uuid, boot_id:Uuid, batch_id:Uuid, cursor:u64, commands:Vec<TerminalCommand> });
fn remote_matches(t: &TerminalIdentity, node: Uuid, boot: Uuid) -> bool {
    matches!(t.session,SessionIdentity::Remote{node_id,boot_id,..} if node_id==node&&boot_id==boot)
}
impl Validate for NodeCommandBatch {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            self.schema_version == 1
                && !self.node_id.is_nil()
                && !self.boot_id.is_nil()
                && !self.batch_id.is_nil()
                && self.commands.len() <= 64,
            "command batch bounds",
        )?;
        for c in &self.commands {
            c.validate()?;
            let t = match c {
                TerminalCommand::Input(v) => &v.lease.terminal,
                TerminalCommand::Resize(v) => &v.lease.terminal,
                TerminalCommand::Controller(v) => &v.terminal,
                TerminalCommand::Watch(v) => &v.terminal,
            };
            ensure(
                remote_matches(t, self.node_id, self.boot_id),
                "batch node binding",
            )?;
        }
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= MAX_BATCH_BYTES,
            "batch bytes",
        )
    }
}
record!(NodeOutputBatch { schema_version:u32, node_id:Uuid, boot_id:Uuid, batch_id:Uuid, cursor:u64, frames:Vec<OutputFrame>, snapshots:Vec<Snapshot>, gaps:Vec<OutputGap>, receipts:Vec<InputReceipt> });
impl Validate for NodeOutputBatch {
    fn validate(&self) -> Result<(), ContractError> {
        ensure(
            self.schema_version == 1
                && !self.node_id.is_nil()
                && !self.boot_id.is_nil()
                && !self.batch_id.is_nil()
                && self.frames.len() <= 64
                && self.snapshots.len() <= 32
                && self.gaps.len() <= 64
                && self.receipts.len() <= 64,
            "output batch count",
        )?;
        for f in &self.frames {
            f.validate()?;
            ensure(
                remote_matches(&f.terminal, self.node_id, self.boot_id),
                "frame node",
            )?;
        }
        for s in &self.snapshots {
            s.validate()?;
            ensure(
                remote_matches(&s.terminal, self.node_id, self.boot_id),
                "snapshot node",
            )?;
        }
        for g in &self.gaps {
            g.validate()?;
            ensure(
                remote_matches(&g.terminal, self.node_id, self.boot_id),
                "gap node",
            )?;
        }
        for r in &self.receipts {
            r.validate()?;
            ensure(
                remote_matches(&r.terminal, self.node_id, self.boot_id),
                "receipt node",
            )?;
        }
        ensure(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= MAX_BATCH_BYTES,
            "output batch bytes",
        )
    }
}
/// Deterministic conformance oracle, not a runtime journal or authority service.
pub struct InputConformance {
    pub terminal: TerminalIdentity,
    pub controller_generation: u64,
    records: BTreeMap<Uuid, (Digest, InputDisposition, usize)>,
    pending: usize,
    uncertain: bool,
    resolution_capture: Option<(Uuid, u64)>,
    used_capture_ids: BTreeSet<Uuid>,
    controller_lease: Option<(Uuid, Text)>,
}
impl InputConformance {
    pub fn new(terminal: TerminalIdentity, controller_generation: u64) -> Self {
        Self {
            terminal,
            controller_generation,
            records: BTreeMap::new(),
            pending: 0,
            uncertain: false,
            resolution_capture: None,
            used_capture_ids: BTreeSet::new(),
            controller_lease: None,
        }
    }
    pub fn receive(
        &mut self,
        c: &InputCommand,
        now_ms: u64,
    ) -> Result<InputDisposition, ContractError> {
        c.validate()?;
        let digest = canonical_digest(c)?;
        if let Some((old, status, _)) = self.records.get(&c.operation_id) {
            return if *old != digest {
                Err(ContractError::Conflict)
            } else {
                Ok(*status)
            };
        }
        if c.lease.terminal != self.terminal || c.lease.generation != self.controller_generation {
            return Err(ContractError::Stale);
        }
        if now_ms < c.issued_at_ms || now_ms >= c.deadline_ms || now_ms >= c.lease.expires_at_ms {
            return Err(ContractError::Expired);
        }
        if self.uncertain {
            return Err(ContractError::DeliveryUnknown);
        }
        if let Some((id, principal)) = &self.controller_lease
            && (*id != c.lease.lease_id || *principal != c.lease.principal_id)
        {
            return Err(ContractError::Stale);
        } else {
            self.controller_lease = Some((c.lease.lease_id, c.lease.principal_id.clone()));
        }
        ensure(self.records.len() < 1024, "receipt history bounds")?;
        ensure(
            self.pending + c.bytes.len() <= MAX_PENDING_INPUT_BYTES,
            "input backlog",
        )?;
        self.pending += c.bytes.len();
        self.records.insert(
            c.operation_id,
            (digest, InputDisposition::Received, c.bytes.len()),
        );
        Ok(InputDisposition::Received)
    }
    pub fn begin_injection(&mut self, c: &InputCommand, now_ms: u64) -> Result<(), ContractError> {
        if c.lease.terminal != self.terminal || c.lease.generation != self.controller_generation {
            return Err(ContractError::Stale);
        }
        if now_ms < c.issued_at_ms || now_ms >= c.deadline_ms || now_ms >= c.lease.expires_at_ms {
            return Err(ContractError::Expired);
        }
        if self.controller_lease.as_ref() != Some(&(c.lease.lease_id, c.lease.principal_id.clone()))
        {
            return Err(ContractError::Stale);
        }
        let r = self
            .records
            .get_mut(&c.operation_id)
            .ok_or(ContractError::Stale)?;
        if r.0 != canonical_digest(c)? {
            return Err(ContractError::Conflict);
        }
        ensure(
            r.1 == InputDisposition::Received && !self.uncertain,
            "injection transition",
        )?;
        r.1 = InputDisposition::Injecting;
        Ok(())
    }
    pub fn finish(&mut self, id: Uuid, confirmed: bool) -> Result<(), ContractError> {
        let r = self.records.get_mut(&id).ok_or(ContractError::Stale)?;
        ensure(r.1 == InputDisposition::Injecting, "finish transition")?;
        r.1 = if confirmed {
            InputDisposition::Applied
        } else {
            InputDisposition::DeliveryUnknown
        };
        self.pending -= r.2;
        self.uncertain |= !confirmed;
        Ok(())
    }
    /// Reserve a unique capture token after uncertainty. Quiet output is not freshness proof.
    pub fn request_resolution_capture(
        &mut self,
        request_id: Uuid,
        controller_generation: u64,
    ) -> Result<(), ContractError> {
        ensure(
            self.uncertain && !request_id.is_nil() && self.used_capture_ids.len() < 1024,
            "resolution request",
        )?;
        if controller_generation != self.controller_generation {
            return Err(ContractError::Stale);
        }
        ensure(
            self.used_capture_ids.insert(request_id),
            "reused capture request",
        )?;
        self.resolution_capture = Some((request_id, controller_generation));
        Ok(())
    }
    pub fn resolve_unknown(&mut self, fresh_snapshot: &Snapshot) -> Result<(), ContractError> {
        ensure(
            self.uncertain
                && self.resolution_capture
                    == Some((
                        fresh_snapshot.capture_request_id,
                        self.controller_generation,
                    )),
            "fresh capture required",
        )?;
        fresh_snapshot.validate()?;
        ensure(
            fresh_snapshot.terminal == self.terminal,
            "resolution identity",
        )?;
        self.uncertain = false;
        self.resolution_capture = None;
        Ok(())
    }
}
/// Snapshot covers all frames through its watermark; only strictly later contiguous frames apply.
pub struct ReplayConformance {
    terminal: TerminalIdentity,
    next: u64,
    seen: BTreeMap<u64, Digest>,
    retained: usize,
}
impl ReplayConformance {
    pub fn from_snapshot(s: &Snapshot) -> Result<Self, ContractError> {
        s.validate()?;
        Ok(Self {
            terminal: s.terminal.clone(),
            next: s
                .through_sequence
                .checked_add(1)
                .ok_or(ContractError::Invalid("sequence overflow"))?,
            seen: BTreeMap::new(),
            retained: s.state.len(),
        })
    }
    pub fn apply(&mut self, f: &OutputFrame) -> Result<bool, ContractError> {
        f.validate()?;
        if f.terminal != self.terminal {
            return Err(ContractError::Stale);
        }
        if let Some(d) = self.seen.get(&f.sequence) {
            return if *d == f.digest {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        if f.sequence < self.next {
            return Ok(false);
        }
        ensure(f.sequence == self.next, "explicit gap/snapshot required")?;
        ensure(
            self.retained + f.bytes.len() <= MAX_REPLAY_BYTES,
            "replay backlog",
        )?;
        self.next = self
            .next
            .checked_add(1)
            .ok_or(ContractError::Invalid("sequence overflow"))?;
        self.retained += f.bytes.len();
        self.seen.insert(f.sequence, f.digest.clone());
        Ok(true)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "body",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContractDocument {
    Launch(Box<ProfiledOffer>),
    Acceptance(ProfiledAcceptance),
    Grant(ProfiledGrant),
    Journal(ProfiledJournalRecord),
    Poll(NodeTerminalPoll),
    Commands(NodeCommandBatch),
    Output(NodeOutputBatch),
}
impl Validate for ContractDocument {
    fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Launch(v) => v.validate(),
            Self::Acceptance(v) => v.validate(),
            Self::Grant(v) => v.validate(),
            Self::Journal(v) => v.validate(),
            Self::Poll(v) => v.validate(),
            Self::Commands(v) => v.validate(),
            Self::Output(v) => v.validate(),
        }
    }
}
impl ProfiledAcceptance {
    pub fn validate_against(&self, offer: &ProfiledOffer) -> Result<(), ContractError> {
        offer.validate()?;
        self.binding.validate()?;
        if self.binding != offer.binding {
            return Err(ContractError::Conflict);
        }
        require_capabilities(&offer.required_capabilities, &self.advertised_capabilities)?;
        require_capabilities(
            &offer.manifest.harness.required_capabilities,
            &self.advertised_capabilities,
        )
    }
}
impl ProfiledGrant {
    pub fn validate_against(
        &self,
        accepted: &ProfiledAcceptance,
        now_ms: u64,
    ) -> Result<(), ContractError> {
        self.binding.validate()?;
        if self.binding != accepted.binding {
            return Err(ContractError::Conflict);
        }
        ensure(!self.grant_id.is_nil(), "nil grant identity")?;
        if self.expires_at_ms <= now_ms {
            return Err(ContractError::Expired);
        }
        Ok(())
    }
}
impl ProfiledJournalRecord {
    pub fn validate_against(
        &self,
        grant: &ProfiledGrant,
        accepted: &ProfiledAcceptance,
    ) -> Result<(), ContractError> {
        self.binding.validate()?;
        if self.binding != grant.binding
            || self.binding != accepted.binding
            || self.grant_id != grant.grant_id
            || self.grant_id.is_nil()
        {
            return Err(ContractError::Conflict);
        }
        Ok(())
    }
}
impl Validate for ProfiledAcceptance {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        require_capabilities(&[], &self.advertised_capabilities)
    }
}
impl Validate for ProfiledGrant {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        ensure(
            !self.grant_id.is_nil() && self.expires_at_ms > 0,
            "grant identity/expiry",
        )
    }
}
impl Validate for ProfiledJournalRecord {
    fn validate(&self) -> Result<(), ContractError> {
        self.binding.validate()?;
        ensure(!self.grant_id.is_nil(), "journal grant identity")
    }
}
impl Validate for InputReceipt {
    fn validate(&self) -> Result<(), ContractError> {
        self.terminal.validate()?;
        ensure(!self.operation_id.is_nil(), "receipt identity")
    }
}
/// Published structural schema. Cross-field, digest, byte-size and authority rules
/// are enforced by Validate and are normative alongside this schema.
pub fn contract_schema() -> serde_json::Value {
    let mut s =
        serde_json::to_value(schemars::schema_for!(ContractDocument)).expect("schema is JSON");
    let defs = s["$defs"].as_object_mut().expect("definitions");
    for (name, field, max) in [
        ("LaunchManifest", "modules", 64),
        ("LaunchManifest", "credentials", 16),
        ("HarnessSettings", "required_capabilities", 64),
        ("ProfiledOffer", "required_capabilities", 64),
        ("ProfiledAcceptance", "advertised_capabilities", 64),
        ("PermissionPlan", "requested", 128),
        ("PermissionPlan", "effective", 128),
        ("PermissionPlan", "required", 128),
        ("PermissionPlan", "omissions", 128),
        ("ContextPlan", "references", 128),
        ("InputCommand", "bytes", 4096),
        ("OutputFrame", "bytes", 65536),
        ("Snapshot", "state", 196608),
        ("NodeCommandBatch", "commands", 64),
        ("NodeOutputBatch", "frames", 64),
        ("NodeOutputBatch", "snapshots", 32),
        ("NodeOutputBatch", "gaps", 64),
        ("NodeOutputBatch", "receipts", 64),
    ] {
        defs.get_mut(name).expect("known type")["properties"][field]["maxItems"] = max.into();
    }
    for name in [
        "LaunchManifest",
        "LaunchBinding",
        "NodeTerminalPoll",
        "NodeCommandBatch",
        "NodeOutputBatch",
    ] {
        defs.get_mut(name).expect("known type")["properties"]["schema_version"]["const"] = 1.into();
    }
    for (name, field, max) in [
        ("TerminalPolicy", "max_input_bytes", 4096),
        ("TerminalPolicy", "max_pending_input_bytes", 32768),
        ("TerminalPolicy", "max_frame_bytes", 65536),
        ("TerminalPolicy", "replay_bytes", 1048576),
        ("ContextPlan", "startup_byte_budget", 1048576),
        ("ContextRef", "bytes", 1048576),
        ("ContextRef", "tokens", 1048576),
        ("NodeTerminalPoll", "hold_ms", 20000),
        ("NodeTerminalPoll", "active_subscriptions", 32),
        ("ResizeCommand", "columns", 1000),
        ("ResizeCommand", "rows", 1000),
        ("Snapshot", "columns", 1000),
        ("Snapshot", "rows", 1000),
    ] {
        defs.get_mut(name).expect("known type")["properties"][field]["maximum"] = max.into();
    }
    s
}
/// Resize retries are independent of literal-input uncertainty and ordered by sequence.
pub struct ResizeConformance {
    pub terminal: TerminalIdentity,
    pub controller_generation: u64,
    last_sequence: u64,
    history: BTreeMap<Uuid, Digest>,
    controller_lease: Option<(Uuid, Text)>,
}
impl ResizeConformance {
    pub fn new(terminal: TerminalIdentity, controller_generation: u64) -> Self {
        Self {
            terminal,
            controller_generation,
            last_sequence: 0,
            history: BTreeMap::new(),
            controller_lease: None,
        }
    }
    pub fn apply(&mut self, c: &ResizeCommand, now_ms: u64) -> Result<bool, ContractError> {
        c.validate()?;
        if c.lease.terminal != self.terminal || c.lease.generation != self.controller_generation {
            return Err(ContractError::Stale);
        }
        if now_ms < c.issued_at_ms || now_ms >= c.deadline_ms {
            return Err(ContractError::Expired);
        }
        if let Some((id, principal)) = &self.controller_lease
            && (*id != c.lease.lease_id || *principal != c.lease.principal_id)
        {
            return Err(ContractError::Stale);
        }
        let d = canonical_digest(c)?;
        if let Some(old) = self.history.get(&c.operation_id) {
            return if *old == d {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        if c.sequence <= self.last_sequence {
            return Err(ContractError::Stale);
        }
        ensure(self.history.len() < 1024, "resize history bounds")?;
        self.controller_lease = Some((c.lease.lease_id, c.lease.principal_id.clone()));
        self.last_sequence = c.sequence;
        self.history.insert(c.operation_id, d);
        Ok(true)
    }
}
/// Bounded batch-id conflict oracle; production transports own durable retention.
#[derive(Default)]
pub struct BatchConformance {
    seen: BTreeMap<Uuid, Digest>,
}
impl BatchConformance {
    fn observe<T: Serialize + Validate>(
        &mut self,
        id: Uuid,
        batch: &T,
    ) -> Result<bool, ContractError> {
        let digest = canonical_digest(batch)?;
        if let Some(old) = self.seen.get(&id) {
            return if *old == digest {
                Ok(false)
            } else {
                Err(ContractError::Conflict)
            };
        }
        ensure(self.seen.len() < 1024, "batch history bounds")?;
        self.seen.insert(id, digest);
        Ok(true)
    }
    pub fn output(&mut self, batch: &NodeOutputBatch) -> Result<bool, ContractError> {
        self.observe(batch.batch_id, batch)
    }
    pub fn commands(&mut self, batch: &NodeCommandBatch) -> Result<bool, ContractError> {
        self.observe(batch.batch_id, batch)
    }
}
