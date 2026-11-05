//! Subject-neutral physical hosting contract, version 1. Artifact pins describe
//! caller-validated execution; they never confer authority by themselves.
use super::*;
use aq_session_protocol::{ContractError, HarnessSettings, ResourceLimits, TerminalPolicy};

/// A logical subject is independent of a physical session. New subject kinds
/// require an explicit contract revision; there are no synthetic task bindings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSubject {
    CentralConversation {
        actor_id: Uuid,
        conversation_id: Uuid,
        conversation_revision: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostStartup {
    pub descriptor: ArtifactRef,
    pub payload: ArtifactRef,
    pub bytes: u64,
}

/// Immutable input association, without a back-reference to CentralPlan or its
/// permit (CentralPlan itself pins this HostPlan). The authenticated owner joins
/// these fields to its durable DispatchAssociation before authorizing launch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostInput {
    pub dispatch_id: Uuid,
    pub host_operation_id: Uuid,
    pub batch: ArtifactRef,
}

/// The caller resolves and validates execution/composition/model/permission and
/// budget policy before admission. The pinned adapter must implement that exact
/// execution artifact or refuse. Startup is one immutable UTF-8 payload; this
/// contract does not qualify native readiness, reply, resume or live input.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPlan {
    pub host_plan_version: u32,
    pub subject: HostSubject,
    pub session: SessionIdentity,
    pub policy: ArtifactRef,
    pub execution: ArtifactRef,
    pub input: HostInput,
    pub harness: HarnessSettings,
    pub resources: ResourceLimits,
    pub terminal: TerminalPolicy,
    pub startup: HostStartup,
    pub machine_boot_id: Uuid,
    pub admitted_boot_ms: u64,
    pub deadline_boot_ms: u64,
}
impl Validate for HostPlan {
    fn validate(&self) -> std::result::Result<(), ContractError> {
        let require = |ok, why| {
            if ok {
                Ok(())
            } else {
                Err(ContractError::Invalid(why))
            }
        };
        self.session.validate()?;
        require(self.host_plan_version == 1, "host plan version")?;
        let HostSubject::CentralConversation {
            actor_id,
            conversation_id,
            conversation_revision,
        } = self.subject;
        require(
            !actor_id.is_nil() && !conversation_id.is_nil() && conversation_revision > 0,
            "central subject identity",
        )?;
        require(
            matches!(self.session, SessionIdentity::Central { .. }),
            "central subject session",
        )?;
        for pin in [
            &self.policy,
            &self.execution,
            &self.input.batch,
            &self.harness.adapter,
            &self.harness.environment,
            &self.startup.descriptor,
            &self.startup.payload,
        ] {
            pin.id.validate()?;
            pin.version.validate()?;
        }
        require(
            !self.input.dispatch_id.is_nil()
                && !self.input.host_operation_id.is_nil()
                && self.input.dispatch_id != self.input.host_operation_id,
            "host input operation identities",
        )?;
        self.harness.harness.validate()?;
        self.harness.cli_version.validate()?;
        require_capabilities(
            &self.harness.required_capabilities,
            &self.harness.required_capabilities,
        )?;
        for id in [
            "aq.harness.startup-context",
            "aq.harness.confined-worktree",
            "aq.harness.stop",
        ] {
            require(
                self.harness
                    .required_capabilities
                    .iter()
                    .any(|c| c.id.as_str() == id && c.version == 1),
                "mandatory host adapter capability",
            )?;
        }
        require(
            !self.machine_boot_id.is_nil()
                && self.deadline_boot_ms > self.admitted_boot_ms
                && self.deadline_boot_ms - self.admitted_boot_ms == self.resources.wall_time_ms,
            "host original deadline",
        )?;
        let r = &self.resources;
        require(
            r.memory_bytes > 0
                && r.cpu_millis > 0
                && r.process_limit > 0
                && r.stop_grace_ms <= r.wall_time_ms,
            "host resource bounds",
        )?;
        let t = &self.terminal;
        require(
            !t.input_enabled
                && t.max_input_bytes > 0
                && t.max_input_bytes <= 4096
                && t.max_pending_input_bytes >= t.max_input_bytes
                && t.max_pending_input_bytes <= 32768
                && t.max_frame_bytes > 0
                && t.max_frame_bytes <= 65536
                && t.replay_bytes >= t.max_frame_bytes
                && t.replay_bytes <= 1048576,
            "host terminal bounds; live input unqualified",
        )?;
        require(
            self.startup.bytes > 0 && self.startup.bytes <= 1_048_576,
            "host startup bounds",
        )?;
        require(
            serde_json::to_vec(self)
                .map_err(|e| ContractError::Json(e.to_string()))?
                .len()
                <= 65_536,
            "host plan bounds",
        )
    }
}
impl HostPlan {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() <= 65_536, "host plan bounds");
        let plan: Self = serde_json::from_slice(bytes)?;
        plan.validate()?;
        Ok(plan)
    }
    pub fn digest(&self) -> Result<Digest> {
        Ok(canonical_digest(self)?)
    }
    pub fn verify_startup(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(
            bytes.len() as u64 == self.startup.bytes
                && Digest::of(bytes) == self.startup.payload.digest
                && std::str::from_utf8(bytes).is_ok(),
            "host startup payload mismatch"
        );
        Ok(())
    }
    pub fn validate_permit_at(&self, permit: &ExecutionPermit, boot: Uuid, now: u64) -> Result<()> {
        self.validate()?;
        permit.validate_at(&self.session, &self.digest()?, now)?;
        ensure!(
            boot == self.machine_boot_id
                && now >= self.admitted_boot_ms
                && permit.issued_boot_ms >= self.admitted_boot_ms
                && permit.expires_boot_ms <= self.deadline_boot_ms,
            "host permit boot/original deadline mismatch"
        );
        Ok(())
    }
}

