//! Explicit local process qualification: cargo test -p aq-session-host
//! --features test-support --test local_tmux -- --ignored
use anyhow::Result;
use aq_session_host::*;
use aq_session_protocol::{LaunchManifest, SessionIdentity, canonical_digest};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use uuid::Uuid;
fn wait<T>(f: impl FnMut() -> Option<T>) -> T {
    wait_named("local process predicate", f)
}
fn wait_named<T>(label: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "{label} timeout");
        std::thread::sleep(Duration::from_millis(2));
    }
}
struct Adapter {
    mode: String,
    dir: PathBuf,
}
impl HarnessAdapter for Adapter {
    fn prepare(&self, m: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            resource_qualification: None,
            manifest_digest: canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            command: LocalCommand {
                executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
                arguments: vec![
                    "fixture".into(),
                    self.dir.to_str().unwrap().into(),
                    self.mode.clone(),
                ],
                environment: Default::default(),
            },
        })
    }
    fn readiness(&self, o: &HostObservation) -> Result<HarnessReadiness> {
        Ok(if o.phase == HostPhase::Stopped {
            HarnessReadiness::Exited
        } else if self.dir.join("fixture.claim").exists() {
            HarnessReadiness::Ready
        } else {
            HarnessReadiness::Unknown
        })
    }
}
struct Authority {
    root: PathBuf,
    pause: Option<String>,
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        durable_new(
            &self.root.join("caller-intent.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-guardian.json"),
            &serde_json::to_vec(i)?,
        )
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        durable_new(
            &self.root.join("caller-start"),
            b"durable start authorization\n",
        )
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        durable_new(
            &self.root.join("caller-started.json"),
            &serde_json::to_vec(o)?,
        )
    }
    fn checkpoint(&mut self, s: &str) -> Result<()> {
        if s == "launch-intent-durable" {
            let auth = self
                .root
                .join(Uuid::from_u128(2).to_string())
                .join("codex-home/auth.json");
            if auth.exists() {
                anyhow::ensure!(
                    self.root.join("caller-intent.json").exists()
                        && std::fs::metadata(auth)?.len() == 0,
                    "private auth published before durable intent"
                );
                durable_new(
                    &self.root.join("auth-order.checked"),
                    b"durable intent precedes private publication",
                )?;
            }
        }
        if self.pause.as_deref() == Some(s) {
            durable_new(&self.root.join("caller-checkpoint"), s.as_bytes())?;
            loop {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        Ok(())
    }
}
struct Case {
    temp: TempDir,
    manifest: LaunchManifest,
    local: LocalEnvironment,
    adapter: Adapter,
    authority: Authority,
    id: Uuid,
}
impl Case {
    fn new(mode: &str, central: bool) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("aqh-")
            .tempdir_in("/tmp")
            .unwrap();
        Self::at(temp, mode, central)
    }
    fn at(temp: TempDir, mode: &str, central: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = Uuid::from_u128(2);
        let mut manifest: LaunchManifest =
            serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
        if central {
            manifest.bindings.session = SessionIdentity::Central {
                authority_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            };
        }
        let material = temp.path().join("context");
        std::fs::write(&material, b"fixture").unwrap();
        let local = LocalEnvironment {
            resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
            workspace: temp.path().canonicalize().unwrap(),
            helper_executable: env!("CARGO_BIN_EXE_aq-session-host").into(),
            tmux_executable: "/usr/bin/tmux".into(),
            tmux_version: "tmux 3.4".into(),
            credential_profiles: manifest
                .credentials
                .iter()
                .map(|c| c.credential_profile.clone())
                .collect(),
            context_material: vec![ContextMaterial {
                artifact: manifest.context.references[0].artifact.clone(),
                path: material,
            }],
        };
        let adapter = Adapter {
            mode: mode.into(),
            dir: temp.path().join(id.to_string()),
        };
        let authority = Authority {
            root: temp.path().into(),
            pause: None,
        };
        Self {
            temp,
            manifest,
            local,
            adapter,
            authority,
            id,
        }
    }
    fn launch(&mut self, ms: u64) -> Result<SessionHandle> {
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: self.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&self.manifest)?,
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + ms,
        };
        SessionHost::launch(
            self.temp.path(),
            self.id,
            &self.manifest,
            p,
            &self.local,
            &self.adapter,
            &mut self.authority,
        )
    }
}
fn tmux(h: &SessionHandle, args: &[&str]) -> std::process::Output {
    Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(h.directory().join("tmux.sock"))
        .args(args)
        .output()
        .unwrap()
}
fn terminate(e: &ProcessEvidence) {
    if let Ok(fd) = ownership::verified_pidfd(e) {
        let _ = ownership::signal(&fd, libc::SIGKILL);
    }
}

