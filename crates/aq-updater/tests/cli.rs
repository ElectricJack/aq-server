//! The `aq-updater` binary: `status`, `run --once` without Docker work,
//! `verify-manifest` exit codes, and (with `test-verifier`) the test signing
//! round trip.

use aq_release::{
    HistoryEntry, ImageRefs, Outcome, ProtocolRange, ReleaseManifest, Step, UpdateState,
    UpdateStatus,
};
use aq_updater::journal::{InFlight, Journal, JournalState, lock, read_history};
use chrono::Utc;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use uuid::Uuid;

const BIN: &str = env!("CARGO_BIN_EXE_aq-updater");

struct H {
    _t: tempfile::TempDir,
    root: PathBuf,
    state_dir: PathBuf,
    config: PathBuf,
}

enum VerifierFixture {
    Cosign,
    #[cfg(feature = "test-verifier")]
    TestKey(String),
}

impl VerifierFixture {
    fn config(&self) -> String {
        match self {
            Self::Cosign => r#"kind = "cosign_keyless"
cosign_path = "/nonexistent/cosign"
identity_regexp = "^x$"
oidc_issuer = "https://token.actions.githubusercontent.com""#
                .to_owned(),
            #[cfg(feature = "test-verifier")]
            Self::TestKey(public_key) => {
                format!("kind = \"test_key\"\npublic_key_hex = \"{public_key}\"")
            }
        }
    }
}

impl H {
    fn new() -> H {
        Self::with_verifier(VerifierFixture::Cosign)
    }

