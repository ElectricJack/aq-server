//! `aq result submit|status` (spec 0006 §9 step 4): the worker-local result
//! claim, run as the real binary against a fixture outbox. Fake presence and
//! acknowledgments below create no real worker or B1 authority.
//!
//! `tests/golden/result.json` pins every step's exit status and output. The
//! same file is checked against `aq-agent result` in aq-worker
//! (`aq-agent-client` `tests/completion.rs`), so the two clients stay
//! byte-for-byte equal while the wrapper exists. Regenerate (and review the
//! diff) with `AQ_REGENERATE_GOLDEN=1 cargo test -p aqv2 --test cli -- result::`.
use aq_agent_result_local::{
    self as wire, Acknowledgment, Binding, Conclusion, Phase, Presence, Submission, SubmissionInput,
};
use aq_session_protocol::{ArtifactRef, Digest, ProfiledOffer, Text};
use nix::libc;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/result.json")
}
fn binding() -> Binding {
    let offer: ProfiledOffer =
        serde_json::from_slice(include_bytes!("../golden/result-offer.json")).unwrap();
    Binding {
        schema_version: 1,
        launch: offer.binding,
        execution_record_digest: Digest::of(b"fixture only, not actual execution"),
        expected_outcome: ArtifactRef {
            id: Text::new("aq.expected-outcome").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture outcome"),
        },
        operation_id: Uuid::from_u128(0x00000000_0000_4000_8000_0000000000c1),
        report_id: Uuid::from_u128(0x00000000_0000_4000_8000_0000000000c2),
        nonce: Uuid::from_u128(0x00000000_0000_4000_8000_0000000000c3),
    }
}
fn input(text: &str) -> Vec<u8> {
    wire::encode(&SubmissionInput {
        schema_version: 1,
        conclusion: Conclusion::new(text).unwrap(),
    })
    .unwrap()
}
fn boottime_ms() -> u64 {
    let t = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME).unwrap();
    t.tv_sec() as u64 * 1000 + t.tv_nsec() as u64 / 1_000_000
}
fn boot_id() -> Uuid {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// A private outbox as the worker installs it: 0700 root and `submissions/`,
/// the binding, a live presence and the owner lock held by this process.
struct Outbox {
    dir: tempfile::TempDir,
    _owner: File,
}
impl Outbox {
    fn new(binding_bytes: &[u8], binding: &Binding) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let private = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir.path(), private.clone()).unwrap();
        let submissions = dir.path().join(wire::SUBMISSIONS_DIRECTORY);
        std::fs::create_dir(&submissions).unwrap();
        std::fs::set_permissions(&submissions, private).unwrap();
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.path().join(wire::OWNER_LOCK_FILE))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let this = Self { dir, _owner: owner };
        this.write(wire::BINDING_FILE, binding_bytes);
        let now = boottime_ms();
        let presence = Presence {
            schema_version: 1,
            binding_digest: binding.digest().unwrap(),
            kernel_boot_id: boot_id(),
            original_execution_deadline_boot_ns: (now + 60_000) * 1_000_000,
            original_hard_deadline_boot_ns: (now + 120_000) * 1_000_000,
        };
        this.write(wire::PRESENCE_FILE, &wire::encode(&presence).unwrap());
        this
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn submission(&self) -> Vec<u8> {
        std::fs::read(
            self.dir
                .path()
                .join(wire::SUBMISSIONS_DIRECTORY)
                .join(wire::SUBMISSION_FILE),
        )
        .unwrap()
    }
}
/// Run `aq ARGS` as the agent would, `$REQUEST`/`$OTHER` naming request files.
fn aq(outbox: Option<&Outbox>, args: &[&str]) -> (u8, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_aqv2"));
    c.env_remove("AQ_CHANNEL").env_remove(wire::RESULT_DIR_ENV);
    if let Some(o) = outbox {
        c.env(wire::RESULT_DIR_ENV, o.dir.path());
    }
    for a in args {
        let sub = |name: &str| o_path(outbox, name);
        c.arg(match *a {
            "$REQUEST" => sub("request.json"),
            "$OTHER" => sub("other.json"),
            a => a.into(),
        });
    }
    let out = c.output().unwrap();
    let text = |b: Vec<u8>| String::from_utf8(b).unwrap();
    (
        out.status.code().unwrap() as u8,
        text(out.stdout),
        text(out.stderr),
    )
}
fn o_path(outbox: Option<&Outbox>, name: &str) -> String {
    let dir = outbox.unwrap().dir.path().join(wire::SUBMISSIONS_DIRECTORY);
    dir.join(name).to_str().unwrap().into()
}
fn ack(submission: &[u8], phase: Phase) -> Vec<u8> {
    let s: Submission = wire::decode(submission).unwrap();
    wire::encode(&Acknowledgment {
        schema_version: 1,
        operation_id: s.binding.operation_id,
        report_id: s.binding.report_id,
        binding_digest: s.binding.digest().unwrap(),
        submission_digest: s.digest().unwrap(),
        phase,
    })
    .unwrap()
}