/// Trusted, version-pinned local adapter. It must resolve the exact execution
/// artifact, enforce its model/scopes/budgets, and refuse unsupported policies.
/// AdapterLaunch.manifest_digest means HostPlan::digest on this interface.
/// Startup bytes are staged at AQ_SESSION_DIRECTORY/context-0.bin before exec.
/// Credentials and private scratch remain the authenticated caller's concern;
/// task-bound private-input/observed factories are deliberately not reused.
pub trait HostAdapter {
    fn prepare(&self, plan: &HostPlan, workspace: &Path) -> Result<AdapterLaunch>;
    fn readiness(&self, observation: &HostObservation) -> Result<HarnessReadiness>;
}

// Untagged only for the private disk envelope: preserve byte-for-byte legacy
// manifest serialization (and configuration digest) on existing host launches.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum LaunchDocument {
    Task(Box<LaunchManifest>),
    Plan(Box<HostPlan>),
}
impl From<LaunchManifest> for LaunchDocument {
    fn from(value: LaunchManifest) -> Self {
        Self::Task(Box::new(value))
    }
}
impl Validate for LaunchDocument {
    fn validate(&self) -> std::result::Result<(), ContractError> {
        match self {
            Self::Task(v) => v.validate(),
            Self::Plan(v) => v.validate(),
        }
    }
}
impl LaunchDocument {
    pub(crate) fn task(&self) -> Result<&LaunchManifest> {
        match self {
            Self::Task(v) => Ok(v),
            Self::Plan(_) => bail!("task-only host extension"),
        }
    }
    pub(crate) fn session(&self) -> &SessionIdentity {
        match self {
            Self::Task(v) => &v.bindings.session,
            Self::Plan(v) => &v.session,
        }
    }
    pub(crate) fn resources(&self) -> &ResourceLimits {
        match self {
            Self::Task(v) => &v.resources,
            Self::Plan(v) => &v.resources,
        }
    }
    pub(crate) fn terminal(&self) -> &TerminalPolicy {
        match self {
            Self::Task(v) => &v.terminal,
            Self::Plan(v) => &v.terminal,
        }
    }
    pub(crate) fn harness(&self) -> &HarnessSettings {
        match self {
            Self::Task(v) => &v.harness,
            Self::Plan(v) => &v.harness,
        }
    }
    pub(crate) fn verify_startup(&self, directory: &Path) -> Result<()> {
        if let Self::Plan(p) = self {
            p.verify_startup(&read_bytes(&directory.join("context-0.bin"), 1_048_576)?)?;
        }
        Ok(())
    }
}

pub(crate) enum Adapter<'a> {
    Task(&'a dyn HarnessAdapter),
    Plan(&'a dyn HostAdapter),
}
impl Adapter<'_> {
    pub(crate) fn prepare(&self, doc: &LaunchDocument, workspace: &Path) -> Result<AdapterLaunch> {
        match (self, doc) {
            (Self::Task(a), LaunchDocument::Task(m)) => a.prepare(m, workspace),
            (Self::Plan(a), LaunchDocument::Plan(p)) => a.prepare(p, workspace),
            _ => bail!("host adapter contract mismatch"),
        }
    }
    pub(crate) fn private_input(
        &self,
        doc: &LaunchDocument,
        workspace: &Path,
    ) -> Result<Option<PrivateLaunchInput>> {
        match (self, doc) {
            (Self::Task(a), LaunchDocument::Task(m)) => a.private_input(m, workspace),
            (Self::Plan(_), LaunchDocument::Plan(_)) => Ok(None),
            _ => bail!("host adapter contract mismatch"),
        }
    }
}
impl SessionHost {
    #[expect(
        clippy::too_many_arguments,
        reason = "Matches the legacy launch boundary plus the separately pinned startup payload"
    )]
    pub fn launch_plan(
        root: &Path,
        launch_id: Uuid,
        plan: &HostPlan,
        startup: &[u8],
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl HostAdapter,
        authority: &mut impl LaunchAuthority,
    ) -> Result<SessionHandle> {
        Self::launch_inner(
            root,
            launch_id,
            StartupInput::Plan(plan, startup),
            permit,
            local,
            Adapter::Plan(adapter),
            authority,
        )
        .map(|(h, _)| h)
    }
}