    fn with_verifier(verifier: VerifierFixture) -> H {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let state_dir = root.join("state");
        let config = root.join("updater.toml");
        std::fs::write(
            &config,
            format!(
                r#"state_dir = "{state}"
backup_dir = "{root}/backups"
[compose]
project_dir = "{root}"
files = ["{root}/compose.yaml"]
project_name = "agent-q-test"
[verifier]
{verifier}
"#,
                state = state_dir.display(),
                root = root.display(),
                verifier = verifier.config(),
            ),
        )
        .unwrap();
        H {
            _t: t,
            root,
            state_dir,
            config,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env("RUST_LOG", "warn")
            .output()
            .unwrap()
    }
}

fn text(o: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}\n--- stderr\n{}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn assert_stdout(o: &Output, snippets: &[&str]) {
    let stdout = String::from_utf8_lossy(&o.stdout);
    for snippet in snippets {
        assert!(stdout.contains(snippet), "missing {snippet:?}: {}", text(o));
    }
}

fn manifest(v: &str, state_format: u32) -> ReleaseManifest {
    let digits = v.replace('.', "");
    ReleaseManifest {
        manifest_version: 1,
        version: v.parse().unwrap(),
        source_revision: format!("rev-{v}"),
        images: ImageRefs {
            server: format!("ghcr.io/electricjack/agent-q-server@sha256:{digits:0>64}"),
            worker: format!("ghcr.io/electricjack/agent-q-worker@sha256:{digits:f>64}"),
        },
        protocol: ProtocolRange { min: 1, max: 1 },
        state_format,
        readable_state_formats: vec![state_format],
        upgrade_from: ">=0.0.0".parse().unwrap(),
        created_at: Utc::now(),
    }
}

/// A journal left in `FailedNeedsOperator`: B failed and the rollback to A
/// failed too, with the state format changed so a restore is needed.
fn seed_needs_operator(state_dir: &Path) -> String {
    seed_needs_operator_locked(state_dir).0
}

/// As `seed_needs_operator`, also returning the journal, which still holds
/// the update lock.
fn seed_needs_operator_locked(state_dir: &Path) -> (String, Journal) {
    let (a, b) = (manifest("1.0.0", 1), manifest("1.1.0", 2));
    let request_id = Uuid::new_v4();
    let backup = PathBuf::from("/var/lib/agent-q/backups/20261030T120000Z-1.0.0-1.1.0.dump");
    let mut j = Journal::open(lock(state_dir).unwrap()).unwrap();
    j.record(JournalState {
        active: Some(a.clone()),
        previous: None,
        current: Some(InFlight {
            request_id,
            target: b.clone(),
            step: Step::RollingBack,
            backup: Some(backup.clone()),
            needs_restore_on_rollback: true,
            started_at: Utc::now(),
            rollback_attempted: true,
        }),
        active_backup: None,
    })
    .unwrap();
    let detail = format!(
        "1.1.0 failed (probe); the rollback to 1.0.0 also failed (start). Run `aq-updater restore-previous --restore-backup (restores {})`",
        backup.display()
    );
    for (outcome, d) in [
        (Outcome::Succeeded, "1.0.0 is active".to_string()),
        (Outcome::FailedNeedsOperator, detail.clone()),
    ] {
        j.append_history(HistoryEntry {
            request_id: Some(if outcome == Outcome::Succeeded {
                Uuid::new_v4()
            } else {
                request_id
            }),
            from: None,
            to: Some(if outcome == Outcome::Succeeded {
                a.version.clone()
            } else {
                b.version.clone()
            }),
            outcome,
            detail: d,
            started_at: Utc::now(),
            finished_at: Utc::now(),
        })
        .unwrap();
    }
    (detail, j)
}

#[test]
fn status_prints_needs_operator_instructions() {
    let h = H::new();
    // `status` is lock-free: it works while another process holds the lock.
    // The seeding journal keeps it: dropping it and locking again can fail
    // while another test's spawned child has not yet exec'd (its copy of
    // the lock fd shares the flock until then).
    let (detail, _held) = seed_needs_operator_locked(&h.state_dir);
    let journal_before = std::fs::read(h.state_dir.join("journal.json")).unwrap();
    let o = h.run(&["status"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(0), "{out}");
    assert_stdout(
        &o,
        &[
            "failed_needs_operator",
            &detail,
            "active:    1.0.0",
            "aq-updater restore-previous --restore-backup",
            "docker compose logs server",
            "succeeded",
        ],
    );
    // No writes.
    assert_eq!(
        std::fs::read(h.state_dir.join("journal.json")).unwrap(),
        journal_before
    );
    assert!(!h.state_dir.join("spool").exists(), "{out}");
}

#[test]
fn status_without_restore_hint_omits_the_flag() {
    let h = H::new();
    seed_needs_operator(&h.state_dir);
    // Rewrite the detail so it no longer asks for --restore-backup.
    let path = h.state_dir.join("history.jsonl");
    let text_hist = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text_hist.replace(" --restore-backup", "")).unwrap();
    let o = h.run(&["status"]);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        stdout.contains("aq-updater restore-previous\n"),
        "{}",
        text(&o)
    );
    assert!(!stdout.contains("--restore-backup"), "{}", text(&o));
}