fn io_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
fn io_setup() -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    io_setup_mode("terminal-journal")
}
fn io_setup_mode(
    mode: &str,
) -> (
    Case,
    SessionHandle,
    terminal_io::TerminalHandle,
    aq_session_protocol::ControllerLease,
) {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::{Effect, Fence};
    let mut case = Case::new(mode, false);
    let host = case.launch(8000).unwrap();
    wait(|| {
        case.adapter
            .dir
            .join("fixture.claim")
            .exists()
            .then_some(())
    });
    let terminal = TerminalIdentity {
        session: case.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let io = host.bind_terminal(&terminal).unwrap();
    let now = io_now();
    let lease = ControllerLease {
        terminal: terminal.clone(),
        principal_id: Text::new("synthetic-owner").unwrap(),
        lease_id: Uuid::new_v4(),
        generation: 1,
        issued_at_ms: now,
        expires_at_ms: now + 6000,
        execution_expires_at_ms: now + 7000,
    };
    let fence = Fence {
        terminal,
        fence_id: Uuid::new_v4(),
        generation: 1,
        controller: Some(lease.clone()),
    };
    io.submit(
        &Effect::Fence(fence.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let receipt = wait(|| io.receipt(fence.fence_id).ok().flatten());
    assert!(matches!(receipt, terminal_io::Receipt::Fence(_)));
    (case, host, io, lease)
}
fn io_input(
    lease: &aq_session_protocol::ControllerLease,
    bytes: &[u8],
) -> aq_session_protocol::InputCommand {
    let now = io_now();
    aq_session_protocol::InputCommand {
        operation_id: Uuid::new_v4(),
        lease: lease.clone(),
        issued_at_ms: now,
        deadline_ms: now + 1500,
        bytes: bytes.to_vec(),
    }
}
fn io_lines(host: &SessionHandle) -> Vec<String> {
    std::fs::read_to_string(host.directory().join("fixture-input.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guarded_literal_unicode_resize_and_durable_duplicate_receipts() {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    let input = io_input(&lease, "literal; $(no-shell) 雪🙂\n".as_bytes());
    assert!(
        io.submit(&Effect::Input(input.clone()), clock)
            .unwrap()
            .is_none()
    );
    let receipt = wait(|| io.receipt(input.operation_id).ok().flatten());
    assert!(
        matches!(receipt, terminal_io::Receipt::Input(ref v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    assert_eq!(io_lines(&host), ["literal; $(no-shell) 雪🙂"]);
    let recovered = terminal_io::TerminalHandle::recover(&host, &lease.terminal).unwrap();
    assert!(
        matches!(recovered.submit(&Effect::Input(input.clone()), clock).unwrap(),
        Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::Applied)
    );
    let mut changed = input.clone();
    changed.bytes = b"different\n".to_vec();
    assert!(io.submit(&Effect::Input(changed), clock).is_err());
    let now = io_now();
    let resize = ResizeCommand {
        operation_id: Uuid::new_v4(),
        lease,
        sequence: 1,
        issued_at_ms: now,
        deadline_ms: now + 1500,
        columns: 100,
        rows: 35,
    };
    io.submit(&Effect::Resize(resize.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(resize.operation_id).ok().flatten()),
        terminal_io::Receipt::Resize(v) if v.applied)
    );
    let size = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            &io.binding().pane.pane_id,
            "#{window_width}x#{window_height}",
        ],
    );
    assert_eq!(String::from_utf8(size.stdout).unwrap().trim(), "100x35");
    assert_eq!(io_lines(&host).len(), 1);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn injecting_crash_before_or_after_physical_effect_is_unknown_never_replayed() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay_protocol::Effect;
    for stage in ["before-effect", "after-effect"] {
        let (_case, host, io, lease) = io_setup();
        let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
        durable_new(
            &host.directory().join(format!("terminal-fault-{stage}.arm")),
            b"synthetic fault\n",
        )
        .unwrap();
        let input = io_input(&lease, b"exactly-once-or-unknown\n");
        io.submit(&Effect::Input(input.clone()), clock).unwrap();
        wait(|| {
            host.directory()
                .join("terminal-fault.reached")
                .exists()
                .then_some(())
        });
        if stage == "after-effect" {
            wait(|| (io_lines(&host).len() == 1).then_some(()));
        }
        terminate(&host.identity().process);
        wait(|| {
            (!ownership::verified_pidfd(&host.identity().process)
                .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
            .then_some(())
        });
        let receipt = io.receipt(input.operation_id).unwrap().unwrap();
        assert!(
            matches!(receipt, terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
        );
        assert!(matches!(io.submit(&Effect::Input(input), clock).unwrap(),
            Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::DeliveryUnknown));
        assert_eq!(io_lines(&host).len(), usize::from(stage == "after-effect"));
        assert_eq!(host.observe().unwrap().phase, HostPhase::StopUncertain);
        // External fixture cleanup is not presented as a guardian stopped proof.
        terminate(&io.binding().pane.pane);
        terminate(&io.binding().pane.server);
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn encrypted_mailbox_tamper_and_missing_key_refuse_without_stopping_expiry() {
    use aq_terminal_relay_protocol::Effect;
    for missing_key in [false, true] {
        let (_case, mut host, io, lease) = io_setup();
        let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
        ownership::signal(&guardian, libc::SIGSTOP).unwrap();
        let input = io_input(&lease, b"private-input-must-not-be-in-file\n");
        io.submit(
            &Effect::Input(input.clone()),
            terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
        )
        .unwrap();
        let path = host.directory().join("terminal-effect.request");
        let encoded = std::fs::read(&path).unwrap();
        assert!(!encoded.windows(input.bytes.len()).any(|v| v == input.bytes));
        if missing_key {
            std::fs::remove_file(host.directory().join("terminal.key")).unwrap();
            assert!(terminal_io::TerminalHandle::recover(&host, &lease.terminal).is_err());
        } else {
            let mut value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            let bytes = value["bytes"].as_array_mut().unwrap();
            let old = bytes[0].as_u64().unwrap();
            bytes[0] = serde_json::json!(old ^ 1);
            std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        ownership::signal(&guardian, libc::SIGCONT).unwrap();
        wait(|| {
            host.directory()
                .join("terminal.disabled")
                .exists()
                .then_some(())
        });
        assert!(io_lines(&host).is_empty());
        assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
        assert!(
            io.submit(
                &Effect::Input(io_input(&lease, b"refused\n")),
                terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap()
            )
            .is_err()
        );
        host.stop().unwrap();
        assert_eq!(
            host.wait_stopped(Duration::from_secs(5)).unwrap().phase,
            HostPhase::Stopped
        );
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn paused_injecting_deadline_and_new_controller_cannot_clear_unknown() {
    use aq_session_protocol::*;
    use aq_terminal_relay_protocol::{Effect, Fence};
    let (_case, mut host, io, mut lease) = io_setup();
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    durable_new(
        &host.directory().join("terminal-fault-before-effect.arm"),
        b"synthetic fault\n",
    )
    .unwrap();
    let mut input = io_input(&lease, b"must-expire-before-effect\n");
    input.deadline_ms = input.issued_at_ms + 200;
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    wait(|| {
        host.directory()
            .join("terminal-fault.reached")
            .exists()
            .then_some(())
    });
    wait(|| (io_now() >= input.deadline_ms).then_some(()));
    ownership::signal(
        &ownership::verified_pidfd(&host.identity().process).unwrap(),
        libc::SIGCONT,
    )
    .unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
    );
    lease.generation = 2;
    lease.lease_id = Uuid::new_v4();
    lease.issued_at_ms = io_now();
    lease.expires_at_ms = lease.issued_at_ms + 4000;
    let fence = Fence {
        terminal: lease.terminal.clone(),
        fence_id: Uuid::new_v4(),
        generation: 2,
        controller: Some(lease.clone()),
    };
    io.submit(&Effect::Fence(fence.clone()), clock).unwrap();
    wait(|| io.receipt(fence.fence_id).ok().flatten());
    let next = io_input(&lease, b"cannot-clear-unknown-by-takeover\n");
    io.submit(&Effect::Input(next.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(next.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
    assert!(io_lines(&host).is_empty());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn exact_observation_and_unstarted_recovery_never_launch_or_inject() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup();
    let started = std::fs::read(case.temp.path().join("caller-started.json")).unwrap();
    let observed = SessionHost::observe_existing(case.temp.path(), host.identity()).unwrap();
    assert_eq!(observed.identity(), host.identity());
    assert_eq!(
        std::fs::read(case.temp.path().join("caller-started.json")).unwrap(),
        started
    );
    let mut wrong = host.identity().clone();
    wrong.nonce = Uuid::new_v4();
    assert!(SessionHost::observe_existing(case.temp.path(), &wrong).is_err());
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let input = io_input(&lease, b"cancel-before-physical-intent\n");
    io.submit(
        &Effect::Input(input.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    assert!(
        io.reject_unstarted_input(
            input.operation_id,
            &aq_session_protocol::Digest::of(b"wrong")
        )
        .is_err()
    );
    let receipt = io
        .reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap())
        .unwrap();
    assert!(
        matches!(receipt, terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert!(!host.directory().join("terminal-effect.request").exists());
    assert!(
        matches!(io.submit(&Effect::Input(input), terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap()).unwrap(),
        Some(terminal_io::Receipt::Input(v)) if v.disposition == InputDisposition::Rejected)
    );
    assert!(io_lines(&host).is_empty());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn tmux_pane_identifier_reuse_does_not_authorize_the_replacement_process() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup();
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let replacement = host.directory().join("synthetic-replacement");
    std::fs::create_dir(&replacement).unwrap();
    let old_pane = io.binding().pane.clone();
    // Deliberate external fixture tampering, never an exposed bridge command.
    assert!(
        tmux(
            &host,
            &[
                "respawn-pane",
                "-k",
                "-t",
                &old_pane.pane_id,
                "--",
                env!("CARGO_BIN_EXE_aq-session-host"),
                "fixture",
                replacement.to_str().unwrap(),
                "terminal-journal"
            ]
        )
        .status
        .success()
    );
    wait(|| replacement.join("fixture.claim").exists().then_some(()));
    let pid = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            &old_pane.pane_id,
            "#{pane_pid}",
        ],
    );
    assert_ne!(
        String::from_utf8(pid.stdout).unwrap().trim(),
        old_pane.pane.pid.to_string()
    );
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
    let input = io_input(&lease, b"must-not-reach-replacement\n");
    io.submit(
        &Effect::Input(input.clone()),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert_eq!(
        host.wait_stopped(Duration::from_secs(5)).unwrap().phase,
        HostPhase::Stopped
    );
    assert!(!replacement.join("fixture-input.jsonl").exists());
    assert!(
        matches!(io.reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap()).unwrap(),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Rejected)
    );
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn replayed_valid_encrypted_ledger_cannot_repeat_an_applied_input() {
    use aq_session_protocol::{InputDisposition, canonical_digest};
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup();
    let original = std::fs::read(host.directory().join("terminal-state.json")).unwrap();
    let input = io_input(&lease, b"one-physical-effect\n");
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    // This is valid old ciphertext under the same key/AAD, not random damage.
    std::fs::write(host.directory().join("terminal-state.json"), original).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    wait(|| {
        host.directory()
            .join("terminal.disabled")
            .exists()
            .then_some(())
    });
    assert!(
        matches!(io.reject_unstarted_input(input.operation_id, &canonical_digest(&input).unwrap()).unwrap(),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::DeliveryUnknown)
    );
    assert_eq!(io_lines(&host), ["one-physical-effect"]);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn copied_launch_directory_and_old_mailbox_cannot_reset_physical_input_history() {
    use aq_session_protocol::InputDisposition;
    use aq_terminal_relay_protocol::Effect;
    use std::os::unix::fs::{DirBuilderExt, symlink};
    let (case, mut host, io, lease) = io_setup();
    let guardian = ownership::verified_pidfd(&host.identity().process).unwrap();
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let original_state = std::fs::read(host.directory().join("terminal-state.json")).unwrap();
    let input = io_input(&lease, b"cannot-reset-history-by-directory-copy\n");
    let clock = terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap();
    io.submit(&Effect::Input(input.clone()), clock).unwrap();
    let mailbox = std::fs::read(host.directory().join("terminal-effect.request")).unwrap();
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    assert!(
        matches!(wait(|| io.receipt(input.operation_id).ok().flatten()),
        terminal_io::Receipt::Input(v) if v.disposition == InputDisposition::Applied)
    );
    wait(|| (io_lines(&host).len() == 1).then_some(()));
    ownership::signal(&guardian, libc::SIGSTOP).unwrap();
    let retained = case.temp.path().join("retained-launch");
    std::fs::rename(host.directory(), &retained).unwrap();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(host.directory())
        .unwrap();
    for name in [
        "launch.json",
        "identity.json",
        "state.json",
        "directory.json",
        "terminal-binding.json",
        "terminal.key",
    ] {
        std::fs::copy(retained.join(name), host.directory().join(name)).unwrap();
    }
    durable_new(
        &host.directory().join("terminal-state.json"),
        &original_state,
    )
    .unwrap();
    durable_new(&host.directory().join("terminal-effect.request"), &mailbox).unwrap();
    symlink(
        retained.join("tmux.sock"),
        host.directory().join("tmux.sock"),
    )
    .unwrap();
    assert!(io.submit(&Effect::Input(input), clock).is_err());
    assert!(terminal_io::TerminalHandle::recover(&host, &lease.terminal).is_err());
    assert!(SessionHost::observe_existing(case.temp.path(), host.identity()).is_err());
    ownership::signal(&guardian, libc::SIGCONT).unwrap();
    wait(|| {
        host.directory()
            .join("terminal.disabled")
            .exists()
            .then_some(())
    });
    assert!(!host.directory().join("fixture-input.jsonl").exists());
    let previous = std::fs::read_to_string(retained.join("fixture-input.jsonl")).unwrap();
    assert_eq!(previous.lines().count(), 1);
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn read_only_output_preserves_unicode_alternate_bytes_and_does_not_resize_or_restart() {
    use aq_session_host::terminal_output::{ControlEvent, OutputObserver};
    use aq_terminal_relay_protocol::Effect;
    let (case, mut host, io, lease) = io_setup_mode("output-pattern");
    let before = tmux(
        &host,
        &[
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{window_width}x#{window_height}",
        ],
    )
    .stdout;
    let mut observer = OutputObserver::open(&io).unwrap();
    wait(|| {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    assert_eq!(
        before,
        tmux(
            &host,
            &[
                "display-message",
                "-p",
                "-t",
                "aq:0.0",
                "#{window_width}x#{window_height}"
            ]
        )
        .stdout
    );
    let input = io_input(&lease, b"emit\n");
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let mut output = vec![];
    let expected = "\x1b[?1049h\x1b[2J\x1b[3;4H雪🙂e\u{301}\x1b[?1049l\r\n".as_bytes();
    wait(|| {
        for event in observer.poll().unwrap() {
            if let ControlEvent::Bytes(bytes) = event {
                output.extend(bytes);
            }
        }
        output
            .windows(expected.len())
            .any(|w| w == expected)
            .then_some(())
    });
    assert!(OutputObserver::open(&io).is_err());
    let process = observer.process().clone();
    drop(observer);
    wait(|| {
        (!ownership::verified_pidfd(&process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
        .then_some(())
    });
    assert!(OutputObserver::open(&io).is_err());
    std::fs::remove_file(host.directory().join("terminal-output.claim")).unwrap();
    assert!(
        OutputObserver::open(&io).is_err(),
        "encrypted one-shot claim survives missing marker"
    );
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    if let Some(root) = std::env::var_os("AQ_HOST_COMPAT_EVIDENCE_DIR") {
        use std::os::unix::fs::MetadataExt;
        let root = PathBuf::from(root);
        let metadata = std::fs::symlink_metadata(&root).unwrap();
        assert!(
            root.is_absolute()
                && metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o700
        );
        let running: HostObservation = serde_json::from_slice(
            &std::fs::read(case.temp.path().join("caller-started.json")).unwrap(),
        )
        .unwrap();
        let proof = host.stopped_proof().unwrap();
        assert_eq!(running.phase, HostPhase::Running);
        assert_eq!(&running.guardian, proof.guardian());
        assert_eq!(running.pane, proof.observation().pane);
        assert!(
            proof.observation().kernel_wait_echild
                && proof.observation().root_status.is_some()
                && proof.observation().reaped_count > 0
        );
        durable_new(&root.join("host-proof-pair.json"), &serde_json::to_vec_pretty(&serde_json::json!({
            "running":running,"stopped_proof":proof,"synthetic":true,"host_version":env!("CARGO_PKG_VERSION")
        })).unwrap()).unwrap();
    }
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn sole_sigstopped_observer_does_not_block_pty_progress_or_guardian_control() {
    use aq_session_host::terminal_output::OutputObserver;
    use aq_terminal_relay_protocol::Effect;
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = OutputObserver::open(&io).unwrap();
    wait_named("output observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    ownership::signal(&client, libc::SIGSTOP).unwrap();
    wait_named("observer stopped or independently severed", || {
        if !ownership::pidfd_is_live(&client).unwrap() {
            return Some(());
        }
        std::fs::read_to_string(format!("/proc/{}/status", observer.process().pid))
            .ok()
            .is_some_and(|s| {
                s.lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'))
            })
            .then_some(())
    });
    let input = io_input(&lease, b"flood\n");
    let operation_id = input.operation_id;
    io.submit(
        &Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    let stopped = Instant::now();
    let mut progress: u64;
    let progress_deadline = stopped + Duration::from_secs(5);
    loop {
        progress = std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if progress >= 4 * 1024 * 1024 && stopped.elapsed() >= Duration::from_millis(1300) {
            break;
        }
        if Instant::now() >= progress_deadline {
            eprintln!(
                "stopped-observer diagnostic receipt={:?} host={:?} root={}",
                io.receipt(operation_id),
                host.observe(),
                host.directory().display()
            );
            if let Some(pane) = host.observe().ok().and_then(|o| o.pane) {
                eprintln!(
                    "synthetic start={:?} syscall={:?}",
                    std::fs::read(host.directory().join("fixture-output-start")),
                    std::fs::read_to_string(format!("/proc/{}/syscall", pane.pane.pid))
                );
                eprintln!(
                    "synthetic pane wchan={:?} status={:?}",
                    std::fs::read_to_string(format!("/proc/{}/wchan", pane.pane.pid)),
                    std::fs::read_to_string(format!("/proc/{}/status", pane.pane.pid))
                );
            }
            std::mem::forget(_case);
            panic!(
                "stopped observer PTY progress timeout: {progress} bytes after {:?}",
                stopped.elapsed()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        !ownership::pidfd_is_live(&client).unwrap(),
        "guardian must sever the stopped read-only client"
    );
    let boot = ownership::raw_boot_ms();
    host.renew(&ExecutionPermit {
        session: lease.terminal.session.clone(),
        manifest_digest: host.identity().intent.manifest_digest.clone(),
        sequence: 2,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 6000,
    })
    .unwrap();
    wait_named("guardian applied independent renewal", || {
        (host.observe().unwrap().permit_sequence == 2).then_some(())
    });
    let lost = observer.poll();
    assert!(
        lost.is_err()
            || lost
                .unwrap()
                .iter()
                .any(|e| matches!(e, aq_session_host::terminal_output::ControlEvent::Exited)),
        "reader cutoff must invalidate continuity"
    );
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(5)).unwrap();
    eprintln!("synthetic output progress with stopped sole observer: {progress} bytes");
}

#[test]
#[ignore = "explicit local tmux 3.4 bounded output qualification"]
fn execution_expiry_stops_writers_even_while_observer_client_remains_sigstopped() {
    use aq_session_host::terminal_output::OutputObserver;
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = OutputObserver::open(&io).unwrap();
    wait(|| {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    ownership::signal(&client, libc::SIGSTOP).unwrap();
    wait(|| {
        if !ownership::pidfd_is_live(&client).unwrap() {
            return Some(());
        }
        std::fs::read_to_string(format!("/proc/{}/status", observer.process().pid))
            .ok()
            .is_some_and(|s| {
                s.lines()
                    .any(|line| line.starts_with("State:") && line.contains('T'))
            })
            .then_some(())
    });
    let boot = ownership::raw_boot_ms();
    host.renew(&ExecutionPermit {
        session: lease.terminal.session,
        manifest_digest: host.identity().intent.manifest_digest.clone(),
        sequence: 2,
        issued_boot_ms: boot,
        expires_boot_ms: boot + 500,
    })
    .unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    assert!(
        !ownership::pidfd_is_live(&client).unwrap(),
        "guardian-owned stopped reader must be reaped too"
    );
    let process = observer.process().clone();
    drop(observer);
    wait(|| {
        (!ownership::verified_pidfd(&process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false)))
        .then_some(())
    });
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn central_and_remote_interaction_stop_and_no_relaunch() {
    for central in [false, true] {
        let mut c = Case::new("interactive", central);
        let mut h = c.launch(5000).unwrap();
        wait(|| std::fs::read_to_string(h.directory().join("fixture.claim")).ok());
        let o = h.observe().unwrap();
        let pane = o.pane.unwrap();
        assert_eq!(
            pane.server.pid,
            serde_json::from_slice::<ProcessEvidence>(
                &std::fs::read(h.directory().join("server.json")).unwrap()
            )
            .unwrap()
            .pid
        );
        assert!(
            tmux(&h, &["send-keys", "-l", "hello; $(unsafe)"])
                .status
                .success()
        );
        assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
        wait(|| {
            h.capture_text()
                .ok()
                .filter(|s| s.contains("echo:hello; $(unsafe)"))
        });
        h.stop().unwrap();
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        assert_eq!(h.stopped_proof().unwrap().guardian(), h.identity());
        assert!(ownership::process_evidence(pane.server.pid).is_err());
        assert!(h.capture_text().is_err());
        assert!(
            !tmux(&h, &["display-message", "-p", "#{pid}"])
                .status
                .success()
        );
        assert!(c.launch(5000).is_err());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn pane_exit_has_no_fallback_and_escaped_descendants_require_kernel_stop() {
    let mut c = Case::new("escaped", false);
    let mut h = c.launch(5000).unwrap();
    wait(|| std::fs::read(h.directory().join("escaped-process.json")).ok());
    let e: ProcessEvidence =
        serde_json::from_slice(&std::fs::read(h.directory().join("escaped-process.json")).unwrap())
            .unwrap();
    assert!(tmux(&h, &["send-keys", "-l", "exit"]).status.success());
    assert!(tmux(&h, &["send-keys", "Enter"]).status.success());
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild && s.reaped_count >= 2);
    assert!(ownership::process_evidence(e.pid).is_err());
    assert_eq!(
        std::fs::read(h.directory().join("pane-exec.claim")).unwrap(),
        b"one native harness exec; no fallback shell\n"
    );
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn permit_expiry_is_independent_and_late_renewal_cannot_revive() {
    let mut c = Case::new("interactive", false);
    let mut h = c.launch(500).unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.stop_reason.as_deref().unwrap().contains("expired"));
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 1000,
    };
    assert!(h.renew(&p).is_err());
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_loss_remains_uncertain_even_after_known_processes_disappear() {
    let mut c = Case::new("interactive", true);
    let h = c.launch(5000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let o = h.observe().unwrap();
    terminate(&o.guardian.process);
    wait(|| (h.observe().unwrap().phase == HostPhase::StopUncertain).then_some(()));
    assert!(h.stop().is_err());
    let p = o.pane.unwrap();
    terminate(&p.server);
    terminate(&p.pane);
    assert_eq!(h.observe().unwrap().phase, HostPhase::StopUncertain);
}
#[test]
#[ignore = "subprocess fixture, called by local crash tests only"]
fn launch_child_fixture() {
    let Ok(root) = std::env::var("AQ_HOST_CASE_ROOT") else {
        return;
    };
    let temp = TempDir::new_in(&root).unwrap();
    let mut c = Case::at(temp, "interactive", false);
    std::fs::write(
        Path::new(&root).join("case-root"),
        c.temp.path().to_str().unwrap(),
    )
    .unwrap();
    c.authority.pause = std::env::var("AQ_HOST_CALLER_PAUSE").ok();
    let started = Instant::now();
    let handle = c.launch(1500);
    durable_new(
        &c.temp.path().join("caller-launch-result.json"),
        &serde_json::to_vec(&serde_json::json!({
            "ok": handle.is_ok(),
            "error": handle.as_ref().err().map(|e| format!("{e:#}")),
            "elapsed_ms": started.elapsed().as_millis(),
            "permit_ms": 1500,
            "caller_stage": std::env::var("AQ_HOST_CALLER_PAUSE").ok(),
            "guardian_stage": std::env::var("AQ_SESSION_HOST_FAULT_STAGE").ok(),
        }))
        .unwrap(),
    )
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    std::mem::forget(c);
}
fn spawn_case(root: &Path, stage: &str, guardian_fault: bool) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "launch_child_fixture",
            "--test-threads=1",
        ])
        .env("AQ_HOST_CASE_ROOT", root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if guardian_fault {
        command.env("AQ_SESSION_HOST_FAULT_STAGE", stage);
    } else {
        command.env("AQ_HOST_CALLER_PAUSE", stage);
    }
    command.spawn().unwrap()
}
fn wait_crash_marker(
    root: &mut Option<TempDir>,
    caller: &mut Child,
    case: &Path,
    marker: &Path,
    stage: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if marker.exists() {
            return;
        }
        let result = std::fs::read_to_string(case.join("caller-launch-result.json")).ok();
        if result.is_some() || Instant::now() >= deadline {
            let retained = root.take().unwrap().keep();
            let _ = caller.kill();
            let _ = caller.wait();
            panic!(
                "synthetic crash stage {stage}: marker not reached; launch_result={result:?}; retained={}",
                retained.display()
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn caller_crashes_at_durable_start_boundaries_never_relaunch() {
    for stage in [
        "launch-intent-durable",
        "guardian-spawned",
        "guardian-identity-durable",
        "start-intent-durable",
        "start-sent",
        "start-receipt-durable",
    ] {
        let mut root = Some(TempDir::new().unwrap());
        let root_path = root.as_ref().unwrap().path().to_path_buf();
        let mut caller = spawn_case(&root_path, stage, false);
        let case = wait(|| std::fs::read_to_string(root_path.join("case-root")).ok());
        let case = Path::new(&case);
        wait_crash_marker(
            &mut root,
            &mut caller,
            case,
            &case.join("caller-checkpoint"),
            stage,
        );
        caller.kill().unwrap();
        caller.wait().unwrap();
        let intent: LaunchIntent =
            serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap())
                .unwrap();
        if stage == "launch-intent-durable" {
            assert!(
                !case
                    .join(intent.launch_id.to_string())
                    .join("identity.json")
                    .exists()
            );
            continue;
        }
        let mut h = wait(|| SessionHost::recover(case, &intent).ok());
        let s = h
            .wait_stopped(Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("{stage}: {e:#}"));
        assert!(s.kernel_wait_echild);
        assert!(
            Command::new("/usr/bin/tmux")
                .args(["-N", "-S"])
                .arg(h.directory().join("tmux.sock"))
                .arg("list-sessions")
                .output()
                .is_ok_and(|o| !o.status.success())
        );
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn guardian_crashes_across_socket_and_pane_creation_never_claim_stop() {
    for stage in [
        "before-tmux-spawn",
        "after-tmux-spawn",
        "socket-created",
        "pane-created",
    ] {
        let mut root = Some(TempDir::new().unwrap());
        let root_path = root.as_ref().unwrap().path().to_path_buf();
        let mut caller = spawn_case(&root_path, stage, true);
        let case = wait(|| std::fs::read_to_string(root_path.join("case-root")).ok());
        let case = Path::new(&case);
        let dir = case.join(Uuid::from_u128(2).to_string());
        wait_crash_marker(
            &mut root,
            &mut caller,
            case,
            &dir.join("guardian-fault.json"),
            stage,
        );
        let identity: GuardianIdentity =
            serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
        let children: Vec<ProcessEvidence> = std::fs::read_to_string(format!(
            "/proc/{}/task/{}/children",
            identity.process.pid, identity.process.pid
        ))
        .unwrap()
        .split_whitespace()
        .filter_map(|pid| ownership::process_evidence(pid.parse().unwrap()).ok())
        .collect();
        terminate(&identity.process);
        caller.kill().unwrap();
        caller.wait().unwrap();
        wait(|| {
            SessionHost::recover(case, &identity.intent)
                .is_err()
                .then_some(())
        });
        for child in children {
            terminate(&child);
        }
        assert!(
            !serde_json::from_slice::<HostObservation>(
                &std::fs::read(dir.join("state.json")).unwrap()
            )
            .unwrap()
            .kernel_wait_echild
        );
    }
}

struct StartGate<'a> {
    inner: &'a mut Authority,
    dir: PathBuf,
    deny: bool,
    collide: bool,
}
impl LaunchAuthority for StartGate<'_> {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        self.inner.record_launch_intent(i)
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        self.inner.record_guardian(i)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        if self.deny {
            anyhow::bail!("simulated caller fsync refusal")
        }
        if self.collide {
            durable_new(&self.dir.join("tmux.sock"), b"foreign socket sentinel")?;
        }
        self.inner.record_start_intent(i, g)
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        self.inner.record_started(o)
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn failed_durable_start_and_socket_collision_never_launch_native_child() {
    for collide in [false, true] {
        let mut c = Case::new("interactive", false);
        let now = ownership::raw_boot_ms();
        let p = ExecutionPermit {
            session: c.manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&c.manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: now + 1000,
        };
        let dir = c.temp.path().join(c.id.to_string());
        let mut gate = StartGate {
            inner: &mut c.authority,
            dir: dir.clone(),
            deny: !collide,
            collide,
        };
        assert!(
            SessionHost::launch(
                c.temp.path(),
                c.id,
                &c.manifest,
                p,
                &c.local,
                &c.adapter,
                &mut gate
            )
            .is_err()
        );
        assert!(!dir.join("fixture.claim").exists());
        assert!(!dir.join("pane-exec.claim").exists());
        let intent: LaunchIntent = serde_json::from_slice(
            &std::fs::read(c.temp.path().join("caller-intent.json")).unwrap(),
        )
        .unwrap();
        let mut h = SessionHost::recover(c.temp.path(), &intent).unwrap();
        assert!(
            h.wait_stopped(Duration::from_secs(5))
                .unwrap()
                .kernel_wait_echild
        );
        if collide {
            assert_eq!(
                std::fs::read(dir.join("tmux.sock")).unwrap(),
                b"foreign socket sentinel"
            );
        } else {
            assert!(!dir.join("start.request").exists());
        }
    }
}
#[cfg(feature = "test-support")]
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn existing_pane_is_refused_and_never_used_for_the_agent() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "socket-created", true);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    let dir = case.join(Uuid::from_u128(2).to_string());
    wait(|| dir.join("guardian-fault.json").exists().then_some(()));
    let identity: GuardianIdentity =
        serde_json::from_slice(&std::fs::read(dir.join("identity.json")).unwrap()).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", identity.process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let status = Command::new("/usr/bin/tmux")
        .args(["-N", "-S"])
        .arg(dir.join("tmux.sock"))
        .args(["new-session", "-d", "-s", "aq", "/bin/sleep", "10"])
        .status()
        .unwrap();
    assert!(status.success());
    let fd = ownership::verified_pidfd(&identity.process).unwrap();
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    let stopped = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(stopped.kernel_wait_echild);
    assert!(stopped.stop_reason.unwrap().contains("startup failed"));
    assert!(!dir.join("fixture.claim").exists());
    assert!(!dir.join("pane-exec.claim").exists());
    caller.kill().unwrap();
    caller.wait().unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn local_configuration_refusals_happen_before_durable_launch_or_process() {
    for case in ["version", "credentials", "context", "hard-resources"] {
        let mut c = Case::new("interactive", false);
        match case {
            "version" => c.local.tmux_version = "tmux unsupported".into(),
            "credentials" => c.local.credential_profiles.clear(),
            "hard-resources" => {
                c.local.resource_policy = ResourcePolicy::RequireQualifiedEnvironment(
                    c.manifest.harness.environment.clone(),
                )
            }
            "context" => std::fs::write(&c.local.context_material[0].path, b"changed").unwrap(),
            _ => unreachable!(),
        }
        assert!(c.launch(1000).is_err());
        assert!(!c.temp.path().join("caller-intent.json").exists());
        assert!(!c.temp.path().join(c.id.to_string()).exists());
    }
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewals_are_monotonic_and_resource_cost_is_observed() {
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(1000).unwrap();
    wait(|| h.directory().join("fixture.claim").exists().then_some(()));
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 2000,
    };
    h.renew(&permit).unwrap();
    h.renew(&permit).unwrap();
    let mut changed = permit.clone();
    changed.expires_boot_ms += 1;
    assert!(h.renew(&changed).is_err());
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    let state = h.observe().unwrap();
    let pane = state.pane.unwrap();
    let mut rss = serde_json::Map::new();
    for (name, pid) in [
        ("guardian", state.guardian.process.pid),
        ("tmux", pane.server.pid),
        ("pane", pane.pane.pid),
    ] {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        rss.insert(name.into(), kb.into());
    }
    println!(
        "qualification_resource_rss_kib={}",
        serde_json::Value::Object(rss)
    );
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn malformed_renewal_stops_and_wall_limit_cannot_be_extended() {
    for malformed in [false, true] {
        let mut c = Case::new("interactive", false);
        if !malformed {
            c.manifest.resources.wall_time_ms = 600;
            c.manifest.resources.stop_grace_ms = 100;
        }
        let mut h = c.launch(500).unwrap();
        if malformed {
            std::fs::write(h.directory().join("renew.request"), b"{not a directive}").unwrap();
        } else {
            let now = ownership::raw_boot_ms();
            let p = ExecutionPermit {
                session: c.manifest.bindings.session.clone(),
                manifest_digest: canonical_digest(&c.manifest).unwrap(),
                sequence: 2,
                issued_boot_ms: now,
                expires_boot_ms: now + 2000,
            };
            assert!(h.renew(&p).is_err());
        }
        let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
        assert!(s.kernel_wait_echild);
        if malformed {
            assert!(s.stop_reason.unwrap().contains("running validation failed"));
        } else {
            assert!(s.stop_reason.unwrap().contains("expired"));
        }
    }
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn renewal_compare_publish_is_serialized_while_guardian_is_stopped() {
    use std::sync::Barrier;
    let mut c = Case::new("interactive", true);
    let mut h = c.launch(5000).unwrap();
    let fd = ownership::verified_pidfd(&h.identity().process).unwrap();
    ownership::signal(&fd, libc::SIGSTOP).unwrap();
    wait(|| {
        std::fs::read_to_string(format!("/proc/{}/stat", h.identity().process.pid))
            .ok()
            .filter(|s| {
                s.rsplit_once(") ")
                    .is_some_and(|(_, tail)| tail.starts_with("T "))
            })
    });
    let now = ownership::raw_boot_ms();
    let p = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: now,
        expires_boot_ms: now + 6000,
    };
    let mut changed = p.clone();
    changed.expires_boot_ms += 1;
    let barrier = Barrier::new(3);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            h.renew(&p)
        });
        let b = scope.spawn(|| {
            barrier.wait();
            h.renew(&changed)
        });
        barrier.wait();
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(a.is_ok(), b.is_ok());
    let accepted = if a.is_ok() {
        p.clone()
    } else {
        changed.clone()
    };
    let rejected = if a.is_ok() { changed } else { p };
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    ownership::signal(&fd, libc::SIGCONT).unwrap();
    wait(|| h.observe().ok().filter(|s| s.permit_sequence == 2));
    h.renew(&accepted).unwrap();
    assert!(h.renew(&rejected).is_err());
    let mut higher = accepted.clone();
    higher.sequence = 3;
    h.renew(&higher).unwrap();
    assert!(h.renew(&accepted).is_err());
    h.stop().unwrap();
    h.wait_stopped(Duration::from_secs(5)).unwrap();
}
#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn helper_expiry_survives_caller_death_without_recovery_control() {
    let root = TempDir::new().unwrap();
    let mut caller = spawn_case(root.path(), "start-receipt-durable", false);
    let case = wait(|| std::fs::read_to_string(root.path().join("case-root")).ok());
    let case = Path::new(&case);
    wait(|| case.join("caller-checkpoint").exists().then_some(()));
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(case.join("caller-intent.json")).unwrap()).unwrap();
    caller.kill().unwrap();
    caller.wait().unwrap();
    let dir = case.join(intent.launch_id.to_string());
    let state = wait(|| {
        serde_json::from_slice::<HostObservation>(&std::fs::read(dir.join("state.json")).ok()?)
            .ok()
            .filter(|s| s.phase == HostPhase::Stopped)
    });
    assert!(state.kernel_wait_echild);
    assert!(state.stop_reason.unwrap().contains("expired"));
    let h = SessionHost::recover(case, &intent).unwrap();
    assert_eq!(h.stopped_proof().unwrap().guardian().intent, intent);
}

#[test]
#[ignore = "requires explicit local tmux 3.4 qualification"]
fn sustained_adopted_child_exits_cannot_starve_expiry_or_stop_proof() {
    let mut c = Case::new("fork-flood", false);
    c.manifest.resources.stop_grace_ms = 100;
    let mut h = c.launch(700).unwrap();
    wait(|| {
        std::fs::read_to_string(h.directory().join("producer-counter"))
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n >= 16)
    });
    let producer: ProcessEvidence = serde_json::from_slice(
        &std::fs::read(h.directory().join("producer-process.json")).unwrap(),
    )
    .unwrap();
    let s = h.wait_stopped(Duration::from_secs(5)).unwrap();
    assert!(s.kernel_wait_echild);
    assert!(s.reaped_count >= 16);
    assert!(s.stop_reason.unwrap().contains("expired"));
    assert!(ownership::process_evidence(producer.pid).is_err());
}

fn rendered_material(c: &mut Case, bytes: &[u8]) -> RenderedStartup {
    use aq_session_protocol::{ArtifactRef, Digest, Text};
    let digest = Digest::of(b"runtime-validated synthetic descriptor fixture");
    let descriptor = ArtifactRef {
        id: Text::new(format!("aq.startup-delivery/{}", digest.as_str())).unwrap(),
        version: Text::new("1").unwrap(),
        digest,
    };
    c.manifest.modules.push(descriptor.clone());
    c.local.context_material.clear();
    let payload = ArtifactRef {
        id: Text::new("final-startup").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    };
    RenderedStartup::new(c.manifest.clone(), descriptor, payload, bytes.to_vec()).unwrap()
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_startup_reaches_owned_harness_as_one_exact_payload() {
    let mut c = Case::new("rendered", false);
    let bytes = "framed\nfixture\n雪\0literal; $(no shell)\n".as_bytes();
    let startup = rendered_material(&mut c, bytes);
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let mut host = SessionHost::launch_rendered(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &c.adapter,
        &mut c.authority,
    )
    .unwrap();
    let received = wait(|| std::fs::read(host.directory().join("rendered-received.bin")).ok());
    assert_eq!(received, bytes);
    assert!(!host.directory().join("context-1.bin").exists());
    let configuration: serde_json::Value =
        serde_json::from_slice(&std::fs::read(host.directory().join("launch.json")).unwrap())
            .unwrap();
    assert!(configuration.get("rendered_startup").is_some());
    host.stop().unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}
#[test]
#[ignore = "actual local tmux host qualification"]
fn rendered_payload_tampered_after_intent_is_refused_before_native_exec() {
    struct Tamper {
        inner: Authority,
        path: PathBuf,
    }
    impl LaunchAuthority for Tamper {
        fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
            self.inner.record_launch_intent(i)
        }
        fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
            self.inner.record_guardian(i)
        }
        fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
            self.inner.record_start_intent(i, g)
        }
        fn record_started(&mut self, o: &HostObservation) -> Result<()> {
            self.inner.record_started(o)
        }
        fn checkpoint(&mut self, stage: &str) -> Result<()> {
            if stage == "start-intent-durable" {
                std::fs::write(&self.path, b"corrupt")?;
            }
            Ok(())
        }
    }
    let mut c = Case::new("rendered", false);
    let startup = rendered_material(&mut c, b"fixture");
    let now = ownership::raw_boot_ms();
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let path = c.temp.path().join(c.id.to_string());
    let mut authority = Tamper {
        inner: c.authority,
        path: path.join("context-0.bin"),
    };
    assert!(
        SessionHost::launch_rendered(
            c.temp.path(),
            c.id,
            &startup,
            permit,
            &c.local,
            &c.adapter,
            &mut authority
        )
        .is_err()
    );
    assert!(!path.join("pane-exec.claim").exists());
    assert!(!path.join("fixture.claim").exists());
    let intent: LaunchIntent =
        serde_json::from_slice(&std::fs::read(c.temp.path().join("caller-intent.json")).unwrap())
            .unwrap();
    let mut host = SessionHost::recover(c.temp.path(), &intent).unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
}

#[test]
#[ignore = "explicit guardian reader-lease process fixture"]
fn reader_owner_child_fixture() {
    let Some(path) = std::env::var_os("AQ_READER_BINDING") else {
        return;
    };
    let path = PathBuf::from(path);
    let binding: terminal_io::TerminalBinding =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let dir = path.parent().unwrap();
    let host = SessionHost::observe_existing(dir.parent().unwrap(), &binding.guardian).unwrap();
    let io = terminal_io::TerminalHandle::recover(&host, &binding.terminal).unwrap();
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    wait_named("child observer attachment", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    std::fs::write(
        dir.join("reader-owner-ready"),
        serde_json::to_vec(observer.process()).unwrap(),
    )
    .unwrap();
    loop {
        if observer.poll().is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
#[ignore = "explicit local tmux guardian read-lease qualification"]
fn quiet_drained_reader_renews_but_undrained_reader_expires_without_stopping_harness() {
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    let end = Instant::now() + Duration::from_millis(2200);
    while Instant::now() < end {
        observer.poll().unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(ownership::pidfd_is_live(&client).unwrap());
    let input = io_input(&lease, b"flood\n");
    let id = input.operation_id;
    io.submit(
        &aq_terminal_relay_protocol::Effect::Input(input),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    // Deliberately do not read the pipe or issue drain receipts.
    wait_named("undrained reader independently severed", || {
        (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
    });
    wait_named("producer progresses after undrained cutoff", || {
        std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v >= 4 * 1024 * 1024)
    });
    assert!(matches!(
        io.receipt(id).unwrap(),
        Some(terminal_io::Receipt::Input(
            aq_session_protocol::InputReceipt {
                disposition: aq_session_protocol::InputDisposition::Applied,
                ..
            }
        ))
    ));
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    assert!(observer.poll().is_err());
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}
#[test]
#[ignore = "explicit local tmux independent owner failure qualification"]
fn dead_or_stopped_owner_and_lost_ready_or_ack_cannot_leave_a_blocking_reader() {
    for mode in ["dead", "stopped", "before-ready", "before-ack"] {
        let (_case, mut host, io, lease) = io_setup_mode("output-flood");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "reader_owner_child_fixture",
                "--test-threads=1",
            ])
            .env(
                "AQ_READER_BINDING",
                host.directory().join("terminal-binding.json"),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if mode.starts_with("before-") {
            command.env("AQ_HOST_READER_PAUSE", mode);
        }
        let mut consumer = command.spawn().unwrap();
        let process: ProcessEvidence = wait_named("owned read client created", || {
            std::fs::read(host.directory().join("reader.process.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
        });
        let client = ownership::verified_pidfd(&process).unwrap();
        if mode.starts_with("before-") {
            wait_named("private handoff pause reached", || {
                host.directory()
                    .join("reader-owner-paused")
                    .exists()
                    .then_some(())
            });
        } else {
            wait_named("reader consumer ready", || {
                host.directory()
                    .join("reader-owner-ready")
                    .exists()
                    .then_some(())
            });
            if mode == "dead" {
                consumer.kill().unwrap();
            } else {
                unsafe {
                    libc::kill(consumer.id() as i32, libc::SIGSTOP);
                }
            }
        }
        let input = io_input(&lease, b"flood\n");
        let id = input.operation_id;
        io.submit(
            &aq_terminal_relay_protocol::Effect::Input(input),
            terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
        )
        .unwrap();
        wait_named("reader severed after owner/handoff failure", || {
            (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
        });
        wait_named("harness progresses independently of owner", || {
            std::fs::read_to_string(host.directory().join("fixture-output-progress"))
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v >= 4 * 1024 * 1024)
        });
        assert!(matches!(
            io.receipt(id).unwrap(),
            Some(terminal_io::Receipt::Input(
                aq_session_protocol::InputReceipt {
                    disposition: aq_session_protocol::InputDisposition::Applied,
                    ..
                }
            ))
        ));
        assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
        assert!(terminal_output::OutputObserver::open(&io).is_err());
        if mode != "dead" {
            consumer.kill().unwrap();
        }
        consumer.wait().unwrap();
        host.stop().unwrap();
        let proof = host.wait_stopped(Duration::from_secs(3)).unwrap();
        assert!(proof.kernel_wait_echild);
    }
}

#[test]
#[ignore = "explicit actual slow-consumer read-lease qualification"]
fn slow_data_only_polls_cannot_renew_an_undrained_reader_forever() {
    let (_case, mut host, io, lease) = io_setup_mode("output-flood");
    let mut observer = terminal_output::OutputObserver::open(&io).unwrap();
    wait_named("slow observer attached", || {
        observer.poll().unwrap();
        observer.is_attached().then_some(())
    });
    let client = ownership::verified_pidfd(observer.process()).unwrap();
    io.submit(
        &aq_terminal_relay_protocol::Effect::Input(io_input(&lease, b"flood\n")),
        terminal_io::QualifiedClock::assume_bounded_skew(0).unwrap(),
    )
    .unwrap();
    wait_named("synthetic flood consumed", || {
        host.directory()
            .join("fixture-output-start")
            .exists()
            .then_some(())
    });
    let started = Instant::now();
    let mut data_polls = 0;
    let mut continued_packets = 0;
    while ownership::pidfd_is_live(&client).unwrap() {
        std::thread::sleep(Duration::from_millis(200));
        match observer.poll() {
            Ok(events) => {
                assert!(events.len() <= terminal_output::MAX_CONTROL_EVENTS);
                assert!(observer.buffered_wire_bytes() <= terminal_output::MAX_CONTROL_READ);
                data_polls += usize::from(
                    events
                        .iter()
                        .any(|e| matches!(e, terminal_output::ControlEvent::Bytes(_))),
                );
                continued_packets += usize::from(observer.buffered_wire_bytes() > 0);
            }
            Err(_) => break,
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "slow data-only reader kept renewing"
        );
    }
    assert!(data_polls > 0);
    wait_named("slow reader cutoff", || {
        (!ownership::pidfd_is_live(&client).unwrap()).then_some(())
    });
    wait_named("harness progresses after slow reader cutoff", || {
        std::fs::read_to_string(host.directory().join("fixture-output-progress"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v >= 4 * 1024 * 1024)
    });
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    eprintln!(
        "slow data-only cutoff={:?}, data_polls={data_polls}, retained_packet_polls={continued_packets}",
        started.elapsed()
    );
    host.stop().unwrap();
    host.wait_stopped(Duration::from_secs(3)).unwrap();
}

struct ObservedAdapter {
    program: PathBuf,
    arguments: Vec<String>,
    executable: aq_session_protocol::ArtifactRef,
    renderer: aq_session_protocol::ArtifactRef,
    private_source: Option<PathBuf>,
    scoped_auth: Option<ScopedCodexAuthBinding>,
}
impl HarnessAdapter for ObservedAdapter {
    fn prepare(&self, m: &LaunchManifest, _: &Path) -> Result<AdapterLaunch> {
        Ok(AdapterLaunch {
            manifest_digest: canonical_digest(m)?,
            adapter: m.harness.adapter.clone(),
            cli_version: m.harness.cli_version.as_str().into(),
            environment: m.harness.environment.clone(),
            capabilities: m.harness.required_capabilities.clone(),
            resource_qualification: None,
            command: LocalCommand {
                executable: self.program.clone(),
                arguments: {
                    let mut args = self.arguments.clone();
                    if let Some(scope) = &self.scoped_auth {
                        args.extend([
                            "--config".into(),
                            format!("openai_base_url=\"{}/v1\"", scope.origin),
                        ]);
                    }
                    args
                },
                environment: self
                    .private_source
                    .as_ref()
                    .map(|_| {
                        std::collections::BTreeMap::from([(
                            "CODEX_HOME".into(),
                            self.program
                                .parent()
                                .unwrap()
                                .join(Uuid::from_u128(2).to_string())
                                .join("codex-home")
                                .to_str()
                                .unwrap()
                                .into(),
                        )])
                    })
                    .unwrap_or_default(),
            },
        })
    }
    fn private_input(&self, _: &LaunchManifest, _: &Path) -> Result<Option<PrivateLaunchInput>> {
        self.private_source
            .as_ref()
            .map(|source| {
                let key = std::fs::read(source.join("material.bin"))?;
                let record = std::fs::read(source.join("record.json"))?;
                if let Some(scope) = &self.scoped_auth {
                    PrivateLaunchInput::codex_scoped_capability(
                        std::fs::File::open(source)?,
                        OriginalPrivateMaterial {
                            digest: aq_session_protocol::Digest::of(&key),
                            bytes: key.len() as u64,
                            record_digest: aq_session_protocol::Digest::of(&record),
                        },
                        scope.clone(),
                        scoped_capability(),
                    )
                } else {
                    PrivateLaunchInput::codex_api_key(
                        std::fs::File::open(source)?,
                        aq_session_protocol::Digest::of(&key),
                        key.len() as u64,
                        aq_session_protocol::Digest::of(&record),
                    )
                }
            })
            .transpose()
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl observed::ObservedHarnessAdapter for ObservedAdapter {
    fn observed_qualification(
        &self,
        _: &LaunchManifest,
    ) -> Result<observed::ObservedAdapterQualification> {
        Ok(observed::ObservedAdapterQualification {
            executable: self.executable.clone(),
            renderer: self.renderer.clone(),
        })
    }
}
#[test]
#[ignore = "actual local tmux closed observed gate qualification"]
fn observed_helper_stays_starting_without_exec_or_started_until_original_expiry() {
    use aq_session_protocol::{ArtifactRef, Digest, TerminalIdentity, Text};
    let mut c = Case::new("unused", false);
    let rendered = rendered_material(&mut c, b"literal startup remains sealed");
    let program = std::fs::canonicalize("/usr/bin/true").unwrap();
    let executable = ArtifactRef {
        id: Text::new("synthetic-true").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(&program).unwrap()),
    };
    let renderer = ArtifactRef {
        id: Text::new("synthetic-renderer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"test-only installed qualification"),
    };
    let pool = observed::SealedElfPool::new(4 * 1024 * 1024, 1).unwrap();
    let elf = pool.load_verified(&program, &executable).unwrap();
    let now = ownership::raw_boot_ms();
    let terminal = TerminalIdentity {
        session: c.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let startup = observed::ObservedStartup::new(
        rendered,
        terminal,
        elf,
        observed::ObservationProfile::small_elf(renderer.clone(), 80, 24).unwrap(),
        now + 1500,
    )
    .unwrap();
    let adapter = ObservedAdapter {
        arguments: vec![],
        program,
        executable,
        renderer,
        private_source: None,
        scoped_auth: None,
    };
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let launch_result = SessionHost::prepare_observed(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &adapter,
        &mut c.authority,
    );
    let mut pending = match launch_result {
        Ok(v) => v,
        Err(error) => {
            let root = c.temp.keep();
            panic!(
                "observed launch error {error:#}; retained {}",
                root.display()
            );
        }
    };
    let state = pending.observe().unwrap();
    assert_eq!(state.phase, HostPhase::Starting);
    let pane = state.pane.unwrap();
    let image = std::fs::metadata(format!("/proc/{}/exe", pane.pane.pid)).unwrap();
    use std::os::unix::fs::MetadataExt;
    let helper = std::fs::metadata(&c.local.helper_executable).unwrap();
    assert_eq!((image.dev(), image.ino()), (helper.dev(), helper.ino()));
    assert!(!c.temp.path().join("caller-started.json").exists());
    assert!(!c.adapter.dir.join("pane-exec.claim").exists());
    assert_eq!(
        std::fs::read(c.adapter.dir.join("context-0.bin")).unwrap(),
        b"literal startup remains sealed"
    );
    let stopped = pending.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    assert!(!c.temp.path().join("caller-started.json").exists());
}

impl observed::ObservedLaunchAuthority for Authority {
    fn record_baseline(&mut self, baseline: &observed::VerifiedBlankBaseline) -> Result<()> {
        durable_new(
            &self.root.join("caller-baseline.json"),
            &serde_json::to_vec(baseline)?,
        )
    }
}
fn observed_program(path: &Path) {
    use std::io::Write;
    let mut child = Command::new("/usr/bin/cc")
        .args(["-O2", "-static", "-x", "c", "-o"])
        .arg(path)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <string.h>
int main(int argc,char **argv) {
 char path[4096], out[4096], b[4096];
 const char *d=getenv("AQ_SESSION_DIRECTORY"); if(!d)return 81;
 if(snprintf(path,sizeof(path),"%s/context-0.bin",d)>=(int)sizeof(path))return 82;
 if(snprintf(out,sizeof(out),"%s/observed-received.bin",d)>=(int)sizeof(out))return 83;
 int in=open(path,O_RDONLY),to=open(out,O_WRONLY|O_CREAT|O_EXCL,0600);if(in<0||to<0)return 84;
 ssize_t n;while((n=read(in,b,sizeof(b)))>0)if(write(to,b,n)!=n)return 85;
 if(n<0||fsync(to))return 86;close(in);close(to);
 if(write(1,"observed-native-ready\n",22)!=22)return 87;
 if(argc>1 && strcmp(argv[1],"--exit")==0)return 0;
 while(read(0,b,sizeof(b))>0){} return 0;
}
"#,
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
}
#[test]
#[ignore = "actual local tmux sealed FD baseline/exec qualification"]
fn observed_blank_gate_releases_only_after_durable_baseline_and_live_fd_exec() {
    use aq_session_protocol::{ArtifactRef, Digest, TerminalIdentity, Text};
    let mut c = Case::new("unused", false);
    let program = c.temp.path().join("observed-native");
    observed_program(&program);
    let executable = ArtifactRef {
        id: Text::new("synthetic-static-elf").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(&program).unwrap()),
    };
    let renderer = ArtifactRef {
        id: Text::new("synthetic-renderer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"test-only installed qualification"),
    };
    let pool = observed::SealedElfPool::new(4 * 1024 * 1024, 1).unwrap();
    let elf = pool.load_verified(&program, &executable).unwrap();
    let rendered = rendered_material(&mut c, "literal\n雪\0startup".as_bytes());
    // Replace mutable source bytes. Successful exec must use the held sealed FD.
    std::fs::write(
        &program,
        b"invalid replacement; this path must never execute",
    )
    .unwrap();
    let terminal = TerminalIdentity {
        session: c.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let now = ownership::raw_boot_ms();
    let startup = observed::ObservedStartup::new(
        rendered,
        terminal.clone(),
        elf,
        observed::ObservationProfile::small_elf(renderer.clone(), 80, 24).unwrap(),
        now + 2500,
    )
    .unwrap();
    let adapter = ObservedAdapter {
        arguments: vec![],
        program,
        executable,
        renderer,
        private_source: None,
        scoped_auth: None,
    };
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 2500,
    };
    let pending = SessionHost::prepare_observed(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &adapter,
        &mut c.authority,
    )
    .unwrap();
    let (mut observer, baseline) = pending.open_baseline_observer().unwrap();
    assert_eq!(baseline.terminal(), &terminal);
    assert_eq!(baseline.dimensions(), (80, 24));
    assert_eq!(pending.observe().unwrap().phase, HostPhase::Starting);
    assert!(!c.adapter.dir.join("observed-received.bin").exists());
    assert!(!c.temp.path().join("caller-started.json").exists());
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    assert!(c.temp.path().join("caller-baseline.json").exists());
    let mut output = vec![];
    let mut host = wait_named("positive exact live sealed exec", || {
        for event in observer.poll().unwrap() {
            if let terminal_output::ControlEvent::Bytes(bytes) = event {
                output.extend(bytes);
            }
        }
        released.poll_started(&mut c.authority).unwrap()
    });
    wait_named("literal payload reached sealed native image", || {
        for event in observer.poll().unwrap() {
            if let terminal_output::ControlEvent::Bytes(bytes) = event {
                output.extend(bytes);
            }
        }
        std::fs::read(c.adapter.dir.join("observed-received.bin")).ok()
    });
    assert_eq!(
        std::fs::read(c.adapter.dir.join("observed-received.bin")).unwrap(),
        "literal\n雪\0startup".as_bytes()
    );
    assert!(c.temp.path().join("caller-started.json").exists());
    let late = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 2,
        issued_boot_ms: ownership::raw_boot_ms(),
        expires_boot_ms: now + 2501,
    };
    assert!(
        host.renew(&late).is_err(),
        "observed original hard deadline cannot renew"
    );
    host.stop().unwrap();
    assert!(
        host.wait_stopped(Duration::from_secs(3))
            .unwrap()
            .kernel_wait_echild
    );
}

fn observed_closed_case() -> (Case, observed::PendingObservedSession) {
    observed_closed_case_mode(false)
}
fn observed_closed_case_mode(short: bool) -> (Case, observed::PendingObservedSession) {
    use aq_session_protocol::{ArtifactRef, Digest, TerminalIdentity, Text};
    let mut c = Case::new("unused", false);
    let program = c.temp.path().join("observed-native");
    observed_program(&program);
    let executable = ArtifactRef {
        id: Text::new("synthetic-static-elf").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(&program).unwrap()),
    };
    let renderer = ArtifactRef {
        id: Text::new("synthetic-renderer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"test-only installed qualification"),
    };
    let pool = observed::SealedElfPool::new(4 * 1024 * 1024, 1).unwrap();
    let elf = pool.load_verified(&program, &executable).unwrap();
    let rendered = rendered_material(&mut c, b"fixed literal startup");
    let terminal = TerminalIdentity {
        session: c.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let now = ownership::raw_boot_ms();
    let startup = observed::ObservedStartup::new(
        rendered,
        terminal,
        elf,
        observed::ObservationProfile::small_elf(renderer.clone(), 80, 24).unwrap(),
        now + 1500,
    )
    .unwrap();
    let adapter = ObservedAdapter {
        arguments: if short { vec!["--exit".into()] } else { vec![] },
        program,
        executable,
        renderer,
        private_source: None,
        scoped_auth: None,
    };
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 1500,
    };
    let result = SessionHost::prepare_observed(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &adapter,
        &mut c.authority,
    );
    let pending = match result {
        Ok(v) => v,
        Err(error) => {
            let root = c.temp.keep();
            panic!(
                "observed preparation {error:#}; retained {}",
                root.display()
            );
        }
    };
    (c, pending)
}
struct BaselineAuthority<'a> {
    base: &'a mut Authority,
    refuse: bool,
    delay_until: Option<u64>,
}
impl LaunchAuthority for BaselineAuthority<'_> {
    fn record_launch_intent(&mut self, v: &LaunchIntent) -> Result<()> {
        self.base.record_launch_intent(v)
    }
    fn record_guardian(&mut self, v: &GuardianIdentity) -> Result<()> {
        self.base.record_guardian(v)
    }
    fn record_start_intent(&mut self, i: &LaunchIntent, g: &GuardianIdentity) -> Result<()> {
        self.base.record_start_intent(i, g)
    }
    fn record_started(&mut self, v: &HostObservation) -> Result<()> {
        self.base.record_started(v)
    }
}
impl observed::ObservedLaunchAuthority for BaselineAuthority<'_> {
    fn record_baseline(&mut self, v: &observed::VerifiedBlankBaseline) -> Result<()> {
        if self.refuse {
            anyhow::bail!("synthetic durability refusal");
        }
        if let Some(until) = self.delay_until {
            while ownership::raw_boot_ms() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        self.base.record_baseline(v)
    }
}
#[test]
#[ignore = "actual local tmux baseline refusal/reader loss/expiry qualification"]
fn observed_baseline_durability_reader_and_expiry_fail_closed_without_exec() {
    for mode in [
        "durability",
        "reader-drop",
        "reader-stop",
        "helper-death",
        "server-death",
        "expiry",
    ] {
        let (mut c, pending) = observed_closed_case();
        let initial = pending.observe().unwrap();
        let (observer, baseline) = pending.open_baseline_observer().unwrap();
        if mode == "reader-drop" {
            drop(observer);
        } else {
            if mode == "reader-stop" {
                let pidfd = ownership::verified_pidfd(observer.process()).unwrap();
                ownership::signal(&pidfd, libc::SIGSTOP).unwrap();
                wait_named("reader enters kernel stopped state", || {
                    std::fs::read_to_string(format!("/proc/{}/stat", observer.process().pid))
                        .ok()
                        .filter(|s| {
                            s.rsplit_once(") ")
                                .is_some_and(|(_, f)| f.starts_with('T') || f.starts_with('t'))
                        })
                });
            }
            if mode == "server-death" {
                terminate(&initial.pane.as_ref().unwrap().server);
            }
            if mode == "helper-death" {
                terminate(&initial.pane.as_ref().unwrap().pane);
            }
            // Keep the observer alive through release for the other failure modes.
            let authority = BaselineAuthority {
                base: &mut c.authority,
                refuse: mode == "durability",
                delay_until: (mode == "expiry").then_some(initial.permit_deadline_boot_ms),
            };
            let mut authority = authority;
            assert!(
                pending
                    .release_after_baseline(&baseline, &mut authority)
                    .is_err(),
                "{mode}"
            );
            let mut recovered =
                SessionHost::recover(c.temp.path(), &initial.guardian.intent).unwrap();
            assert!(
                recovered
                    .wait_stopped(Duration::from_secs(3))
                    .unwrap()
                    .kernel_wait_echild,
                "{mode}"
            );
            assert!(!c.adapter.dir.join("pane-exec.claim").exists(), "{mode}");
            assert!(
                !c.temp.path().join("caller-started.json").exists(),
                "{mode}"
            );
            continue;
        }
        assert!(
            pending
                .release_after_baseline(&baseline, &mut c.authority)
                .is_err(),
            "{mode}"
        );
        let mut recovered = SessionHost::recover(c.temp.path(), &initial.guardian.intent).unwrap();
        assert!(
            recovered
                .wait_stopped(Duration::from_secs(3))
                .unwrap()
                .kernel_wait_echild,
            "{mode}"
        );
        assert!(!c.adapter.dir.join("pane-exec.claim").exists(), "{mode}");
        assert!(
            !c.temp.path().join("caller-started.json").exists(),
            "{mode}"
        );
    }
}

#[test]
#[ignore = "actual local tmux missed exec evidence fault qualification"]
fn observed_short_exec_without_live_kernel_evidence_never_records_started() {
    if std::env::var_os("AQ_OBS_SHORT_FIXTURE").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "observed_short_exec_without_live_kernel_evidence_never_records_started",
                "--ignored",
                "--nocapture",
            ])
            .env("AQ_OBS_SHORT_FIXTURE", "1")
            .env("AQ_SESSION_HOST_FAULT_STAGE", "observed-exec-permit")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let (mut c, pending) = observed_closed_case_mode(true);
    let initial = pending.observe().unwrap();
    let (mut observer, baseline) = pending.open_baseline_observer().unwrap();
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    wait_named("guardian test fault after fixed exec permit", || {
        c.adapter
            .dir
            .join("guardian-fault.json")
            .exists()
            .then_some(())
    });
    wait_named(
        "short native exits before positive guardian observation",
        || {
            let _ = observer.poll();
            (c.adapter.dir.join("observed-received.bin").exists()
                && ownership::process_evidence(initial.pane.as_ref().unwrap().pane.pid).is_err())
            .then_some(())
        },
    );
    assert_eq!(released.observe().unwrap().phase, HostPhase::Starting);
    assert!(!c.temp.path().join("caller-started.json").exists());
    ownership::signal(
        &ownership::verified_pidfd(&initial.guardian.process).unwrap(),
        libc::SIGCONT,
    )
    .unwrap();
    wait_named(
        "missed exec evidence remains uncertain without Started",
        || {
            let _ = observer.poll();
            released
                .poll_started(&mut c.authority)
                .is_err()
                .then_some(())
        },
    );
    let mut recovered = SessionHost::recover(c.temp.path(), &initial.guardian.intent).unwrap();
    assert!(
        recovered
            .wait_stopped(Duration::from_secs(3))
            .unwrap()
            .kernel_wait_echild
    );
    assert!(c.adapter.dir.join("pane-exec.claim").exists());
    assert!(!c.temp.path().join("caller-started.json").exists());
}

#[test]
#[ignore = "actual native-exec profile: requires local cc/static ELF and tmux"]
fn native_exec_profile_delivers_exact_sealed_stdin_under_actual_guardian() {
    native_private_input_case(false, false);
}
#[test]
#[ignore = "actual native-exec original material preexec refusal: requires cc/static ELF and tmux"]
fn native_exec_refuses_original_record_changed_after_private_publication() {
    native_private_input_case(true, false);
}
#[test]
#[ignore = "actual scoped private auth/ECHILD: static ELF and tmux only, no agent/provider"]
fn native_exec_scoped_capability_is_published_under_intent_and_cleaned_after_echild() {
    native_private_input_case(false, true);
}
#[test]
#[ignore = "actual scoped original-source refusal: static ELF and tmux only, no agent/provider"]
fn native_exec_scoped_auth_refuses_changed_original_record_at_exec() {
    native_private_input_case(true, true);
}
fn scoped_capability() -> zeroize::Zeroizing<Vec<u8>> {
    zeroize::Zeroizing::new(format!("aq-responses-{}", "a".repeat(64)).into_bytes())
}
fn native_private_input_case(tamper_record: bool, scoped: bool) {
    use aq_session_protocol::{ArtifactRef, Digest, TerminalIdentity, Text};
    use std::io::Write;
    let mut c = Case::new("unused", false);
    let payload = b"literal startup > argument delivery\nwithout an appended prompt\n".repeat(128);
    c.manifest.context.startup_byte_budget = 1_048_576;
    c.manifest.terminal.input_enabled = false;
    let rendered = rendered_material(&mut c, &payload);
    use std::os::unix::fs::PermissionsExt;
    let source = c.temp.path().join("retained-provider");
    std::fs::create_dir(&source).unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, bytes) in [
        ("material.bin", b"sk-private-offline-only".as_slice()),
        ("record.json", b"{\"schema_version\":1}".as_slice()),
    ] {
        let path = source.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let program = c.temp.path().join("native-stdin-peer");
    let mut cc = Command::new("/usr/bin/cc")
        .args(["-O2", "-static", "-x", "c", "-o"])
        .arg(&program)
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    cc.stdin.take().unwrap().write_all(br#"
#define _GNU_SOURCE
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
const unsigned char image_padding[5*1024*1024]={1};
int main(int argc,char **argv) {
 char path[4096], auth[128]={0}; const char *home=getenv("CODEX_HOME");
 if(!home || snprintf(path,sizeof(path),"%s/auth.json",home)<0)return 83;
 int fd=open(path,O_RDONLY|O_NOFOLLOW); if(fd<0)return 84;
 ssize_t got=read(fd,auth,sizeof(auth));close(fd);
 const char *expected=argc==3 ? "{\"OPENAI_API_KEY\":\"aq-responses-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}" : "{\"OPENAI_API_KEY\":\"sk-private-offline-only\"}";
 if(got!=(ssize_t)strlen(expected) || memcmp(auth,expected,strlen(expected)))return 85;
 if(argc==3 && getenv("AQ_SESSION_DIRECTORY"))return 86;
 puts("AUTH:exact-private-file");
 if((fcntl(0,F_GET_SEALS)&(F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL))!=(F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL))return 82;
 unsigned char b[8192]; uint64_t hash=14695981039346656037ULL, n=0; ssize_t count;
 while((count=read(0,b,sizeof(b)))>0){for(ssize_t i=0;i<count;i++){hash^=b[i];hash*=1099511628211ULL;}n+=count;}
 if(count<0 || (argc!=1 && argc!=3) || image_padding[argc]!=0)return 80;
 printf("STDIN:%llu:%016llx\n",(unsigned long long)n,(unsigned long long)hash);fflush(stdout);
 for(;;)pause();
}
"#).unwrap();
    assert!(cc.wait().unwrap().success());
    assert!(std::fs::metadata(&program).unwrap().len() > observed::MAX_ELF_BYTES);
    let executable = ArtifactRef {
        id: Text::new("offline.native-stdin-peer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(&program).unwrap()),
    };
    let renderer = ArtifactRef {
        id: Text::new("offline.literal-observer").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"not a screen renderer qualification"),
    };
    let pool = observed::NativeExecPool::new(8 * 1024 * 1024, 1).unwrap();
    let image = pool.load_verified(&program, &executable).unwrap();
    assert_eq!(
        pool.charged_bytes().unwrap(),
        std::fs::metadata(&program).unwrap().len()
    );
    std::fs::write(
        &program,
        b"mutable installation replacement; must never execute",
    )
    .unwrap();
    let now = ownership::raw_boot_ms();
    let terminal = TerminalIdentity {
        session: c.manifest.bindings.session.clone(),
        terminal_id: Uuid::new_v4(),
        epoch: 1,
    };
    let startup = observed::ObservedStartup::native_exec(
        rendered,
        terminal,
        image,
        observed::NativeExecObservationProfile::new(renderer.clone(), 80, 24).unwrap(),
        now + 8000,
    )
    .unwrap();
    let adapter = ObservedAdapter {
        arguments: vec![],
        program,
        executable,
        renderer,
        private_source: Some(source.clone()),
        scoped_auth: scoped.then(|| ScopedCodexAuthBinding {
            launch_id: c.id,
            origin: "http://127.0.0.1:12345".into(),
            deadline_boot_ms: now + 8000,
            binding: aq_session_protocol::LaunchBinding {
                schema_version: 1,
                session: c.manifest.bindings.session.clone(),
                offer_id: Uuid::new_v4(),
                attempt_id: c.manifest.bindings.attempt_id,
                assignment_generation: c.manifest.bindings.assignment_generation,
                execution_spec_digest: Digest::of(b"static peer only"),
                launch_manifest_digest: canonical_digest(&c.manifest).unwrap(),
                graph_access_profile_digest: c
                    .manifest
                    .bindings
                    .graph_access_profile
                    .digest
                    .clone(),
            },
        }),
    };
    let permit = ExecutionPermit {
        session: c.manifest.bindings.session.clone(),
        manifest_digest: canonical_digest(&c.manifest).unwrap(),
        sequence: 1,
        issued_boot_ms: now,
        expires_boot_ms: now + 8000,
    };
    let pending = SessionHost::prepare_observed(
        c.temp.path(),
        c.id,
        &startup,
        permit,
        &c.local,
        &adapter,
        &mut c.authority,
    )
    .unwrap();
    assert!(c.temp.path().join("caller-intent.json").exists());
    assert!(c.temp.path().join("auth-order.checked").exists());
    let auth = c.adapter.dir.join("codex-home/auth.json");
    let expected_auth = if scoped {
        format!(
            "{{\"OPENAI_API_KEY\":\"{}\"}}",
            std::str::from_utf8(&scoped_capability()).unwrap()
        )
        .into_bytes()
    } else {
        b"{\"OPENAI_API_KEY\":\"sk-private-offline-only\"}".to_vec()
    };
    assert_eq!(std::fs::read(&auth).unwrap(), expected_auth);
    if scoped {
        assert!(!String::from_utf8_lossy(&expected_auth).contains("sk-private"));
        assert!(
            !std::fs::read_to_string(c.adapter.dir.join("launch.json"))
                .unwrap()
                .contains(std::str::from_utf8(&scoped_capability()).unwrap())
        );
    }
    assert!(
        !std::fs::read_to_string(c.adapter.dir.join("launch.json"))
            .unwrap()
            .contains("sk-private")
    );
    assert_eq!(pending.observe().unwrap().phase, HostPhase::Starting);
    let intent = pending.observe().unwrap().guardian.intent;
    let (mut output, baseline) = pending.open_baseline_observer().unwrap();
    if tamper_record {
        std::fs::write(
            source.join("record.json"),
            b"changed original record after auth publication",
        )
        .unwrap();
    }
    let mut released = pending
        .release_after_baseline(&baseline, &mut c.authority)
        .unwrap();
    if tamper_record {
        let observed = wait_named("changed original material stops before exec", || {
            let observation = released.observe().unwrap();
            assert_ne!(observation.phase, HostPhase::Running);
            (observation.phase == HostPhase::Stopped).then_some(observation)
        });
        assert!(observed.kernel_wait_echild);
        assert!(!c.temp.path().join("caller-started.json").exists());
        assert!(auth.exists());
        let mut recovered = SessionHost::recover(c.temp.path(), &intent).unwrap();
        let proof = recovered.stopped_proof().unwrap();
        assert!(
            recovered
                .cleanup_private_inputs(&proof)
                .unwrap()
                .home_removed
        );
        return;
    }
    let mut handle = wait_named("native sealed exec", || {
        released.poll_started(&mut c.authority).unwrap()
    });
    let hash = payload.iter().fold(14695981039346656037_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(1099511628211)
    });
    let expected = format!("STDIN:{}:{hash:016x}", payload.len());
    let mut literal = Vec::new();
    wait_named("native exact stdin output", || {
        for event in output.poll().unwrap() {
            if let terminal_output::ControlEvent::Bytes(bytes) = event {
                literal.extend_from_slice(&bytes);
            }
        }
        String::from_utf8_lossy(&literal)
            .contains(&expected)
            .then_some(())
    });
    assert!(String::from_utf8_lossy(&literal).contains("AUTH:exact-private-file"));
    assert!(handle.stopped_proof().is_err());
    assert_eq!(handle.observe().unwrap().phase, HostPhase::Running);
    handle.stop().unwrap();
    let stopped = handle.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    let generated = auth.parent().unwrap().join("runtime-file");
    std::fs::write(&generated, b"keep unknown generated files").unwrap();
    let proof = handle.stopped_proof().unwrap();
    let cleaned = handle.cleanup_private_inputs(&proof).unwrap();
    assert!(cleaned.auth_removed && !cleaned.home_removed);
    assert_eq!(
        std::fs::read(&generated).unwrap(),
        b"keep unknown generated files"
    );
    std::fs::remove_file(generated).unwrap();
    assert!(!auth.exists());
    assert_eq!(
        std::fs::read(source.join("material.bin")).unwrap(),
        b"sk-private-offline-only"
    );
    assert!(handle.cleanup_private_inputs(&proof).unwrap().home_removed);
}