#[derive(Default)]
struct Transcript {
    steps: Vec<Value>,
}

fn capture_step(name: &str, args: &[&str], outbox: Option<&Outbox>, write: Value) -> Value {
    let (exit, stdout, stderr) = aq(outbox, args);
    json!({
        "name":name, "write":write, "args":args, "aq_only":args.contains(&"--json"),
        "exit":exit, "stdout":stdout, "stderr":stderr
    })
}

impl Transcript {
    fn claim(&mut self, outbox: &Outbox) -> Vec<u8> {
        let submit = &["result", "submit", "--request-file", "$REQUEST"];
        self.steps.push(capture_step(
            "ready",
            &["result", "status"],
            Some(outbox),
            json!({}),
        ));
        self.steps
            .push(capture_step("submit", submit, Some(outbox), json!({})));
        let submission = outbox.submission();
        self.steps.push(capture_step(
            "submit_again",
            submit,
            Some(outbox),
            json!({}),
        ));
        self.steps.push(capture_step(
            "changed_claim",
            &["result", "submit", "--request-file", "$OTHER"],
            Some(outbox),
            json!({}),
        ));
        assert_eq!(outbox.submission(), submission, "the original is kept");
        submission
    }

    fn acknowledge(&mut self, name: &str, outbox: &Outbox, bytes: Vec<u8>) {
        outbox.write("ack.json", &bytes);
        self.steps.push(capture_step(
            name,
            &["result", "status"],
            Some(outbox),
            json!({"ack.json":String::from_utf8(bytes).unwrap()}),
        ));
    }

    fn statuses(&mut self, binding: &Binding, outbox: &Outbox, submission: &[u8]) {
        self.acknowledge("accepted", outbox, ack(submission, Phase::Accepted));
        let report = binding.report_id;
        let receipt = ArtifactRef {
            id: Text::new(format!("aq.result-record/{report}")).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"fixture receipt"),
        };
        self.acknowledge(
            "recorded",
            outbox,
            ack(submission, Phase::Recorded { receipt }),
        );
        self.steps.push(capture_step(
            "recorded_json",
            &["--json", "result", "status"],
            Some(outbox),
            json!({}),
        ));
        self.steps.push(capture_step(
            "no_channel",
            &["result", "status"],
            None,
            json!({}),
        ));
        self.steps.push(capture_step(
            "no_channel_json",
            &["result", "status", "--json"],
            None,
            json!({}),
        ));
    }
}

