use std::sync::atomic::{AtomicBool, Ordering};
static IN_GUARDIAN: AtomicBool = AtomicBool::new(false);
pub(crate) fn in_guardian() -> bool {
    IN_GUARDIAN.load(Ordering::Relaxed)
}
use super::*;
use ownership::{
    EmergencyReaper, process_evidence, raw_boot_ms, reap_children, signal_children_bounded,
    verified_pidfd,
};
#[cfg(feature = "test-support")]
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
pub(crate) fn machine() -> Result<(String, String)> {
    Ok((
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .into(),
        std::fs::read_link("/proc/self/ns/pid")?
            .to_string_lossy()
            .into_owned(),
    ))
}
pub(crate) fn guardian_live(id: &GuardianIdentity) -> bool {
    machine().is_ok_and(|(boot, ns)| boot == id.machine_boot_id && ns == id.pid_namespace)
        && verified_pidfd(&id.process)
            .is_ok_and(|fd| ownership::pidfd_is_live(&fd).unwrap_or(false))
}
fn save(dir: &Path, state: &HostObservation) -> Result<()> {
    durable_replace(&dir.join("state.json"), &serde_json::to_vec(state)?)
}
fn request(dir: &Path, name: &str, id: &GuardianIdentity) -> Result<Option<ExecutionPermit>> {
    let path = dir.join(name);
    if !path.exists() {
        return Ok(None);
    }
    let request: BoundRequest = read_json(&path)?;
    ensure!(
        request.guardian == *id,
        "request guardian identity mismatch"
    );
    Ok(Some(request.permit))
}
fn parent(pid: u32) -> Result<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("process parent stat")?
        .1
        .split_whitespace()
        .nth(1)
        .context("process parent")?
        .parse()?)
}
fn descendant(mut pid: u32, ancestor: u32) -> Result<bool> {
    for _ in 0..128 {
        if pid == ancestor {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        pid = parent(pid)?;
    }
    Ok(false)
}
fn pane_status(config: &SealedLaunch, dir: &Path) -> Result<(u32, u32, bool, String)> {
    let bytes = tmux(
        config,
        dir,
        &[
            "display-message",
            "-p",
            "-t",
            "aq:0.0",
            "#{pid} #{pane_pid} #{pane_dead} #{pane_id}",
        ],
    )?;
    let text = String::from_utf8(bytes)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 4, "tmux identity response");
    Ok((
        fields[0].parse()?,
        fields[1].parse()?,
        fields[2] == "1",
        fields[3].into(),
    ))
}
pub(crate) fn run(dir: &Path) -> Result<()> {
    ownership::become_subreaper()?;
    IN_GUARDIAN.store(true, Ordering::Relaxed);
    let directory_guard = terminal_io::DirectoryGuard::open(dir)?;
    directory_guard.anchor()?;
    let config: SealedLaunch = read_json(&dir.join("launch.json"))?;
    config.manifest.validate()?;
    config.command.validate()?;
    if let Some(spec) = &config.observed {
        spec.validate(&config)?;
    }
    let intent = config.intent()?;
    config.validate_permit_at(
        &config.permit,
        &intent.session,
        &intent.manifest_digest,
        raw_boot_ms(),
    )?;
    durable_new(
        &dir.join("guardian.claim"),
        b"one guardian per immutable launch\n",
    )?;
    let (boot, ns) = machine()?;
    let identity = GuardianIdentity {
        intent,
        nonce: Uuid::new_v4(),
        process: process_evidence(std::process::id())?,
        machine_boot_id: boot,
        pid_namespace: ns,
    };
    let mut state = HostObservation {
        guardian: identity.clone(),
        phase: HostPhase::Waiting,
        pane: None,
        permit_sequence: config.permit.sequence,
        permit_deadline_boot_ms: config.permit.expires_boot_ms,
        observed_boot_ms: raw_boot_ms(),
        reaped_count: 0,
        kernel_wait_echild: false,
        root_status: None,
        stop_reason: None,
    };
    let mut observed_owner = if config.observed.is_some() {
        Some(observed::gate::Owner::new(dir)?)
    } else {
        None
    };
    save(dir, &state)?;
    durable_new(&dir.join("identity.json"), &serde_json::to_vec(&identity)?)?;
    let _reaper = EmergencyReaper;
    let mut reader_owner = reader_lease::Owner::new(dir)?;
    let mut viewer_owner = reader_lease::Owner::new_viewer(dir)?;
    let mut viewer_revision_highwater = 0;
    let mut root: Option<Child> = None;
    let mut root_pid = None;
    let mut stop_started = None;
    let mut pane_fd = None;
    let mut terminal_failed = false;
    let mut terminal_revision_highwater = 0;
    let mut last_time = raw_boot_ms();
    let wall_deadline = config.wall_deadline()?;
    loop {
        let now = raw_boot_ms();
        if now < last_time {
            state.phase = HostPhase::Stopping;
            state.stop_reason = Some("clock regressed".into());
        }
        last_time = now;
        state.observed_boot_ms = now;
        // Stop and expiry are evaluated before renewal or any new process effect.
        if dir.join("stop.request").exists() {
            match request(dir, "stop.request", &identity) {
                Ok(Some(_)) => {
                    state.phase = HostPhase::Stopping;
                    state.stop_reason = Some("cancelled".into());
                }
                _ => {
                    state.phase = HostPhase::Stopping;
                    state.stop_reason = Some("invalid stop directive".into());
                }
            }
        }
        if now >= state.permit_deadline_boot_ms || now >= wall_deadline {
            state.phase = HostPhase::Stopping;
            state.stop_reason = Some("execution permit expired".into());
        }
        let reaped = reap_children()?;
        state.reaped_count += reaped.processes.len() as u64;
        for (pid, status) in &reaped.processes {
            if Some(*pid) == root_pid {
                state.root_status = Some(*status);
            }
        }
        if reaped.kernel_echild && (state.phase == HostPhase::Stopping || root_pid.is_some()) {
            state.phase = HostPhase::Stopped;
            state.kernel_wait_echild = true;
            save(dir, &state)?;
            return Ok(());
        }
        if state.root_status.is_some() {
            state.phase = HostPhase::Stopping;
            state.stop_reason.get_or_insert("tmux server exited".into());
        }
        if state.phase == HostPhase::Stopping {
            let started = *stop_started.get_or_insert(now);
            let grace = config.manifest.resources().stop_grace_ms.min(1000);
            signal_children_bounded(if now.saturating_sub(started) < grace {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            })?;
            save(dir, &state)?;
            if now.saturating_sub(started) >= 5000 {
                state.phase = HostPhase::StopUncertain;
                save(dir, &state)?;
                return Ok(());
            }
        } else {
            let startup = (|| -> Result<()> {
                if state.phase == HostPhase::Waiting && dir.join("start.request").exists() {
                    let permit =
                        request(dir, "start.request", &identity)?.context("start request")?;
                    ensure!(permit == config.permit, "changed initial start permit");
                    config.validate_permit_at(
                        &permit,
                        &identity.intent.session,
                        &identity.intent.manifest_digest,
                        raw_boot_ms(),
                    )?;
                    state.phase = HostPhase::Starting;
                    save(dir, &state)?;
                    ensure!(!dir.join("tmux.sock").exists(), "tmux socket collision");
                    fault_point(dir, "before-tmux-spawn")?;
                    ensure!(
                        raw_boot_ms() < state.permit_deadline_boot_ms
                            && raw_boot_ms() < wall_deadline
                            && !dir.join("stop.request").exists(),
                        "authority expired/cancelled before tmux spawn"
                    );
                    let child = Command::new(&config.tmux)
                        .args(["-D", "-u", "-S"])
                        .arg(dir.join("tmux.sock"))
                        .args(["-f", "/dev/null"])
                        .env_clear()
                        .env("PATH", "/usr/bin:/bin")
                        .env("TERM", "xterm-256color")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()?;
                    root_pid = Some(child.id());
                    fault_point(dir, "after-tmux-spawn")?;
                    let server = process_evidence(child.id())?;
                    root = Some(child);
                    durable_new(&dir.join("server.json"), &serde_json::to_vec(&server)?)?;
                }
                if state.phase == HostPhase::Starting
                    && state.pane.is_none()
                    && dir.join("tmux.sock").exists()
                {
                    fault_point(dir, "socket-created")?;
                    ensure!(
                        raw_boot_ms() < state.permit_deadline_boot_ms
                            && raw_boot_ms() < wall_deadline
                            && !dir.join("stop.request").exists(),
                        "authority expired/cancelled before pane launch"
                    );
                    tmux(&config, dir, &["set-option", "-g", "remain-on-exit", "on"])?;
                    let helper = config.helper.to_str().context("helper UTF-8")?;
                    let launch_dir = dir.to_str().context("launch directory UTF-8")?;
                    let workspace = config.workspace.to_str().context("workspace UTF-8")?;
                    ensure!(
                        raw_boot_ms() < state.permit_deadline_boot_ms
                            && raw_boot_ms() < wall_deadline
                            && !dir.join("stop.request").exists(),
                        "authority expired/cancelled immediately before pane creation"
                    );
                    if config.observed.is_some() {
                        tmux(&config, dir, &["set-option", "-g", "status", "off"])?;
                    }
                    let columns = config
                        .observed
                        .as_ref()
                        .map_or(80, |s| s.columns)
                        .to_string();
                    let rows = config.observed.as_ref().map_or(24, |s| s.rows).to_string();
                    let mut arguments = vec!["new-session", "-d", "-s", "aq", "-c", workspace];
                    if config.observed.is_some() {
                        arguments.extend(["-x", &columns, "-y", &rows]);
                    }
                    arguments.extend(["--", helper, "pane", launch_dir]);
                    tmux(&config, dir, &arguments)?;
                    fault_point(dir, "pane-created")?;
                    let (server_pid, pane_pid, dead, pane_id) = pane_status(&config, dir)?;
                    ensure!(
                        Some(server_pid) == root_pid && !dead,
                        "tmux/pane identity mismatch"
                    );
                    ensure!(
                        parent(server_pid)? == std::process::id()
                            && descendant(pane_pid, server_pid)?,
                        "tmux escaped guardian ancestry"
                    );
                    let pane = PaneIdentity {
                        server: process_evidence(server_pid)?,
                        pane: process_evidence(pane_pid)?,
                        pane_id,
                        session_name: "aq".into(),
                    };
                    pane_fd = Some(verified_pidfd(&pane.pane)?);
                    state.pane = Some(pane);
                    if config.observed.is_none() {
                        state.phase = HostPhase::Running;
                    }
                    save(dir, &state)?;
                }
                Ok(())
            })();
            if let Err(error) = startup {
                state.phase = HostPhase::Stopping;
                state.stop_reason = Some(format!("startup failed: {error}"));
                save(dir, &state)?;
            }
            if state.phase == HostPhase::Running {
                let running = (|| -> Result<()> {
                    if let Some(permit) = request(dir, "renew.request", &identity)?
                        && permit.sequence > state.permit_sequence
                    {
                        config.validate_permit_at(
                            &permit,
                            &identity.intent.session,
                            &identity.intent.manifest_digest,
                            raw_boot_ms(),
                        )?;
                        ensure!(
                            raw_boot_ms() < state.permit_deadline_boot_ms,
                            "late renewal cannot revive session"
                        );
                        ensure!(
                            permit.expires_boot_ms <= wall_deadline,
                            "renewal exceeds attempt wall limit"
                        );
                        if matches!(config.manifest, LaunchDocument::Plan(_)) {
                            ensure!(
                                state.permit_sequence.checked_add(1) == Some(permit.sequence)
                                    && permit.expires_boot_ms > state.permit_deadline_boot_ms,
                                "host renewal must advance exact live prior permit"
                            );
                        }
                        state.permit_sequence = permit.sequence;
                        state.permit_deadline_boot_ms = permit.expires_boot_ms;
                        save(dir, &state)?;
                    }
                    if !ownership::pidfd_is_live(pane_fd.as_ref().context("pane pidfd")?)? {
                        state.phase = HostPhase::Stopping;
                        state.stop_reason = Some("harness pane exited".into());
                        save(dir, &state)?;
                    }
                    if state.phase == HostPhase::Running
                        && !terminal_failed
                        && terminal_io::tick(
                            dir,
                            &config,
                            &state,
                            wall_deadline,
                            &directory_guard,
                            &mut terminal_revision_highwater,
                        )
                        .is_err()
                    {
                        // Terminal IPC failure must not block independent expiry.
                        terminal_failed = true;
                        let _ = durable_new(
                            &dir.join("terminal.disabled"),
                            b"terminal effects refused\n",
                        );
                    }
                    Ok(())
                })();
                if let Err(error) = running {
                    state.phase = HostPhase::Stopping;
                    state.stop_reason = Some(format!("running validation failed: {error}"));
                    save(dir, &state)?;
                }
            }
        }
        if let Some(owner) = &mut observed_owner
            && matches!(state.phase, HostPhase::Waiting | HostPhase::Starting)
        {
            match owner.tick(&config, &state, dir, &reader_owner, &directory_guard) {
                Ok(true) => {
                    state.phase = HostPhase::Running;
                    state.observed_boot_ms = raw_boot_ms();
                    save(dir, &state)?;
                }
                Ok(false) => {}
                Err(_) => {
                    state.phase = HostPhase::Stopping;
                    state.stop_reason = Some(
                        "observed gate failed closed; post-grant exec evidence may be unknown"
                            .into(),
                    );
                    save(dir, &state)?;
                }
            }
        }
        reader_owner.tick(
            dir,
            &config,
            &state,
            wall_deadline,
            &directory_guard,
            reader_lease::Gate {
                highwater: &mut terminal_revision_highwater,
                pending_observation: observed_owner
                    .as_ref()
                    .and_then(|o| o.closed_ready())
                    .is_some(),
            },
        );
        viewer_owner.tick(
            dir,
            &config,
            &state,
            wall_deadline,
            &directory_guard,
            reader_lease::Gate {
                highwater: &mut viewer_revision_highwater,
                pending_observation: false,
            },
        );
        let _ = &root;
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(crate) fn exec_pane(dir: &Path) -> Result<()> {
    let config: SealedLaunch = read_json(&dir.join("launch.json"))?;
    let identity: GuardianIdentity = read_json(&dir.join("identity.json"))?;
    ensure!(
        config.intent()? == identity.intent && guardian_live(&identity),
        "pane launch identity/guardian mismatch"
    );
    let server: ProcessEvidence = read_json(&dir.join("server.json"))?;
    let _server_live = verified_pidfd(&server)?;
    ensure!(
        descendant(std::process::id(), server.pid)?,
        "pane outside owned server"
    );
    let permit = request(dir, "start.request", &identity)?.context("pane start authorization")?;
    config.validate_permit_at(
        &permit,
        &identity.intent.session,
        &identity.intent.manifest_digest,
        raw_boot_ms(),
    )?;
    ensure!(
        !dir.join("stop.request").exists() && raw_boot_ms() < config.wall_deadline()?,
        "pane cancelled/wall limit expired"
    );
    config.manifest.verify_startup(dir)?;
    if let Some(rendered) = &config.rendered_startup {
        let bytes = read_bytes(&dir.join("context-0.bin"), 1_048_576)?;
        rendered.verify(config.manifest.task()?, &bytes)?;
    }
    if config.observed.is_some() {
        return observed::gate::wait_closed(dir, &config, &identity, &server);
    }
    durable_new(
        &dir.join("pane-exec.claim"),
        b"one native harness exec; no fallback shell\n",
    )?;
    let mut command = Command::new(&config.command.executable);
    command
        .args(&config.command.arguments)
        .current_dir(&config.workspace)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("AQ_SESSION_DIRECTORY", dir)
        .envs(&config.command.environment);
    config.validate_permit_at(
        &permit,
        &identity.intent.session,
        &identity.intent.manifest_digest,
        raw_boot_ms(),
    )?;
    ensure!(
        raw_boot_ms() < config.wall_deadline()? && !dir.join("stop.request").exists(),
        "authority expired/cancelled immediately before native exec"
    );
    let error = command.exec();
    Err(error).context("native harness exec failed; no fallback")
}
/// Deterministic interactive fixture, explicitly not a real harness adapter.
#[cfg(feature = "test-support")]
pub(crate) fn fixture(dir: &Path, mode: &str) -> Result<()> {
    durable_new(&dir.join("fixture.claim"), b"synthetic interactive child\n")?;
    durable_new(
        &dir.join("fixture-process.json"),
        &serde_json::to_vec(&process_evidence(std::process::id())?)?,
    )?;
    if mode == "rendered" {
        let bytes = read_bytes(&dir.join("context-0.bin"), 1_048_576)?;
        durable_new(&dir.join("rendered-received.bin"), &bytes)?;
    }
    if mode == "fork-flood" {
        let producer = unsafe { libc::fork() };
        ensure!(producer >= 0, "producer fork");
        if producer == 0 {
            unsafe {
                libc::setsid();
                libc::signal(libc::SIGHUP, libc::SIG_IGN);
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            let result = (|| -> Result<()> {
                durable_new(
                    &dir.join("producer-process.json"),
                    &serde_json::to_vec(&process_evidence(std::process::id())?)?,
                )?;
                let mut count = 0u64;
                loop {
                    let first = unsafe { libc::fork() };
                    if first == 0 {
                        let second = unsafe { libc::fork() };
                        unsafe { libc::_exit(if second < 0 { 92 } else { 0 }) }
                    }
                    ensure!(first > 0, "flood fork");
                    let mut status = 0;
                    unsafe {
                        libc::waitpid(first, &mut status, 0);
                    };
                    count += 1;
                    if count.is_multiple_of(16) {
                        durable_replace(
                            &dir.join("producer-counter"),
                            count.to_string().as_bytes(),
                        )?;
                    }
                }
            })();
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 93 }) }
        }
    }
    if mode == "escaped" {
        let first = unsafe { libc::fork() };
        ensure!(first >= 0, "fixture fork");
        if first == 0 {
            if unsafe { libc::setsid() } < 0 {
                unsafe { libc::_exit(61) }
            }
            let second = unsafe { libc::fork() };
            if second < 0 {
                unsafe { libc::_exit(62) }
            }
            if second > 0 {
                unsafe { libc::_exit(0) }
            }
            let result = (|| -> Result<()> {
                durable_new(
                    &dir.join("escaped-process.json"),
                    &serde_json::to_vec(&process_evidence(std::process::id())?)?,
                )?;
                let mut count = 0u64;
                loop {
                    count += 1;
                    durable_replace(&dir.join("escaped-counter"), count.to_string().as_bytes())?;
                    std::thread::sleep(Duration::from_millis(5));
                }
            })();
            unsafe { libc::_exit(if result.is_ok() { 0 } else { 63 }) }
        }
        let mut status = 0;
        ensure!(
            unsafe { libc::waitpid(first, &mut status, 0) } == first,
            "fixture intermediate reap"
        );
    }
    println!("AQ_FIXTURE_READY");
    std::io::stdout().flush()?;
    for line in BufReader::new(std::io::stdin()).lines() {
        let line = line?;
        if mode == "output-pattern" && line == "emit" {
            std::io::stdout()
                .write_all("\x1b[?1049h\x1b[2J\x1b[3;4H雪🙂e\u{301}\x1b[?1049l\n".as_bytes())?;
            std::io::stdout().flush()?;
        }
        if mode == "output-flood" && line == "flood" {
            std::fs::write(
                dir.join("fixture-output-start"),
                b"synthetic flood accepted",
            )?;
            let block = vec![b'x'; 8192];
            for n in 1..=4096u64 {
                std::io::stdout().write_all(&block)?;
                if n.is_multiple_of(8) {
                    std::io::stdout().flush()?;
                    // Volatile synthetic progress: fsync latency must not stand in
                    // for PTY backpressure in the stalled-reader qualification.
                    std::fs::write(
                        dir.join("fixture-output-progress"),
                        (n * block.len() as u64).to_string().as_bytes(),
                    )?;
                }
            }
        }
        if mode == "terminal-journal" {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(dir.join("fixture-input.jsonl"))?;
            let mut encoded = serde_json::to_vec(&line)?;
            encoded.push(b'\n');
            file.write_all(&encoded)?;
            file.sync_all()?;
        }
        if line == "exit" {
            break;
        }
        println!("echo:{line}");
        std::io::stdout().flush()?;
    }
    Ok(())
}

pub(crate) fn fault_point(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    if std::env::var("AQ_SESSION_HOST_FAULT_STAGE").ok().as_deref() == Some(_stage) {
        durable_new(&_dir.join("guardian-fault.json"), _stage.as_bytes())?;
        unsafe { libc::raise(libc::SIGSTOP) };
    }
    Ok(())
}
