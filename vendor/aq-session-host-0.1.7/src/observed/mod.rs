//! Explicit observed-launch component candidate. Sealed bytes are not launch
//! permission, a physical baseline, harness readiness or a Snapshot producer.
mod exec;
mod executable;
mod named_environment;
mod named_image;
mod program;
pub use named_environment::NamedReadonlyEnvironment;
pub use named_image::{
    MAX_NAMED_EXEC_BYTES, MAX_NAMED_POOL_BYTES, MAX_NAMED_POOL_ENTRIES, NamedReadonlyExecImage,
    NamedReadonlyExecPool, NamedReadonlyFile,
};
use program::ObservedProgram;
pub(crate) mod gate;
pub use executable::{
    MAX_ELF_BYTES, MAX_ELF_POOL_BYTES, MAX_ELF_POOL_ENTRIES, MAX_NATIVE_EXEC_BYTES,
    MAX_NATIVE_EXEC_POOL_BYTES, MAX_NATIVE_EXEC_POOL_ENTRIES, NativeExecImage, NativeExecPool,
    QualifiedElf, SealedElfPool,
};

use crate::*;
use aq_session_protocol::TerminalIdentity;
use executable::ElfIdentity;
pub const OBSERVATION_PROFILE: &str = "linux-tmux34-sealed-small-elf-v1";
pub const NAMED_READONLY_EXEC_PROFILE: &str = "linux-tmux34-named-readonly-native-exec-stdin-v1";
pub const NAMED_READONLY_INTERACTIVE_PROFILE: &str =
    "linux-tmux34-named-readonly-interactive-pty-v1";