#[test]
fn status_shows_in_progress_and_available() {
    let h = H::new();
    let a = manifest("1.0.0", 1);
    let b = manifest("1.1.0", 1);
    {
        let mut j = Journal::open(lock(&h.state_dir).unwrap()).unwrap();
        j.record(JournalState {
            active: Some(a.clone()),
            previous: None,
            current: Some(InFlight {
                request_id: Uuid::new_v4(),
                target: b.clone(),
                step: Step::BackedUp,
                backup: None,
                needs_restore_on_rollback: false,
                started_at: Utc::now(),
                rollback_attempted: false,
            }),
            active_backup: None,
        })
        .unwrap();
    }
    let status_dir = h.state_dir.join("spool/status");
    std::fs::create_dir_all(&status_dir).unwrap();
    let published = UpdateStatus {
        updater_version: "0.1.0".into(),
        state: UpdateState::Idle,
        active: None,
        previous: None,
        available: Some(aq_release::ReleaseSummary {
            version: "1.2.0".parse().unwrap(),
            source_revision: "rev".into(),
            server_image: b.images.server.clone(),
            state_format: 1,
        }),
        history: vec![],
        updated_at: Utc::now(),
    };
    std::fs::write(
        status_dir.join("status.json"),
        serde_json::to_vec(&published).unwrap(),
    )
    .unwrap();
    let o = h.run(&["status"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert_stdout(
        &o,
        &[
            "in_progress",
            "backed_up",
            "available: 1.2.0",
            "previous:  none",
        ],
    );
}

#[test]
fn status_prints_raw_corrupt_journal() {
    let h = H::new();
    std::fs::create_dir_all(&h.state_dir).unwrap();
    std::fs::write(
        h.state_dir.join("journal.json"),
        b"{\"journal_format\": 1, \"act",
    )
    .unwrap();
    let o = h.run(&["status"]);
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("will not be guessed at"), "{out}");
    assert!(stdout.contains("{\"journal_format\": 1, \"act"), "{out}");
}

#[test]
fn bad_config_exits_2() {
    let h = H::new();
    std::fs::write(&h.config, "state_dir = 1").unwrap();
    for args in [
        &["status"][..],
        &["run", "--once"],
        &["verify-manifest", "a", "b"],
    ] {
        let o = h.run(args);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o));
    }
    let o = Command::new(BIN).arg("no-such-command").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn run_once_with_nothing_to_do_publishes_idle_status() {
    let h = H::new();
    // A stale marker from a crash, and a malformed request.
    let spool = h.state_dir.join("spool");
    std::fs::create_dir_all(spool.join("status")).unwrap();
    std::fs::create_dir_all(spool.join("requests")).unwrap();
    std::fs::write(spool.join("status/maintenance"), b"").unwrap();
    std::fs::write(spool.join("requests/garbage.json"), b"nope").unwrap();
    let o = h.run(&["run", "--once"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert_spool_ready(&spool);
    assert_idle_rejection(&h.state_dir, &spool);
}

fn assert_spool_ready(spool: &Path) {
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&spool.join("requests")), 0o2770);
    assert_eq!(mode(&spool.join("status")), 0o755);
    assert!(!spool.join("status/maintenance").exists());
    assert!(!spool.join("requests/garbage.json").exists());
}

fn assert_idle_rejection(state_dir: &Path, spool: &Path) {
    let s: UpdateStatus =
        serde_json::from_slice(&std::fs::read(spool.join("status/status.json")).unwrap()).unwrap();
    assert_eq!(s.state, UpdateState::Idle);
    let hist = read_history(state_dir, 10).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].outcome, Outcome::Rejected);
    assert_eq!(hist[0].request_id, None);
    assert_eq!(s.history, hist);
}

