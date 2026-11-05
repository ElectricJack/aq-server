//! Explicit independent physical image/gate qualification; never invokes Codex.
use anyhow::Result;
use aq_session_host::{observed::*, *};
use aq_session_protocol::{
    ArtifactRef, Digest, LaunchManifest, TerminalIdentity, Text, canonical_digest,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;
struct Adapter {
    program: PathBuf,
    home: PathBuf,
    executable: ArtifactRef,
    renderer: ArtifactRef,
}
impl HarnessAdapter for Adapter {
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
                arguments: vec![],
                environment: BTreeMap::from([(
                    "CODEX_HOME".into(),
                    self.home.to_str().unwrap().into(),
                )]),
            },
        })
    }
    fn readiness(&self, _: &HostObservation) -> Result<HarnessReadiness> {
        Ok(HarnessReadiness::Unknown)
    }
}
impl ObservedHarnessAdapter for Adapter {
    fn observed_qualification(&self, _: &LaunchManifest) -> Result<ObservedAdapterQualification> {
        Ok(ObservedAdapterQualification {
            executable: self.executable.clone(),
            renderer: self.renderer.clone(),
        })
    }
}
struct Authority {
    root: PathBuf,
}
impl LaunchAuthority for Authority {
    fn record_launch_intent(&mut self, i: &LaunchIntent) -> Result<()> {
        durable_new(&self.root.join("intent.json"), &serde_json::to_vec(i)?)
    }
    fn record_guardian(&mut self, i: &GuardianIdentity) -> Result<()> {
        durable_new(&self.root.join("guardian.json"), &serde_json::to_vec(i)?)
    }
    fn record_start_intent(&mut self, _: &LaunchIntent, _: &GuardianIdentity) -> Result<()> {
        durable_new(&self.root.join("start-intent"), b"exact original authority")
    }
    fn record_started(&mut self, o: &HostObservation) -> Result<()> {
        durable_new(&self.root.join("started.json"), &serde_json::to_vec(o)?)
    }
}
impl ObservedLaunchAuthority for Authority {
    fn record_baseline(&mut self, b: &VerifiedBlankBaseline) -> Result<()> {
        durable_new(&self.root.join("baseline.json"), &serde_json::to_vec(b)?)
    }
}
fn reference(id: &str, bytes: &[u8]) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(bytes),
    }
}
fn pin(name: &str, path: &str) -> ArtifactRef {
    let digest = std::env::var(name).expect("explicit independently selected physical fixture pin");
    ArtifactRef {
        digest: serde_json::from_value(serde_json::json!(digest)).unwrap(),
        ..reference(path, b"unused")
    }
}
#[test]
#[ignore = "requires confined wrapper plus prepared named-readonly-peer.c static image; never executes Codex"]
fn named_retained_fd_exec_has_stable_self_helper_exact_stdin_and_physical_echild() {
    assert_eq!(
        std::env::var("AQ_NATIVE_FIXTURE_CONFINED").as_deref(),
        Ok("v1")
    );
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("host");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut manifest: LaunchManifest =
        serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    manifest.terminal.input_enabled = false;
    let descriptor = reference(
        "aq.startup-delivery/named-kernel-fixture",
        b"trusted static fixture descriptor only",
    );
    let descriptor = ArtifactRef {
        id: Text::new(format!(
            "aq.startup-delivery/{}",
            descriptor.digest.as_str()
        ))
        .unwrap(),
        ..descriptor
    };
    manifest.modules.push(descriptor.clone());
    let payload = b"named readonly sealed stdin\n";
    let rendered = RenderedStartup::new(
        manifest.clone(),
        descriptor,
        reference("named-kernel-payload", payload),
        payload.to_vec(),
    )
    .unwrap();
    let environment = reference(
        "named-kernel-environment",
        b"independently confined test only; no production or secret authority",
    );
    let pool = NamedReadonlyExecPool::new(
        64 * 1024 * 1024,
        1,
        NamedReadonlyEnvironment::inspect(environment).unwrap(),
    )
    .unwrap();
    let program = PathBuf::from("/opt/aq-qualification/codex");
    let executable = pin("AQ_NATIVE_CODEX_SHA256", "named-static-peer");
    let image = pool.load_verified(&program, &executable).unwrap();
    let _helper = pool
        .load_auxiliary_verified(
            Path::new("/opt/aq-qualification/aq-session-host"),
            &pin("AQ_NATIVE_HOST_SHA256", "source-built-host"),
        )
        .unwrap();
    let _tmux = pool
        .load_auxiliary_verified(
            Path::new("/opt/aq-qualification/tmux"),
            &pin("AQ_NATIVE_TMUX_SHA256", "tmux34"),
        )
        .unwrap();
    let renderer = reference(
        "named-literal-observer",
        b"literal event observer only; no Snapshot qualification",
    );
    let id = Uuid::new_v4();
    let adapter = Adapter {
        program,
        home: root.join(id.to_string()).join("peer-home"),
        executable,
        renderer: renderer.clone(),
    };
    let now = ownership::raw_boot_ms();
    let until = now + 8000;
    let startup = ObservedStartup::named_readonly_exec(
        rendered,
        TerminalIdentity {
            session: manifest.bindings.session.clone(),
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        },
        image,
        NamedReadonlyObservationProfile::new(renderer, 80, 24).unwrap(),
        until,
    )
    .unwrap();
    let local = LocalEnvironment {
        resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
        workspace,
        helper_executable: "/opt/aq-qualification/aq-session-host".into(),
        tmux_executable: "/opt/aq-qualification/tmux".into(),
        tmux_version: "tmux 3.4".into(),
        credential_profiles: manifest
            .credentials
            .iter()
            .map(|c| c.credential_profile.clone())
            .collect(),
        context_material: vec![],
    };
    let mut authority = Authority { root: root.clone() };
    let pending = SessionHost::prepare_observed(
        &root,
        id,
        &startup,
        ExecutionPermit {
            session: manifest.bindings.session.clone(),
            manifest_digest: canonical_digest(&manifest).unwrap(),
            sequence: 1,
            issued_boot_ms: now,
            expires_boot_ms: until,
        },
        &local,
        &adapter,
        &mut authority,
    )
    .unwrap();
    let (mut observer, baseline) = pending.open_baseline_observer().unwrap();
    let mut released = pending
        .release_after_baseline(&baseline, &mut authority)
        .unwrap();
    let mut bytes = vec![];
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut host = None;
    while host.is_none() || !String::from_utf8_lossy(&bytes).contains("NAMED-READY:physical-only") {
        for event in observer.poll().unwrap() {
            if let terminal_output::ControlEvent::Bytes(b) = event {
                bytes.extend(b);
            }
        }
        if host.is_none() {
            host = released.poll_started(&mut authority).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "static named physical proof timed out: {}",
            String::from_utf8_lossy(&bytes)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        text.contains("NAMED-STDIN:exact-sealed")
            && text.contains("NAMED-EXE:/opt/aq-qualification/codex")
            && text.contains("NAMED-HELPER:same-inode-canonical")
    );
    let mut host = host.unwrap();
    assert_eq!(host.observe().unwrap().phase, HostPhase::Running);
    assert!(root.join("started.json").exists() && root.join("baseline.json").exists());
    host.stop().unwrap();
    let stopped = host.wait_stopped(Duration::from_secs(3)).unwrap();
    assert_eq!(stopped.phase, HostPhase::Stopped);
    assert!(stopped.kernel_wait_echild);
    assert!(
        stopped.reaped_count >= 2,
        "guardian must reap its owned pane/server; peer already waited for its helper"
    );
}