pub const NATIVE_EXEC_PROFILE: &str = "linux-tmux34-sealed-native-exec-stdin-v1";
/// Same bounded literal observer geometry, with an explicitly different native
/// executable/startup policy. This does not establish renderer or model evidence.
pub struct NamedReadonlyObservationProfile(ObservationProfile);
impl NamedReadonlyObservationProfile {
    pub fn new(renderer: ArtifactRef, columns: u16, rows: u16) -> Result<Self> {
        Ok(Self(ObservationProfile::small_elf(
            renderer, columns, rows,
        )?))
    }
}
pub struct NativeExecObservationProfile(ObservationProfile);
impl NativeExecObservationProfile {
    pub fn new(renderer: ArtifactRef, columns: u16, rows: u16) -> Result<Self> {
        Ok(Self(ObservationProfile::small_elf(
            renderer, columns, rows,
        )?))
    }
}
/// Closed candidate policy; the renderer reference is an installed qualification,
/// not an offer's self-assertion. No physical Snapshot is produced here.
pub struct ObservationProfile {
    renderer: ArtifactRef,
    columns: u16,
    rows: u16,
}
impl ObservationProfile {
    pub fn small_elf(renderer: ArtifactRef, columns: u16, rows: u16) -> Result<Self> {
        renderer.id.validate()?;
        renderer.version.validate()?;
        ensure!(
            columns > 0
                && columns <= 240
                && rows > 0
                && rows <= 120
                && u32::from(columns) * u32::from(rows) <= 12000,
            "observed pane geometry bounds"
        );
        Ok(Self {
            renderer,
            columns,
            rows,
        })
    }
}
pub struct ObservedAdapterQualification {
    pub executable: ArtifactRef,
    pub renderer: ArtifactRef,
}
pub trait ObservedHarnessAdapter: HarnessAdapter {
    fn observed_qualification(
        &self,
        manifest: &LaunchManifest,
    ) -> Result<ObservedAdapterQualification>;
}
pub struct ObservedStartup {
    pub(crate) rendered: RenderedStartup,
    pub(crate) program: ObservedProgram,
    pub(crate) spec: Spec,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Spec {
    pub version: u32,
    pub owner: ProcessEvidence,
    pub profile: String,
    pub baseline_id: Uuid,
    pub terminal: TerminalIdentity,
    pub renderer: ArtifactRef,
    pub columns: u16,
    pub rows: u16,
    pub hard_deadline_boot_ms: u64,
    pub program: ElfIdentity,
}
impl ObservedStartup {
    pub fn new(
        rendered: RenderedStartup,
        terminal: TerminalIdentity,
        program: QualifiedElf,
        profile: ObservationProfile,
        hard_deadline_boot_ms: u64,
    ) -> Result<Self> {
        rendered
            .binding
            .verify(&rendered.manifest, &rendered.bytes)?;
        terminal.validate()?;
        ensure!(
            terminal.session == rendered.manifest.bindings.session
                && hard_deadline_boot_ms > ownership::raw_boot_ms(),
            "observed exact session/deadline"
        );
        let spec = Spec {
            version: 1,
            owner: ownership::process_evidence(std::process::id())?,
            profile: OBSERVATION_PROFILE.into(),
            baseline_id: Uuid::new_v4(),
            terminal,
            renderer: profile.renderer,
            columns: profile.columns,
            rows: profile.rows,
            hard_deadline_boot_ms,
            program: program.identity().clone(),
        };
        Ok(Self {
            rendered,
            program: ObservedProgram::Sealed(program),
            spec,
        })
    }
}
impl ObservedStartup {
    pub fn native_exec(
        rendered: RenderedStartup,
        terminal: TerminalIdentity,
        program: NativeExecImage,
        profile: NativeExecObservationProfile,
        hard_deadline_boot_ms: u64,
    ) -> Result<Self> {
        ensure!(
            !rendered.manifest.terminal.input_enabled,
            "native stdin profile does not support terminal input"
        );
        let mut value = Self::new(
            rendered,
            terminal,
            program.0,
            profile.0,
            hard_deadline_boot_ms,
        )?;
        value.spec.profile = NATIVE_EXEC_PROFILE.into();
        Ok(value)
    }
}
impl ObservedStartup {
    pub fn named_readonly_exec(
        rendered: RenderedStartup,
        terminal: TerminalIdentity,
        program: NamedReadonlyExecImage,
        profile: NamedReadonlyObservationProfile,
        hard_deadline_boot_ms: u64,
    ) -> Result<Self> {
        program.revalidate()?;
        rendered
            .binding
            .verify(&rendered.manifest, &rendered.bytes)?;
        terminal.validate()?;
        ensure!(
            !rendered.manifest.terminal.input_enabled
                && terminal.session == rendered.manifest.bindings.session
                && hard_deadline_boot_ms > ownership::raw_boot_ms(),
            "named readonly startup session/deadline/input policy"
        );
        let program = ObservedProgram::Named(program);
        let spec = Spec {
            version: 1,
            owner: ownership::process_evidence(std::process::id())?,
            profile: NAMED_READONLY_EXEC_PROFILE.into(),
            baseline_id: Uuid::new_v4(),
            terminal,
            renderer: profile.0.renderer,
            columns: profile.0.columns,
            rows: profile.0.rows,
            hard_deadline_boot_ms,
            program: program.identity().clone(),
        };
        Ok(Self {
            rendered,
            program,
            spec,
        })
    }
}
impl ObservedStartup {
    /// Explicit interactive mode: same actual named readonly image and guardian,
    /// exact rendered prompt appended at exec, stdin remains the owned PTY.
    pub fn named_readonly_interactive(
        rendered: RenderedStartup,
        terminal: TerminalIdentity,
        program: NamedReadonlyExecImage,
        profile: NamedReadonlyObservationProfile,
        hard_deadline_boot_ms: u64,
    ) -> Result<Self> {
        program.revalidate()?;
        rendered
            .binding
            .verify(&rendered.manifest, &rendered.bytes)?;
        terminal.validate()?;
        exec::validate_interactive_prompt(&rendered.bytes)?;
        ensure!(
            rendered.manifest.terminal.input_enabled
                && terminal.session == rendered.manifest.bindings.session
                && hard_deadline_boot_ms > ownership::raw_boot_ms(),
            "interactive readonly startup session/deadline/input policy"
        );
        let program = ObservedProgram::Named(program);
        let spec = Spec {
            version: 1,
            owner: ownership::process_evidence(std::process::id())?,
            profile: NAMED_READONLY_INTERACTIVE_PROFILE.into(),
            baseline_id: Uuid::new_v4(),
            terminal,
            renderer: profile.0.renderer,
            columns: profile.0.columns,
            rows: profile.0.rows,
            hard_deadline_boot_ms,
            program: program.identity().clone(),
        };
        Ok(Self {
            rendered,
            program,
            spec,
        })
    }
}
impl Spec {
    pub(crate) fn native_exec(&self) -> bool {
        matches!(
            self.profile.as_str(),
            NATIVE_EXEC_PROFILE | NAMED_READONLY_EXEC_PROFILE | NAMED_READONLY_INTERACTIVE_PROFILE
        )
    }
    pub(crate) fn interactive(&self) -> bool {
        self.profile == NAMED_READONLY_INTERACTIVE_PROFILE
    }
    pub(crate) fn validate(&self, config: &SealedLaunch) -> Result<()> {
        self.terminal.validate()?;
        self.renderer.id.validate()?;
        self.renderer.version.validate()?;
        self.program.artifact.id.validate()?;
        self.program.artifact.version.validate()?;
        ensure!(
            self.owner.pid > 0
                && self.owner.start_ticks > 0
                && self.version == 1
                && match self.profile.as_str() {
                    OBSERVATION_PROFILE =>
                        self.program.profile.is_none() && self.program.bytes <= MAX_ELF_BYTES,
                    NATIVE_EXEC_PROFILE =>
                        self.program.profile == Some(executable::ImageProfile::NativeExecStdinV1)
                            && self.program.bytes <= MAX_NATIVE_EXEC_BYTES
                            && !config.manifest.terminal().input_enabled,
                    NAMED_READONLY_EXEC_PROFILE =>
                        matches!(&self.program.profile,
                        Some(executable::ImageProfile::NamedReadonlyExecStdinV1(e)) if e.role==named_image::NamedImageRole::Program)
                            && self.program.bytes <= MAX_NAMED_EXEC_BYTES
                            && !config.manifest.terminal().input_enabled,
                    NAMED_READONLY_INTERACTIVE_PROFILE =>
                        matches!(&self.program.profile,
                        Some(executable::ImageProfile::NamedReadonlyExecStdinV1(e)) if e.role==named_image::NamedImageRole::Program)
                            && self.program.bytes <= MAX_NAMED_EXEC_BYTES
                            && config.manifest.terminal().input_enabled
                            && config.command.arguments.last().is_some_and(|v| v == "--"),
                    _ => false,
                }
                && !self.baseline_id.is_nil()
                && self.terminal.session == config.manifest.task()?.bindings.session
                && config.rendered_startup.is_some()
                && config.tmux_version == "tmux 3.4"
                && self.columns > 0
                && self.columns <= 240
                && self.rows > 0
                && self.rows <= 120
                && u32::from(self.columns) * u32::from(self.rows) <= 12000
                && self.program.bytes >= 64
                && self.program.inode > 0
                && self.hard_deadline_boot_ms >= config.permit.expires_boot_ms
                && self.hard_deadline_boot_ms
                    <= config
                        .permit
                        .issued_boot_ms
                        .checked_add(config.manifest.resources().wall_time_ms)
                        .context("observed wall deadline overflow")?,
            "observed closed profile or authority bounds"
        );
        Ok(())
    }
}

/// A physically owned pane whose executable gate is still closed. This is not a
/// SessionHandle: ordinary terminal input/resize cannot escape before Running.
pub struct PendingObservedSession {
    handle: SessionHandle,
    client: gate::Client,
    spec: Spec,
}
impl SessionHost {
    pub fn prepare_observed(
        root: &Path,
        launch_id: Uuid,
        startup: &ObservedStartup,
        permit: ExecutionPermit,
        local: &LocalEnvironment,
        adapter: &impl ObservedHarnessAdapter,
        authority: &mut impl LaunchAuthority,
    ) -> Result<PendingObservedSession> {
        let qualification = adapter.observed_qualification(&startup.rendered.manifest)?;
        ensure!(
            qualification.executable == startup.spec.program.artifact
                && qualification.renderer == startup.spec.renderer,
            "observed installed qualification mismatch"
        );
        let (handle, client) = Self::launch_inner(
            root,
            launch_id,
            StartupInput::Observed(startup),
            permit,
            local,
            Adapter::Task(adapter),
            authority,
        )?;
        Ok(PendingObservedSession {
            handle,
            client: client.context("observed gate unavailable")?,
            spec: startup.spec.clone(),
        })
    }
}
impl PendingObservedSession {
    pub fn observe(&self) -> Result<HostObservation> {
        self.handle.observe()
    }
    pub fn wait_stopped(&mut self, timeout: Duration) -> Result<HostObservation> {
        self.handle.wait_stopped(timeout)
    }
    pub fn stop(&self) -> Result<()> {
        self.handle.stop()
    }
    pub(crate) fn closed(&self) -> Result<gate::ClosedReady> {
        let state = self.handle.observe()?;
        ensure!(
            state.phase == HostPhase::Starting,
            "observed gate is not closed"
        );
        self.client
            .closed(
                self.spec.baseline_id,
                self.spec
                    .hard_deadline_boot_ms
                    .min(state.permit_deadline_boot_ms),
            )?
            .context("closed helper not ready")
    }
}

/// Narrow physical blank premise, not a renderer Snapshot or resize barrier.
/// Construction is private to the guardian's exact closed-helper/read-lease gate.
/// ```compile_fail
/// let _: aq_session_host::observed::VerifiedBlankBaseline = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlankBaselineRecord {
    ready: gate::ClosedReady,
    terminal: TerminalIdentity,
    renderer: ArtifactRef,
    configuration_digest: Digest,
}
#[derive(Clone, Serialize)]
#[serde(transparent)]
pub struct VerifiedBlankBaseline(BlankBaselineRecord);
impl VerifiedBlankBaseline {
    pub fn terminal(&self) -> &TerminalIdentity {
        &self.0.terminal
    }
    pub fn renderer(&self) -> &ArtifactRef {
        &self.0.renderer
    }
    pub fn dimensions(&self) -> (u16, u16) {
        (self.0.ready.columns, self.0.ready.rows)
    }
    pub fn guardian(&self) -> &GuardianIdentity {
        &self.0.ready.guardian
    }
    pub fn baseline_id(&self) -> Uuid {
        self.0.ready.baseline_id
    }
    pub fn digest(&self) -> Result<Digest> {
        Ok(Digest::of(&serde_json::to_vec(self)?))
    }
}
pub trait ObservedLaunchAuthority: LaunchAuthority {
    /// The exact baseline acceptance and permission to release must be durable
    /// before returning. Failure sends no release and cannot be retried blindly.
    fn record_baseline(&mut self, baseline: &VerifiedBlankBaseline) -> Result<()>;
}
pub struct ReleasedObservedSession {
    handle: Option<SessionHandle>,
    _client: gate::Client,
}
impl PendingObservedSession {
    pub fn open_baseline_observer(
        &self,
    ) -> Result<(terminal_output::OutputObserver, VerifiedBlankBaseline)> {
        let ready = self.closed()?;
        let terminal = self.handle.bind_terminal_inner(&self.spec.terminal, true)?;
        let mut observer = terminal_output::OutputObserver::open(&terminal)?;
        let deadline = self
            .handle
            .observe()?
            .permit_deadline_boot_ms
            .min(self.spec.hard_deadline_boot_ms)
            .min(
                ownership::raw_boot_ms()
                    .checked_add(500)
                    .context("baseline wait overflow")?,
            );
        loop {
            for event in observer.poll()? {
                ensure!(
                    matches!(
                        event,
                        terminal_output::ControlEvent::Attached
                            | terminal_output::ControlEvent::LayoutChanged
                    ),
                    "pane emitted output or continuity failed before blank baseline"
                );
            }
            if observer.drained_after(ready.helper_ready_boot_ms)
                && let Some(baseline) = self.client.confirm(self.spec.baseline_id, deadline)?
            {
                ensure!(
                    baseline.0.ready == ready
                        && baseline.0.terminal == self.spec.terminal
                        && baseline.0.renderer == self.spec.renderer
                        && baseline.0.configuration_digest
                            == self.handle.identity.intent.configuration_digest,
                    "guardian baseline receipt does not match exact closed specification"
                );
                return Ok((observer, baseline));
            }
            ensure!(
                ownership::raw_boot_ms() < deadline,
                "blank baseline readiness deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    pub fn release_after_baseline(
        self,
        baseline: &VerifiedBlankBaseline,
        authority: &mut impl ObservedLaunchAuthority,
    ) -> Result<ReleasedObservedSession> {
        ensure!(
            baseline.0.ready.baseline_id == self.spec.baseline_id
                && baseline.0.terminal == self.spec.terminal
                && baseline.0.ready.guardian == self.handle.identity,
            "baseline release identity mismatch"
        );
        authority.record_baseline(baseline)?;
        let state = self.handle.observe()?;
        ensure!(
            state.phase == HostPhase::Starting,
            "baseline release requires closed helper"
        );
        self.client.release(
            baseline.digest()?,
            state
                .permit_deadline_boot_ms
                .min(self.spec.hard_deadline_boot_ms),
        )?;
        Ok(ReleasedObservedSession {
            handle: Some(self.handle),
            _client: self.client,
        })
    }
}
impl ReleasedObservedSession {
    pub fn observe(&self) -> Result<HostObservation> {
        self.handle
            .as_ref()
            .context("observed result already consumed")?
            .observe()
    }
    pub fn stop(&self) -> Result<()> {
        self.handle
            .as_ref()
            .context("observed result already consumed")?
            .stop()
    }
    /// Nonblocking: keep draining the observer between calls. Only actual live
    /// kernel FD-exec evidence produces Running; EOF/error/short exit cannot.
    pub fn poll_started(
        &mut self,
        authority: &mut impl LaunchAuthority,
    ) -> Result<Option<SessionHandle>> {
        let handle = self
            .handle
            .as_ref()
            .context("observed result already consumed")?;
        let observation = handle.observe()?;
        if observation.phase == HostPhase::Running {
            let config: SealedLaunch = read_json(&handle.directory.join("launch.json"))?;
            let spec = config
                .observed
                .as_ref()
                .context("observed Started specification missing")?;
            ensure!(
                ownership::raw_boot_ms() < observation.permit_deadline_boot_ms
                    && ownership::raw_boot_ms() < config.wall_deadline()?
                    && !handle.directory.join("stop.request").exists()
                    && supervisor::guardian_live(&handle.identity),
                "observed Started authority expired or lost"
            );
            exec::live_pane(
                observation
                    .pane
                    .as_ref()
                    .context("observed Started pane missing")?,
            )?;
            exec::observed_image(
                &observation
                    .pane
                    .as_ref()
                    .context("observed Started pane missing")?
                    .pane,
                &spec.program,
            )?;
            if let Err(error) = authority.record_started(&observation) {
                let _ = handle.stop();
                self.handle.take();
                return Err(error).context("Started persistence uncertain; no automatic retry");
            }
            return Ok(self.handle.take());
        }
        ensure!(
            matches!(observation.phase, HostPhase::Starting),
            "observed launch ended without Started; post-grant recovery required"
        );
        Ok(None)
    }
}