fn check_transcript(doc: &Value, binding: &Binding, outbox: &Outbox) {
    let actual = serde_json::to_string_pretty(doc).unwrap() + "\n";
    if std::env::var_os("AQ_REGENERATE_GOLDEN").is_some() {
        std::fs::write(golden_path(), &actual).unwrap();
    }
    let expected = std::fs::read_to_string(golden_path()).unwrap_or_default();
    assert!(
        expected == actual,
        "result.json drifted; regenerate with AQ_REGENERATE_GOLDEN=1 and review the diff.\n{actual}"
    );
    // What the golden must say, independent of regeneration.
    let exits: Vec<(String, u64)> = doc["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().into(),
                s["exit"].as_u64().unwrap(),
            )
        })
        .collect();
    let want = [
        ("ready", 0),
        ("submit", 10),
        ("submit_again", 10),
        ("changed_claim", 11),
        ("accepted", 10),
        ("recorded", 0),
        ("recorded_json", 0),
        ("no_channel", 15),
        ("no_channel_json", 15),
    ];
    let want: Vec<(String, u64)> = want.iter().map(|(n, e)| ((*n).into(), *e)).collect();
    assert_eq!(exits, want);
    let pending: Value = serde_json::from_str(doc["steps"][1]["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(pending["phase"]["kind"], "pending_local");
    assert_eq!(pending["operation_id"], json!(binding.operation_id));
    // The client never prints the nonce, a path or a credential.
    let printed = doc["steps"].to_string();
    assert!(!printed.contains(&binding.nonce.to_string()));
    assert!(!printed.contains(outbox.dir.path().to_str().unwrap()));
    assert!(!printed.contains("aqa_"));
}

/// The full claim lifecycle through the binary, pinned by the golden file:
/// ready, a new claim (pending after the bounded wait), the same claim again,
/// a changed claim (conflict, original kept), the worker's accepted and
/// recorded acknowledgments, `--json`, and an absent channel (exit 15).
#[test]
fn result_submit_and_status_match_the_golden_and_aq_agent() {
    let binding = binding();
    let binding_bytes = wire::encode(&binding).unwrap();
    let outbox = Outbox::new(&binding_bytes, &binding);
    let request = input("Changed scoped file; cargo test -p example exit0; limits none");
    let other = input("different claim");
    outbox.write("submissions/request.json", &request);
    outbox.write("submissions/other.json", &other);
    let mut transcript = Transcript::default();
    let submission = transcript.claim(&outbox);
    transcript.statuses(&binding, &outbox, &submission);
    let doc = json!({
        "contract": wire::CONTRACT,
        "note": "Generated by aqv2 tests/cli/result.rs; aq-worker checks aq-agent result against it.",
        "binding": String::from_utf8(binding_bytes).unwrap(),
        "request": String::from_utf8(request).unwrap(),
        "other": String::from_utf8(other).unwrap(),
        "submission": String::from_utf8(submission).unwrap(),
        "steps": transcript.steps,
    });
    check_transcript(&doc, &binding, &outbox);
}

/// The channel is the worker's: a group- or world-accessible root, a symlinked
/// request, or an oversize or privileged request is refused before any write.
#[test]
fn result_refuses_an_unsafe_channel_or_request() {
    let binding = binding();
    let outbox = Outbox::new(&wire::encode(&binding).unwrap(), &binding);
    let submissions = outbox.dir.path().join(wire::SUBMISSIONS_DIRECTORY);
    outbox.write("submissions/request.json", &input("ok"));
    std::os::unix::fs::symlink(
        submissions.join("request.json"),
        submissions.join("link.json"),
    )
    .unwrap();
    let link = submissions.join("link.json");
    let (exit, _, stderr) = aq(
        Some(&outbox),
        &["result", "submit", "--request-file", link.to_str().unwrap()],
    );
    assert_eq!(
        (exit, stderr.trim()),
        (15, aqv2::result::refused().to_string().as_str())
    );
    let privileged = br#"{"schema_version":1,"conclusion":"x","operation_id":"00000000-0000-4000-8000-000000000001"}"#;
    outbox.write("submissions/privileged.json", privileged);
    let path = submissions.join("privileged.json");
    let (exit, _, _) = aq(
        Some(&outbox),
        &["result", "submit", "--request-file", path.to_str().unwrap()],
    );
    assert_eq!(exit, 15);
    outbox.write(
        "submissions/big.json",
        &vec![b' '; wire::MAX_INPUT_BYTES + 1],
    );
    let path = submissions.join("big.json");
    let (exit, _, _) = aq(
        Some(&outbox),
        &["result", "submit", "--request-file", path.to_str().unwrap()],
    );
    assert_eq!(exit, 15);
    assert!(!submissions.join(wire::SUBMISSION_FILE).exists());
    std::fs::set_permissions(outbox.dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
    let (exit, _, _) = aq(Some(&outbox), &["result", "status"]);
    assert_eq!(exit, 15);
}
