//! Taskless contract and physical Linux/tmux qualification. No provider or server.
use anyhow::{Result, ensure};
use aq_session_host::*;
use aq_session_protocol::{ArtifactRef, Digest, LaunchManifest, SessionIdentity, Text, Validate};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use uuid::Uuid;

fn pin(name: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(name).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(name.as_bytes()),
    }
}
fn fixture() -> HostPlan {
    let legacy: LaunchManifest =
        serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    let now = ownership::raw_boot_ms();
    let mut terminal = legacy.terminal;
    terminal.input_enabled = false;
    let mut resources = legacy.resources;
    resources.wall_time_ms = 15_000;
    resources.stop_grace_ms = 20;
    HostPlan {
        host_plan_version: 1,
        subject: HostSubject::CentralConversation {
            actor_id: Uuid::new_v4(),
            conversation_id: Uuid::new_v4(),
            conversation_revision: 1,
        },
        session: SessionIdentity::Central {
            authority_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        policy: pin("actor-policy"),
        execution: pin("execution"),
        input: HostInput {
            dispatch_id: Uuid::new_v4(),
            host_operation_id: Uuid::new_v4(),
            batch: pin("batch"),
        },
        harness: legacy.harness,
        resources,
        terminal,
        startup: HostStartup {
            descriptor: pin("startup-descriptor"),
            payload: ArtifactRef {
                digest: Digest::of(b"supervisor startup\n"),
                ..pin("startup-payload")
            },
            bytes: 19,
        },
        machine_boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
        admitted_boot_ms: now,
        deadline_boot_ms: now + 15_000,
    }
}
fn permit(p: &HostPlan, duration: u64) -> ExecutionPermit {
    let now = ownership::raw_boot_ms();
    ExecutionPermit {
        session: p.session.clone(),
        manifest_digest: p.digest().unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + duration,
    }
}
#[test]
fn supervisor_session_contract_and_v1_validation() {
    let p = fixture();
    p.verify_startup(b"supervisor startup\n").unwrap();
    let encoded = serde_json::to_vec(&p).unwrap();
    assert_eq!(
        HostPlan::decode(&encoded).unwrap().digest().unwrap(),
        p.digest().unwrap()
    );
    let text = String::from_utf8(encoded).unwrap();
    for forbidden in [
        "task_id",
        "attempt_id",
        "project_id",
        "graph_id",
        "assignment_generation",
    ] {
        assert!(!text.contains(forbidden));
    }
    for (field, value) in [
        ("host_plan_version", serde_json::json!(2)),
        ("extra", serde_json::json!(1)),
        ("task_id", serde_json::json!(Uuid::new_v4())),
    ] {
        let mut v = serde_json::to_value(&p).unwrap();
        v[field] = value;
        assert!(HostPlan::decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    assert!(p.verify_startup(b"changed").is_err());
    let mut changed = p.clone();
    changed.terminal.input_enabled = true;
    assert!(changed.validate().is_err());
    let mut changed = p.clone();
    changed.deadline_boot_ms += 1;
    assert!(changed.validate().is_err());
    let mut legacy: LaunchManifest =
        serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    legacy.bindings.session = p.session;
    legacy.validate().unwrap();
    legacy.bindings.task_id = Uuid::nil();
    legacy.bindings.attempt_id = Uuid::nil();
    assert!(legacy.validate().is_err());
}
#[test]
fn supervisor_session_permit_binds_every_plan_input_and_original_deadline() {
    let p = fixture();
    let grant = permit(&p, 1000);
    let now = grant.issued_boot_ms;
    p.validate_permit_at(&grant, p.machine_boot_id, now)
        .unwrap();
    assert!(p.validate_permit_at(&grant, Uuid::new_v4(), now).is_err());
    assert!(
        p.validate_permit_at(&grant, p.machine_boot_id, now - 1)
            .is_err()
    );
    assert!(
        p.validate_permit_at(&grant, p.machine_boot_id, grant.expires_boot_ms)
            .is_err()
    );
    let mut over = grant.clone();
    over.expires_boot_ms = p.deadline_boot_ms + 1;
    assert!(p.validate_permit_at(&over, p.machine_boot_id, now).is_err());
    for field in ["policy", "execution"] {
        let mut value = serde_json::to_value(&p).unwrap();
        value[field] = serde_json::to_value(pin("different")).unwrap();
        let changed = HostPlan::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            changed
                .validate_permit_at(&grant, p.machine_boot_id, now)
                .is_err()
        );
    }
    let mut changed = p.clone();
    changed.input.dispatch_id = Uuid::new_v4();
    assert!(
        changed
            .validate_permit_at(&grant, p.machine_boot_id, now)
            .is_err()
    );
    changed.input.host_operation_id = changed.input.dispatch_id;
    assert!(changed.validate().is_err());
    let mut changed = p.clone();
    changed.input.batch = pin("different-batch");
    assert!(
        changed
            .validate_permit_at(&grant, p.machine_boot_id, now)
            .is_err()
    );
    let mut changed = p.clone();
    changed.subject = HostSubject::CentralConversation {
        actor_id: Uuid::new_v4(),
        conversation_id: Uuid::new_v4(),
        conversation_revision: 1,
    };
    assert!(
        changed
            .validate_permit_at(&grant, p.machine_boot_id, now)
            .is_err()
    );
    let mut changed = p.clone();
    if let SessionIdentity::Central { generation, .. } = &mut changed.session {
        *generation += 1;
    }
    assert!(
        changed
            .validate_permit_at(&grant, p.machine_boot_id, now)
            .is_err()
    );
}

struct Adapter {
    mismatch: bool,
}
impl HostAdapter for Adapter {
    fn prepare(&self, p: &HostPlan, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: if self.mismatch { Digest::of(b"wrong") } else { p.digest()? },
            adapter: p.harness.adapter.clone(), cli_version: p.harness.cli_version.as_str().into(),
            environment: p.harness.environment.clone(), capabilities: p.harness.required_capabilities.clone(),
            command: LocalCommand { executable: "/bin/sh".into(), arguments: vec!["-c".into(), "cat \"$AQ_SESSION_DIRECTORY/context-0.bin\" > \"$AQ_SESSION_DIRECTORY/delivered\"; exec /bin/sleep 30".into()], environment: Default::default() },
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
struct Authority {
    root: std::path::PathBuf,
    events: Vec<&'static str>,
    fail: Option<&'static str>,
    malformed_start: bool,
}
impl Authority {
    fn write(&mut self, stage: &'static str, data: Vec<u8>) -> Result<()> {
        ensure!(self.fail != Some(stage), "injected callback failure");
        durable_new(&self.root.join(stage), &data)?;
        self.events.push(stage);
        Ok(())
    }
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        self.write("intent", serde_json::to_vec(i)?)
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        self.write("guardian", serde_json::to_vec(i)?)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        if self.malformed_start {
            // This is genuinely malformed published input, not a slow writer.
            durable_new(
                &self
                    .root
                    .join(i.launch_id.to_string())
                    .join("start.request"),
                b"{",
            )?;
        }
        self.write("start", vec![])
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        self.write("running", serde_json::to_vec(o)?)
    }
}
struct Case {
    dir: tempfile::TempDir,
    plan: HostPlan,
    local: LocalEnvironment,
    authority: Authority,
    id: Uuid,
}
impl Case {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("aqhs-")
            .tempdir_in("/tmp")
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let plan = fixture();
        let version = Command::new("/usr/bin/tmux")
            .arg("-V")
            .output()
            .expect("tmux required for physical qualification");
        assert!(version.status.success());
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(pin("admission")),
            workspace: dir.path().canonicalize().unwrap(),
            helper_executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: String::from_utf8(version.stdout).unwrap().trim().into(),
            credential_profiles: vec![],
            context_material: vec![],
        };
        let authority = Authority {
            root: dir.path().into(),
            events: vec![],
            fail: None,
            malformed_start: false,
        };
        Self {
            dir,
            plan,
            local,
            authority,
            id: Uuid::new_v4(),
        }
    }
    fn launch(&mut self, duration: u64) -> Result<SessionHandle> {
        SessionHost::launch_plan(
            self.dir.path(),
            self.id,
            &self.plan,
            b"supervisor startup\n",
            permit(&self.plan, duration),
            &self.local,
            &Adapter { mismatch: false },
            &mut self.authority,
        )
    }
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    while !predicate() {
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn supervisor_session_starts_under_central_permit() {
    for iteration in 0..50 {
        let mut c = Case::new();
        let h = c.launch(4000).unwrap_or_else(|error| {
            panic!("central start iteration {iteration} failed: {error:#}")
        });
        assert_eq!(h.observe().unwrap().phase, HostPhase::Running);
        assert_eq!(
            c.authority.events,
            ["intent", "guardian", "start", "running"]
        );
        wait(|| h.directory().join("delivered").exists());
        assert_eq!(
            std::fs::read(h.directory().join("delivered")).unwrap(),
            b"supervisor startup\n"
        );
        h.stop().unwrap();
        wait(|| h.stopped_proof().is_ok());
    }
}
#[test]
fn supervisor_session_malformed_start_still_stops_without_launching() {
    let mut c = Case::new();
    c.authority.malformed_start = true;
    assert!(c.launch(4000).is_err());
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(c.dir.path().join("intent")).unwrap()).unwrap();
    let state_path = c.dir.path().join(c.id.to_string()).join("state.json");
    wait(|| {
        let state: HostObservation =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        state.phase == HostPhase::Stopped
    });
    let h = SessionHost::recover(c.dir.path(), &intent).unwrap();
    wait(|| h.stopped_proof().is_ok());
    let stopped = h.observe().unwrap();
    assert!(stopped.stop_reason.unwrap().contains("startup failed:"));
    assert!(stopped.pane.is_none());
    assert!(!h.directory().join("delivered").exists());
    assert!(!h.directory().join("tmux.sock").exists());
}
#[test]
fn supervisor_session_physical_restart_observation_renew_and_stopped_proof() {
    let mut c = Case::new();
    let h = c.launch(4000).unwrap();
    assert_eq!(
        c.authority.events,
        ["intent", "guardian", "start", "running"]
    );
    assert_eq!(
        h.identity().intent.manifest_digest,
        c.plan.digest().unwrap()
    );
    assert!(h.stopped_proof().is_err());
    wait(|| h.directory().join("delivered").exists());
    assert_eq!(
        std::fs::read(h.directory().join("delivered")).unwrap(),
        b"supervisor startup\n"
    );
    let sealed = std::fs::read_to_string(h.directory().join("launch.json")).unwrap();
    assert!(!sealed.contains("task_id") && !sealed.contains("attempt_id"));
    let identity = h.identity().clone();
    drop(h);
    let h = SessionHost::observe_existing(c.dir.path(), &identity).unwrap();
    assert_eq!(c.authority.events.len(), 4); // Observation cannot fabricate callbacks.
    let mut stale = identity.clone();
    stale.nonce = Uuid::new_v4();
    assert!(SessionHost::observe_existing(c.dir.path(), &stale).is_err());
    let mut renew = permit(&c.plan, 6000);
    renew.sequence = 3;
    assert!(h.renew(&renew).is_err());
    renew.sequence = 2;
    h.renew(&renew).unwrap();
    h.renew(&renew).unwrap();
    wait(|| h.observe().unwrap().permit_sequence == 2);
    let mut changed = renew.clone();
    changed.expires_boot_ms += 1;
    assert!(h.renew(&changed).is_err());
    let mut shrinking = renew.clone();
    shrinking.sequence = 3;
    shrinking.expires_boot_ms -= 1;
    assert!(h.renew(&shrinking).is_err());
    h.stop().unwrap();
    assert!(h.renew(&renew).is_err());
    wait(|| h.stopped_proof().is_ok());
    let proof = h.stopped_proof().unwrap();
    assert_eq!(proof.guardian(), &identity);
    assert!(proof.observation().kernel_wait_echild);
    assert!(SessionHost::observe_existing(c.dir.path(), &identity).is_err());
    assert!(c.launch(1000).is_err()); // Never silently replay launch identity.
}
#[test]
fn supervisor_session_expiry_and_recovery_do_not_resurrect() {
    let mut c = Case::new();
    let h = c.launch(4000).unwrap();
    wait(|| h.stopped_proof().is_ok());
    let mut renewed = permit(&c.plan, 2000);
    renewed.sequence = 2;
    assert!(h.renew(&renewed).is_err());
    let mut c = Case::new();
    let h = c.launch(4000).unwrap();
    let intent = h.identity().intent.clone();
    drop(h);
    let recovered = SessionHost::recover(c.dir.path(), &intent).unwrap();
    wait(|| recovered.stopped_proof().is_ok());
}
#[test]
fn supervisor_session_callback_failures_never_replay_start() {
    for stage in ["intent", "guardian", "start", "running"] {
        let mut c = Case::new();
        c.authority.fail = Some(stage);
        assert!(c.launch(4000).is_err());
        let launch_dir = c.dir.path().join(c.id.to_string());
        assert!(!c.dir.path().join(stage).exists());
        if stage == "intent" {
            assert!(!launch_dir.join("guardian.claim").exists());
        } else {
            let intent: LaunchIntent =
                serde_json::from_slice(&std::fs::read(c.dir.path().join("intent")).unwrap())
                    .unwrap();
            if stage != "running" {
                assert!(!launch_dir.join("start.request").exists());
                assert!(!launch_dir.join("delivered").exists());
            }
            let recovered = SessionHost::recover(c.dir.path(), &intent).unwrap();
            wait(|| recovered.stopped_proof().is_ok());
        }
        assert!(c.launch(1000).is_err());
    }
}
#[test]
fn supervisor_session_adapter_mismatch_refuses_before_durable_intent() {
    let mut c = Case::new();
    assert!(
        SessionHost::launch_plan(
            c.dir.path(),
            c.id,
            &c.plan,
            b"supervisor startup\n",
            permit(&c.plan, 1000),
            &c.local,
            &Adapter { mismatch: true },
            &mut c.authority
        )
        .is_err()
    );
    assert!(c.authority.events.is_empty());
    assert!(!c.dir.path().join(c.id.to_string()).exists());
}
#[test]
fn supervisor_session_guardian_loss_is_unknown_even_after_processes_exit() {
    let mut c = Case::new();
    let h = c.launch(4000).unwrap();
    let state = h.observe().unwrap();
    let pane = state.pane.unwrap();
    let guardian = ownership::verified_pidfd(&h.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGKILL).unwrap();
    wait(|| !ownership::pidfd_is_live(&guardian).unwrap());
    for process in [&pane.pane, &pane.server] {
        if let Ok(fd) = ownership::verified_pidfd(process) {
            ownership::signal(&fd, libc::SIGKILL).unwrap();
        }
    }
    assert_eq!(h.observe().unwrap().phase, HostPhase::StopUncertain);
    assert!(h.stopped_proof().is_err());
    assert!(SessionHost::observe_existing(c.dir.path(), h.identity()).is_err());
}