#[test]
fn run_once_keeps_maintenance_for_needs_operator() {
    let h = H::new();
    seed_needs_operator(&h.state_dir);
    let o = h.run(&["run", "--once"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let status = h.state_dir.join("spool/status");
    assert!(status.join("maintenance").exists());
    let s: UpdateStatus =
        serde_json::from_slice(&std::fs::read(status.join("status.json")).unwrap()).unwrap();
    assert!(
        matches!(s.state, UpdateState::FailedNeedsOperator { .. }),
        "{s:?}"
    );
}

#[test]
fn locked_commands_refuse_while_locked() {
    let h = H::new();
    let _held = lock(&h.state_dir).unwrap();
    for args in [
        &["run", "--once"][..],
        &["install", "1.2.3"],
        &["restore-previous"],
    ] {
        let o = h.run(args);
        assert_eq!(o.status.code(), Some(1), "{args:?}: {}", text(&o));
        assert!(
            String::from_utf8_lossy(&o.stderr).contains("another aq-updater is running"),
            "{}",
            text(&o)
        );
    }
}

#[test]
fn install_rejects_bad_version() {
    let h = H::new();
    let o = h.run(&["install", "latest"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn verify_manifest_cosign_unavailable_is_config_error() {
    let h = H::new();
    let m = h.root.join("manifest.json");
    let b = h.root.join("manifest.sigstore.json");
    let mut release = manifest("1.0.0", 1);
    release.source_revision = "0123456789abcdef0123456789abcdef01234567".into();
    std::fs::write(&m, release.to_canonical_json()).unwrap();
    std::fs::write(&b, b"{}").unwrap();
    let o = h.run(&["verify-manifest", m.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    // A manifest the signature cannot be bound to is refused before cosign
    // runs: an invalid release, not a config error.
    std::fs::write(&m, manifest("1.0.0", 1).to_canonical_json()).unwrap();
    let o = h.run(&["verify-manifest", m.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(text(&o).contains("source_revision"), "{}", text(&o));
    let o = h.run(&["verify-manifest", "/nonexistent/m", b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[cfg(feature = "test-verifier")]
#[test]
fn test_sign_round_trip_through_verify_manifest() {
    let keys = tempfile::tempdir().unwrap();
    let keydir = keys.path().join("keys");
    let o = Command::new(BIN)
        .args(["test-keygen"])
        .arg(&keydir)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let secret = keydir.join("secret.hex");
    let public = std::fs::read_to_string(keydir.join("public.hex")).unwrap();
    let mode = std::fs::metadata(&secret).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    assert_eq!(public.trim().len(), 64);
    // Never overwrites an existing key.
    let again = Command::new(BIN)
        .arg("test-keygen")
        .arg(&keydir)
        .output()
        .unwrap();
    assert_ne!(again.status.code(), Some(0));

    let h = H::with_verifier(VerifierFixture::TestKey(public.trim().to_owned()));
    let m = h.root.join("manifest.json");
    std::fs::write(&m, manifest("1.0.0", 1).to_canonical_json()).unwrap();
    let o = Command::new(BIN)
        .args(["test-sign", "--key"])
        .arg(&secret)
        .arg(&m)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let bundle = String::from_utf8(o.stdout).unwrap();
    assert_eq!(bundle.len(), 129, "{bundle:?}");
    assert!(bundle.ends_with('\n'));
    assert!(
        bundle[..128]
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    );
    let b = h.root.join("manifest.sigstore.json");
    std::fs::write(&b, &bundle).unwrap();

    let o = h.run(&["verify-manifest", m.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));

    // One changed byte fails.
    let mut tampered = std::fs::read(&m).unwrap();
    let i = tampered.iter().position(|&c| c == b'1').unwrap();
    tampered[i] = b'2';
    std::fs::write(&m, &tampered).unwrap();
    let o = h.run(&["verify-manifest", m.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
}

#[test]
fn status_escapes_control_characters_in_history() {
    let h = H::new();
    {
        let j = Journal::open(lock(&h.state_dir).unwrap()).unwrap();
        j.append_history(HistoryEntry {
            request_id: None,
            from: None,
            to: None,
            outcome: Outcome::Rejected,
            detail: "bad \u{1b}[31mred\u{7} name".into(),
            started_at: Utc::now(),
            finished_at: Utc::now(),
        })
        .unwrap();
    }
    let o = h.run(&["status"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert!(
        !o.stdout.contains(&0x1b) && !o.stdout.contains(&0x07),
        "{}",
        text(&o)
    );
    assert!(
        String::from_utf8_lossy(&o.stdout).contains("bad \\u{1b}[31mred"),
        "{}",
        text(&o)
    );
}

#[test]
fn run_once_exits_with_a_stuck_request_entry() {
    let h = H::new();
    let requests = h.state_dir.join("spool/requests");
    std::fs::create_dir_all(requests.join("x.json")).unwrap();
    std::fs::write(requests.join("x.json/a"), b"1").unwrap();
    std::fs::create_dir_all(requests.join(".rejected-x.json")).unwrap();
    std::fs::write(requests.join(".rejected-x.json/b"), b"2").unwrap();
    let mut child = Command::new(BIN)
        .arg("--config")
        .arg(&h.config)
        .args(["run", "--once"])
        .env("RUST_LOG", "warn")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("run --once did not exit");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0));
    let hist = read_history(&h.state_dir, 10).unwrap();
    assert_eq!(hist.len(), 1, "{hist:?}");
    assert_eq!(hist[0].outcome, Outcome::Rejected);
}

const NOTICE: &str = "aq-updater: interrupt received; finishing the current operation so recovery stays consistent (send SIGKILL to abort; the journal will recover on next start)";

/// A `docker` stand-in on PATH: `docker <slow> ...` writes `started`,
/// sleeps, writes `finished` and fails (so the command is refused, quickly
/// and without a registry or database); everything else succeeds at once.
fn slow_docker(dir: &Path, slow: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("docker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = {slow} ]; then\n  : > {d}/started\n  sleep 2\n  : > {d}/finished\n  echo 'Error: not found' >&2\n  exit 1\nfi\nexit 0\n",
            d = dir.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn wait_for(path: &Path, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn spawn_with_path(h: &H, bin: &Path, args: &[&str]) -> std::process::Child {
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    Command::new(BIN)
        .arg("--config")
        .arg(&h.config)
        .args(args)
        .env("RUST_LOG", "warn")
        .env("PATH", path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

fn signal(pid: u32, sig: &str) {
    let ok = Command::new("kill")
        .args([sig, &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill {sig} {pid}");
}

/// Ctrl-C, a dropped SSH session or a SIGTERM during a manual install never
/// kill aq-updater mid-operation: the running docker command completes and
/// its outcome is recorded.
#[test]
fn install_finishes_through_interrupts() {
    let h = H::new();
    let bin = slow_docker(&h.root, "pull");
    let child = spawn_with_path(&h, &bin, &["install", "1.2.3"]);
    wait_for(&h.root.join("started"), "docker pull to start");
    for sig in ["-INT", "-HUP", "-TERM", "-INT"] {
        signal(child.id(), sig);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    let out = text(&o);
    // Exited by itself (a code, not a signal), after docker finished.
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(h.root.join("finished").exists(), "{out}");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert_eq!(stderr.matches(NOTICE).count(), 1, "{out}");
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(stdout.contains("refused"), "{out}");
    let hist = read_history(&h.state_dir, 10).unwrap();
    assert_eq!(hist.len(), 1, "{hist:?}");
    assert_eq!(hist[0].outcome, Outcome::Refused);
    assert!(
        hist[0].detail.contains("cannot fetch the release"),
        "{hist:?}"
    );
    let s: UpdateStatus = serde_json::from_slice(
        &std::fs::read(h.state_dir.join("spool/status/status.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(s.state, UpdateState::Idle);
}

/// The same for `restore-previous`, interrupted while it reads the
/// installed state format (a `docker compose exec`).
#[test]
fn restore_previous_finishes_through_interrupts() {
    let h = H::new();
    let bin = slow_docker(&h.root, "compose");
    {
        let mut j = Journal::open(lock(&h.state_dir).unwrap()).unwrap();
        j.record(JournalState {
            active: Some(manifest("1.1.0", 1)),
            previous: Some(manifest("1.0.0", 1)),
            current: None,
            active_backup: None,
        })
        .unwrap();
    }
    let child = spawn_with_path(&h, &bin, &["restore-previous"]);
    wait_for(&h.root.join("started"), "docker compose to start");
    for sig in ["-HUP", "-INT", "-TERM"] {
        signal(child.id(), sig);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let o = child.wait_with_output().unwrap();
    let out = text(&o);
    assert_eq!(o.status.code(), Some(1), "{out}");
    assert!(h.root.join("finished").exists(), "{out}");
    assert_eq!(
        String::from_utf8_lossy(&o.stderr).matches(NOTICE).count(),
        1,
        "{out}"
    );
    let hist = read_history(&h.state_dir, 10).unwrap();
    assert_eq!(hist.len(), 1, "{hist:?}");
    assert_eq!(hist[0].outcome, Outcome::Refused);
    assert!(hist[0].detail.contains("state format"), "{hist:?}");
}
