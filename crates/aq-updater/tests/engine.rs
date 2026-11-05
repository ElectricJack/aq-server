//! Update engine against `FakeRuntime`. Signed with a `TestKey` generated
//! here, so this file needs `--features test-verifier`.
#![cfg(feature = "test-verifier")]

mod common;

use aq_release::{
    HistoryEntry, ImageRefs, Outcome, ProtocolRange, ReadinessReport, ReleaseManifest, Step,
    UpdateRequest, UpdateState,
};
use aq_updater::engine::{Engine, EngineSettings};
use aq_updater::journal::{Journal, lock, read_history};
use aq_updater::verify::TestKey;
use chrono::Utc;
use common::fake_runtime::FakeRuntime;
use ed25519_dalek::{Signer, SigningKey};
use semver::Version;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

const A: &str = "1.0.0";
const B: &str = "1.1.0";

type TestEngine = Engine<FakeRuntime, TestKey>;

struct H {
    _t: tempfile::TempDir,
    state_dir: PathBuf,
    backup_dir: PathBuf,
    fake: FakeRuntime,
    key: SigningKey,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_key() -> SigningKey {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    SigningKey::from_bytes(&seed)
}

fn manifest(v: &str, state_format: u32, readable: &[u32]) -> ReleaseManifest {
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
        readable_state_formats: readable.to_vec(),
        upgrade_from: ">=0.0.0".parse().unwrap(),
        created_at: Utc::now(),
    }
}

fn req(v: &str) -> UpdateRequest {
    UpdateRequest {
        id: Uuid::new_v4(),
        version: Some(v.parse().unwrap()),
        requested_by: "owner".into(),
        requested_at: Utc::now(),
    }
}

fn ver(v: &str) -> Version {
    v.parse().unwrap()
}

impl H {
    fn new() -> H {
        let t = tempfile::tempdir().unwrap();
        H {
            state_dir: t.path().join("state"),
            backup_dir: t.path().join("backups"),
            _t: t,
            fake: FakeRuntime::default(),
            key: random_key(),
        }
    }

    fn settings(&self) -> EngineSettings {
        EngineSettings {
            backup_dir: self.backup_dir.clone(),
            readiness_deadline: Duration::from_secs(120),
            min_free_bytes: 2 * 1024 * 1024 * 1024,
            backups_keep: 5,
            expected_auth_mode: "cloudflare_access".into(),
            maintenance_drain: Duration::ZERO,
        }
    }

    fn engine_with(&self, settings: EngineSettings) -> TestEngine {
        let journal = Journal::open(lock(&self.state_dir).unwrap()).unwrap();
        let verifier = TestKey::from_hex(&hex(self.key.verifying_key().as_bytes())).unwrap();
        Engine::new(
            journal,
            self.fake.clone(),
            verifier,
            Box::new(self.fake.maintenance()),
            settings,
        )
    }

    fn engine(&self) -> TestEngine {
        self.engine_with(self.settings())
    }

    /// Publish `m` signed by the trusted key.
    fn publish(&self, m: &ReleaseManifest) {
        let bytes = m.to_canonical_json();
        let sig = hex(&self.key.sign(&bytes).to_bytes());
        self.fake
            .add_release(m, bytes, format!("{sig}\n").into_bytes());
    }

    /// Publish `m` signed by some other key.
    fn publish_untrusted(&self, m: &ReleaseManifest) {
        let bytes = m.to_canonical_json();
        let sig = hex(&random_key().sign(&bytes).to_bytes());
        self.fake.add_release(m, bytes, sig.into_bytes());
    }

    fn history(&self) -> Vec<HistoryEntry> {
        read_history(&self.state_dir, 1000).unwrap()
    }

    fn dumps(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = match std::fs::read_dir(&self.backup_dir) {
            Ok(rd) => rd
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "dump"))
                .collect(),
            Err(_) => Vec::new(),
        };
        v.sort();
        v
    }
}

/// A harness with release A (format 1) installed and running, log cleared.
async fn with_a() -> (H, TestEngine, ReleaseManifest) {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    let mut e = h.engine();
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(h.fake.running().as_deref(), Some(a.images.server.as_str()));
    h.fake.clear_log();
    (h, e, a)
}

fn active_version(e: &TestEngine) -> Option<Version> {
    e.journal_state().active.as_ref().map(|m| m.version.clone())
}

#[tokio::test]
async fn successful_update_a_to_b() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(r.from, Some(ver(A)));
    assert_eq!(r.to, Some(ver(B)));

    let s = e.journal_state();
    assert_eq!(s.active.as_ref().unwrap().version, ver(B));
    assert_eq!(s.previous.as_ref().unwrap().version, ver(A));
    assert!(s.current.is_none());
    // Persisted, not only in memory.
    let disk = Journal::read(&h.state_dir).unwrap();
    assert_eq!(disk.active.unwrap().version, ver(B));

    // Maintenance entered then left, around the backup and the replacement.
    let enter = h.fake.position("enter").unwrap();
    let backup = h.fake.position("backup").unwrap();
    let start = h.fake.position("start_server").unwrap();
    let leave = h.fake.position("leave").unwrap();
    assert!(
        enter < backup && backup < start && start < leave,
        "{:?}",
        h.fake.log()
    );
    assert!(!h.fake.maintenance_on());
    assert_eq!(
        h.fake.position("start_server"),
        h.fake
            .position(&format!("start_server {}", b.images.server))
    );
    assert_eq!(
        h.fake.count("stop_server"),
        0,
        "A is replaced in place, not stopped first"
    );

    // Backup taken, named <utc>-<from>-<to>.dump, and recorded as active_backup.
    let backup = e.journal_state().active_backup.clone().unwrap();
    assert!(backup.exists());
    let name = backup.file_name().unwrap().to_str().unwrap();
    assert!(name.ends_with(&format!("-{A}-{B}.dump")), "{name}");
    assert_eq!(
        name.len(),
        "20261030T120000Z".len() + format!("-{A}-{B}.dump").len(),
        "{name}"
    );
    assert_eq!(h.dumps().len(), 2, "first-install backup plus this one");

    assert_eq!(h.history()[0], r);
    assert_eq!(h.history().len(), 2);
    let st = e.status();
    assert_eq!(st.state, UpdateState::Idle);
    assert_eq!(st.active.unwrap().version, ver(B));
    assert_eq!(st.previous.unwrap().server_image, a.images.server);
    assert_eq!(st.history[0], r);
    assert_eq!(h.fake.running().unwrap(), b.images.server);
}

#[tokio::test]
async fn channel_request_installs_channel_release() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    let mut r = req(B);
    r.version = None;
    let out = e.install(r).await.unwrap();
    assert_eq!(out.outcome, Outcome::Succeeded, "{}", out.detail);
    assert_eq!(h.fake.log()[0], "fetch_release channel");
}

#[tokio::test]
async fn first_install_has_no_from_and_backs_up_none() {
    let (h, e, _a) = with_a().await;
    let first = &h.history()[0];
    assert_eq!(first.from, None);
    assert_eq!(first.outcome, Outcome::Succeeded);
    assert!(e.journal_state().previous.is_none());
    let name = h.dumps()[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(name.ends_with(&format!("-none-{A}.dump")), "{name}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&h.backup_dir)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "backup dir mode {mode:o}");
}

#[tokio::test]
async fn unverified_release_refused_before_pull() {
    let (h, mut e, _a) = with_a().await;
    h.publish_untrusted(&manifest(B, 1, &[1]));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("verif"), "{}", r.detail);
    assert_eq!(h.fake.count("pull"), 0);
    assert_eq!(h.fake.log(), vec![format!("fetch_release {B}")]);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.history()[0], r);
}

#[tokio::test]
async fn version_mismatch_between_request_and_manifest_refused() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let mut r = req(B);
    r.version = Some(ver("1.2.0"));
    // Serve B's manifest under 1.2.0.
    let bytes = b.to_canonical_json();
    let sig = hex(&h.key.sign(&bytes).to_bytes());
    let mut b2 = b.clone();
    b2.version = ver("1.2.0");
    h.fake.add_release(&b2, bytes, sig.into_bytes());
    let out = e.install(r).await.unwrap();
    assert_eq!(out.outcome, Outcome::Refused, "{}", out.detail);
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn incompatible_state_refused() {
    let (h, mut e, _a) = with_a().await;
    // B cannot read format 1, which the database holds.
    h.publish(&manifest(B, 2, &[2]));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("state format 1"), "{}", r.detail);
    assert_eq!(h.fake.count("pull"), 0);
    assert_eq!(h.fake.count("enter"), 0);
    assert!(e.journal_state().current.is_none());
    assert_eq!(active_version(&e), Some(ver(A)));
}

#[tokio::test]
async fn downgrade_and_same_version_refused() {
    let (h, mut e, _a) = with_a().await;
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    h.publish(&manifest("0.9.0", 1, &[1]));
    let r = e.install(req("0.9.0")).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("restore-previous"), "{}", r.detail);
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn low_disk_refused_before_maintenance() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.set_free(2 * 1024 * 1024 * 1024 - 1);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("free"), "{}", r.detail);
    assert_eq!(h.fake.count("pull"), 0);
    assert_eq!(h.fake.count("enter"), 0);
    assert!(e.journal_state().current.is_none());
}

#[tokio::test]
async fn free_space_needs_three_times_last_backup() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    let settings = EngineSettings {
        min_free_bytes: 10,
        ..h.settings()
    };
    h.fake.set_backup_size(1000);
    let mut e = h.engine_with(settings);
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    h.publish(&manifest(B, 1, &[1]));
    // The A install wrote a 1000-byte dump: 3000 bytes are needed.
    h.fake.set_free(2999);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("3000"), "{}", r.detail);
    h.fake.set_free(3000);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
}

#[tokio::test]
async fn pull_failure_aborts_without_stopping_a() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.fail("pull", 0, "registry unreachable");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("registry unreachable"), "{}", r.detail);
    assert_eq!(h.fake.count("start_server"), 0);
    assert_eq!(h.fake.count("stop_server"), 0);
    assert_eq!(h.fake.count("enter"), 0);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert!(e.journal_state().current.is_none());
    assert_eq!(active_version(&e), Some(ver(A)));
}

#[tokio::test]
async fn image_label_mismatch_aborts_before_maintenance() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_labels(
        &b.images.server,
        HashMap::from([("io.agent-q.state-format".to_string(), "2".to_string())]),
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("io.agent-q.state-format"), "{}", r.detail);
    assert_eq!(h.fake.count("enter"), 0);
    assert_eq!(h.fake.count("start_server"), 0);
}

#[tokio::test]
async fn backup_failure_aborts_and_leaves_maintenance() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("backup", 0, "No space left on device");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    assert!(r.detail.contains("No space left on device"), "{}", r.detail);
    assert!(h.fake.position("enter").unwrap() < h.fake.position("leave").unwrap());
    assert!(!h.fake.maintenance_on(), "maintenance cleared");
    assert_eq!(h.fake.count("start_server"), 0);
    assert_eq!(h.fake.count("stop_server"), 0);
    assert_eq!(h.fake.running().unwrap(), a.images.server, "A left running");
    assert!(e.journal_state().current.is_none());
    assert_eq!(active_version(&e), Some(ver(A)));
    assert_eq!(h.dumps().len(), 1, "no partial dump left behind");
}

#[tokio::test]
async fn existing_backup_file_is_never_overwritten_or_deleted() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    // Occupy the backup name for this second and the next two.
    let now = Utc::now();
    let taken: Vec<PathBuf> = (0..3)
        .map(|s| {
            let t = now + chrono::Duration::seconds(s);
            let p = h
                .backup_dir
                .join(format!("{}-{A}-{B}.dump", t.format("%Y%m%dT%H%M%SZ")));
            std::fs::write(&p, b"someone else's").unwrap();
            p
        })
        .collect();
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted, "{}", r.detail);
    assert!(r.detail.contains("already exists"), "{}", r.detail);
    assert_eq!(h.fake.count("backup"), 0);
    for p in &taken {
        assert_eq!(std::fs::read(p).unwrap(), b"someone else's");
    }
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn unhealthy_b_rolls_back_to_a() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("wait_healthy", 0, "health: unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("wait_healthy"), "{}", r.detail);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(e.journal_state().previous.is_none());
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert!(!h.fake.maintenance_on());
    assert_eq!(h.history()[0], r);
}

#[tokio::test]
async fn readiness_version_mismatch_rolls_back() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    // B's image answers as A: the probe must not accept it.
    h.fake.set_report(
        &b.images.server,
        ReadinessReport {
            version: ver(A),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "cloudflare_access".into(),
        },
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("version"), "{}", r.detail);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn readiness_checks_revision_database_protocol_and_auth() {
    let mk = |f: &dyn Fn(&mut ReadinessReport)| {
        let b = manifest(B, 1, &[1]);
        let mut r = ReadinessReport {
            version: ver(B),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "cloudflare_access".into(),
        };
        f(&mut r);
        r
    };
    let cases: Vec<(&str, ReadinessReport)> = vec![
        (
            "source_revision",
            mk(&|r| r.source_revision = "other".into()),
        ),
        ("state_format", mk(&|r| r.state_format = 2)),
        ("database", mk(&|r| r.database = "error".into())),
        ("protocol", mk(&|r| r.protocol_max = 2)),
        ("auth_mode", mk(&|r| r.auth_mode = "dev".into())),
    ];
    for (what, report) in cases {
        let (h, mut e, _a) = with_a().await;
        let b = manifest(B, 1, &[1]);
        h.publish(&b);
        h.fake.set_report(&b.images.server, report);
        let r = e.install(req(B)).await.unwrap();
        assert_eq!(r.outcome, Outcome::RolledBack, "{what}: {}", r.detail);
        assert!(r.detail.contains(what), "{what}: {}", r.detail);
    }
}

#[tokio::test]
async fn rollback_restores_backup_when_state_format_changed() {
    let (h, mut e, a) = with_a().await;
    // B reads format 1 and migrates it to 2; A cannot read 2.
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.fail("wait_healthy", 0, "health: unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);

    let suffix = format!("-{A}-{B}.dump");
    let backup = h
        .dumps()
        .into_iter()
        .find(|d| d.to_str().unwrap().ends_with(&suffix))
        .unwrap();
    let log = h.fake.log();
    let restore = h.fake.position("restore").expect("restore called");
    assert_eq!(log[restore], format!("restore {}", backup.display()));
    let stop = h.fake.position("stop_server").unwrap();
    let start_a = log
        .iter()
        .rposition(|c| *c == format!("start_server {}", a.images.server))
        .unwrap();
    assert!(stop < restore && restore < start_a, "{log:?}");
    assert_eq!(h.fake.db_format(), Some(1), "A's data is back");
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn rollback_skips_restore_when_state_format_same() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("readiness", 0, "connection refused");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0);
    assert_eq!(h.fake.count("stop_server"), 0);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn failed_rollback_stops_with_needs_operator() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("wait_healthy", 1, "A unhealthy");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert!(
        r.detail.contains("B unhealthy") && r.detail.contains("A unhealthy"),
        "{}",
        r.detail
    );
    let start_a = format!("start_server {}", a.images.server);
    assert_eq!(h.fake.count(&start_a), 1, "exactly one rollback attempt");
    assert_eq!(h.fake.count("wait_healthy"), 2);
    assert!(h.fake.maintenance_on(), "maintenance stays entered");
    assert_eq!(h.fake.count("leave"), 0);
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.step, Step::RollingBack);
    assert!(cur.rollback_attempted);
    match e.status().state {
        UpdateState::FailedNeedsOperator { detail } => assert_eq!(detail, r.detail),
        other => panic!("expected FailedNeedsOperator, got {other:?}"),
    }

    // Nothing loops: a restart takes no runtime action and adds no entry,
    // but makes sure maintenance is on even if the marker was lost.
    drop(e);
    h.fake.clear_log();
    h.fake.set_maintenance(false);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.fake.log(), vec!["enter".to_string()]);
    assert!(h.fake.maintenance_on());
    h.fake.clear_log();
    assert_eq!(
        h.history()
            .iter()
            .filter(|x| x.outcome == Outcome::FailedNeedsOperator)
            .count(),
        1
    );
    // And new requests are rejected while the operator decides.
    let r2 = e.install(req(B)).await.unwrap();
    assert_eq!(r2.outcome, Outcome::Rejected);
    assert!(h.fake.log().is_empty());
}

#[tokio::test]
async fn restore_previous_after_needs_operator_retries_active_once() {
    let (h, mut e, a) = with_a().await;
    // B migrates to format 2, then both B and the rollback fail.
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("start_server", 1, "A would not start");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert!(r.detail.contains("restore-previous"), "{}", r.detail);

    // The data may be B's, so the operator must opt into the restore.
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("--restore-backup"), "{}", r.detail);
    assert!(h.fake.log().is_empty());
    assert!(matches!(
        e.status().state,
        UpdateState::FailedNeedsOperator { .. }
    ));

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_eq!(h.fake.db_format(), Some(1));
    assert!(h.fake.position("restore").unwrap() < h.fake.position("start_server").unwrap());
    assert!(e.journal_state().current.is_none());
    assert!(!h.fake.maintenance_on());
    assert_eq!(e.status().state, UpdateState::Idle);
}

#[tokio::test]
async fn first_install_failed_probe_aborts() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    h.fake.fail("wait_healthy", 0, "never healthy");
    let mut e = h.engine();
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted, "{}", r.detail);
    assert!(r.detail.contains("never healthy"), "{}", r.detail);
    assert_eq!(h.fake.count("stop_server"), 1);
    assert!(h.fake.position("start_server").unwrap() < h.fake.position("stop_server").unwrap());
    assert!(h.fake.running().is_none());
    assert!(!h.fake.maintenance_on());
    assert!(e.journal_state().active.is_none());
    assert!(e.journal_state().current.is_none());
}

#[tokio::test]
async fn second_request_rejected_while_in_flight() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.publish(&manifest("1.2.0", 1, &[1]));
    // Leave an update in flight (process "dies" while probing).
    h.fake.crash("wait_healthy", 0);
    assert!(e.install(req(B)).await.is_err());
    assert!(e.journal_state().current.is_some());

    // A second updater process cannot take the lock.
    assert!(lock(&h.state_dir).is_err());

    // A second request in this process is rejected without touching the runtime.
    h.fake.clear_log();
    let second = req("1.2.0");
    let r = e.install(second.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Rejected);
    assert_eq!(r.detail, "update_in_progress");
    assert_eq!(r.request_id, Some(second.id));
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert_eq!(h.history()[0], r);
    // The in-flight update is untouched.
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.target.version, ver(B));
    assert_eq!(cur.step, Step::Probing);
    match e.status().state {
        UpdateState::InProgress { target, step, .. } => {
            assert_eq!(target, Some(ver(B)));
            assert_eq!(step, Step::Probing);
        }
        other => panic!("expected InProgress, got {other:?}"),
    }
}

/// One row of the crash table: crash right after `step` was persisted.
struct CrashCase {
    step: Step,
    /// Script the fake so the next runtime call after `step` crashes.
    script: fn(&FakeRuntime),
    /// Rewrite the persisted journal before recovery (seed a state the
    /// engine itself never writes).
    rollback_attempted_override: Option<bool>,
    /// The crash came after the outcome line was appended. With
    /// `drop_history_line` the line is removed again, i.e. the crash landed
    /// between the journal write and the history append.
    recorded: bool,
    drop_history_line: bool,
    outcome: Outcome,
    active: &'static str,
    maintenance_on: bool,
}

fn crash_cases() -> Vec<CrashCase> {
    vec![
        CrashCase {
            step: Step::Requested,
            script: |f| f.crash("server_state_format", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::Aborted,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::Validated,
            script: |f| f.crash("free_bytes", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::Aborted,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::Pulled,
            script: |f| f.crash("pull", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::Aborted,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::Maintenance,
            script: |f| f.crash("enter", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::Aborted,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::BackedUp,
            script: |f| f.crash("backup", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::Aborted,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::Replacing,
            script: |f| f.crash("start_server", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
        CrashCase {
            step: Step::Probing,
            script: |f| f.crash("wait_healthy", 0),
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
        // Swap persisted, crash before the history line: recovery finishes.
        CrashCase {
            step: Step::Active,
            script: |f| f.crash("leave", 0),
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: true,
            outcome: Outcome::Succeeded,
            active: B,
            maintenance_on: false,
        },
        // Outcome recorded, crash after leave(): recovery only clears.
        CrashCase {
            step: Step::Active,
            script: |f| f.crash_after("leave", 0),
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: false,
            outcome: Outcome::Succeeded,
            active: B,
            maintenance_on: false,
        },
        // Rollback finished and maintenance left, but the history line is
        // missing: the engine cannot tell the rollback worked, so it stops
        // for the operator and turns maintenance back on.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash_after("leave", 0);
            },
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: true,
            outcome: Outcome::FailedNeedsOperator,
            active: A,
            maintenance_on: true,
        },
        // Same crash with the line present: history is written before
        // leave(), so the rollback is known to have worked.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash_after("leave", 0);
            },
            rollback_attempted_override: None,
            recorded: true,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
        // Crash during the rollback itself: never retried automatically.
        CrashCase {
            step: Step::RollingBack,
            script: |f| {
                f.fail("wait_healthy", 0, "B unhealthy");
                f.crash("start_server", 1);
            },
            rollback_attempted_override: None,
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::FailedNeedsOperator,
            active: A,
            maintenance_on: true,
        },
        // RollingBack persisted without the attempt flag: roll back now.
        CrashCase {
            step: Step::RollingBack,
            script: |f| f.crash("wait_healthy", 0),
            rollback_attempted_override: Some(false),
            recorded: false,
            drop_history_line: false,
            outcome: Outcome::RolledBack,
            active: A,
            maintenance_on: false,
        },
    ]
}

/// Crash right after each persisted step of an A -> B install, restart, and
/// check what recovery concludes. B has `b_format`; when it differs from A's
/// (1), B migrates the data and every rollback must restore it.
async fn run_crash_table(b_format: u32, b_readable: &[u32]) {
    let cases = crash_cases();
    // Every persisted step is covered.
    for s in [
        Step::Requested,
        Step::Validated,
        Step::Pulled,
        Step::Maintenance,
        Step::BackedUp,
        Step::Replacing,
        Step::Probing,
        Step::Active,
        Step::RollingBack,
    ] {
        assert!(cases.iter().any(|c| c.step == s), "no crash case for {s:?}");
    }

    for c in &cases {
        let label = format!(
            "B format {b_format}: {:?} (override {:?}, recorded {}, dropped {})",
            c.step, c.rollback_attempted_override, c.recorded, c.drop_history_line
        );
        let (h, mut e, a) = with_a().await;
        let b = manifest(B, b_format, b_readable);
        h.publish(&b);
        (c.script)(&h.fake);
        let request = req(B);
        assert!(
            e.install(request.clone()).await.is_err(),
            "{label}: expected a crash"
        );
        drop(e);

        let mut on_disk = Journal::read(&h.state_dir).unwrap();
        let cur = on_disk.current.as_mut().expect("in flight after crash");
        let expected_step = if c.rollback_attempted_override.is_some() {
            Step::Probing
        } else {
            c.step
        };
        assert_eq!(cur.step, expected_step, "{label}: persisted step");
        if let Some(flag) = c.rollback_attempted_override {
            cur.step = c.step;
            cur.rollback_attempted = flag;
            let mut j = Journal::open(lock(&h.state_dir).unwrap()).unwrap();
            j.record(on_disk.clone()).unwrap();
        }
        if c.drop_history_line {
            let path = h.state_dir.join("history.jsonl");
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<&str> = text.lines().collect();
            let last: HistoryEntry = serde_json::from_str(lines.pop().unwrap()).unwrap();
            assert_eq!(last.request_id, Some(request.id), "{label}: dropped line");
            let rest: String = lines.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(&path, rest).unwrap();
        }
        let kept = c.recorded && !c.drop_history_line;
        let history_before = h.history().len();
        h.fake.clear_log();

        let mut e = h.engine();
        let out = match e.recover().await.unwrap() {
            Some(out) => {
                assert!(!kept, "{label}: outcome was already recorded");
                out
            }
            None => {
                assert!(kept, "{label}: no outcome");
                h.history()[0].clone()
            }
        };
        assert_eq!(out.outcome, c.outcome, "{label}: {}", out.detail);
        assert_eq!(out.request_id, Some(request.id), "{label}");
        assert_eq!(active_version(&e), Some(ver(c.active)), "{label}");
        if c.outcome != Outcome::FailedNeedsOperator {
            let active_image = e
                .journal_state()
                .active
                .as_ref()
                .unwrap()
                .images
                .server
                .clone();
            assert_eq!(
                h.fake.running(),
                Some(active_image),
                "{label}: active running"
            );
            let active_format = e.journal_state().active.as_ref().unwrap().state_format;
            assert_eq!(h.fake.db_format(), Some(active_format), "{label}: data");
        }
        // When recovery itself rolled back, check how.
        if c.outcome == Outcome::RolledBack && !kept {
            let log = h.fake.log();
            let start_a = h
                .fake
                .position(&format!("start_server {}", a.images.server))
                .unwrap_or_else(|| panic!("{label}: A not started: {log:?}"));
            if b_format != a.state_format {
                // B may have migrated the data: restore before starting A.
                let stop = h.fake.position("stop_server");
                let restore = h.fake.position("restore");
                assert!(
                    stop.is_some_and(|s| restore.is_some_and(|r| s < r && r < start_a)),
                    "{label}: expected stop, restore, start(A): {log:?}"
                );
            } else {
                assert_eq!(h.fake.count("restore"), 0, "{label}: {log:?}");
            }
        }
        assert_eq!(
            h.fake.maintenance_on(),
            c.maintenance_on,
            "{label}: maintenance"
        );
        assert_eq!(
            e.journal_state().current.is_some(),
            c.outcome == Outcome::FailedNeedsOperator,
            "{label}: in-flight cleared"
        );
        // Exactly one history entry per terminal outcome.
        let added = if kept { 0 } else { 1 };
        assert_eq!(h.history().len(), history_before + added, "{label}");
        assert_eq!(h.history()[0], out, "{label}");

        // A second restart is a no-op.
        drop(e);
        let mut e = h.engine();
        assert_eq!(e.recover().await.unwrap(), None, "{label}: second recover");
        assert_eq!(h.history().len(), history_before + added, "{label}");
        assert_eq!(h.fake.maintenance_on(), c.maintenance_on, "{label}");
    }
}

#[tokio::test]
async fn recover_after_crash_at_each_step() {
    run_crash_table(1, &[1]).await;
}

#[tokio::test]
async fn recover_after_crash_at_each_step_with_state_format_change() {
    run_crash_table(2, &[1, 2]).await;
}

#[tokio::test]
async fn recover_after_backup_crash_removes_partial_dump() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.crash("backup", 0);
    assert!(e.install(req(B)).await.is_err());
    let partial = Journal::read(&h.state_dir)
        .unwrap()
        .current
        .unwrap()
        .backup
        .unwrap();
    std::fs::write(&partial, b"torn").unwrap();
    drop(e);
    let mut e = h.engine();
    assert_eq!(
        e.recover().await.unwrap().unwrap().outcome,
        Outcome::Aborted
    );
    assert!(!partial.exists());
    assert!(e.journal_state().active_backup.as_ref().unwrap().exists());
}

#[tokio::test]
async fn recover_with_terminal_entry_already_recorded_does_not_duplicate() {
    // Crash between appending the outcome and clearing the journal.
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("pull", 0, "nope");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Aborted);
    drop(e);
    let mut state = Journal::read(&h.state_dir).unwrap();
    let target = manifest(B, 1, &[1]);
    state.current = Some(
        serde_json::from_value(serde_json::json!({
            "request_id": r.request_id.unwrap(),
            "target": target,
            "step": "pulled",
            "backup": null,
            "needs_restore_on_rollback": false,
            "started_at": r.started_at,
            "rollback_attempted": false
        }))
        .unwrap(),
    );
    Journal::open(lock(&h.state_dir).unwrap())
        .unwrap()
        .record(state)
        .unwrap();
    let n = h.history().len();
    h.fake.clear_log();
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.history().len(), n);
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.fake.count("start_server"), 0);
}

#[tokio::test]
async fn idle_recover_clears_stale_maintenance() {
    let (h, e, _a) = with_a().await;
    drop(e);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
    assert_eq!(h.fake.log(), vec!["leave".to_string()]);
}

#[tokio::test]
async fn restore_previous_requires_flag_when_state_format_differs() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    assert_eq!(h.fake.db_format(), Some(2));
    let a_backup = e.journal_state().active_backup.clone().unwrap();
    h.fake.clear_log();

    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(r.detail.contains("--restore-backup"), "{}", r.detail);
    assert!(r.detail.contains("lost"), "{}", r.detail);
    assert!(
        r.detail.contains(&a_backup.display().to_string()),
        "{}",
        r.detail
    );
    assert!(
        h.fake.log().is_empty(),
        "nothing stopped: {:?}",
        h.fake.log()
    );
    assert_eq!(active_version(&e), Some(ver(B)));

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(r.from, Some(ver(B)));
    assert_eq!(r.to, Some(ver(A)));
    let log = h.fake.log();
    let stop = h.fake.position("stop_server").unwrap();
    let restore = h
        .fake
        .position(&format!("restore {}", a_backup.display()))
        .unwrap();
    let start = h
        .fake
        .position(&format!("start_server {}", a.images.server))
        .unwrap();
    assert!(stop < restore && restore < start, "{log:?}");
    assert_eq!(h.fake.db_format(), Some(1));
    let s = e.journal_state();
    assert_eq!(s.active.as_ref().unwrap().version, ver(A));
    assert_eq!(s.previous.as_ref().unwrap().version, ver(B));
    assert!(s.active_backup.is_none(), "no backup to go forward again");
    assert!(!h.fake.maintenance_on());
    assert!(a_backup.exists(), "the restored backup is kept");

    // Going forward again needs a backup that no longer exists.
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
}

#[tokio::test]
async fn restore_previous_same_format_swaps_without_restore() {
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0);
    assert_eq!(h.fake.count("stop_server"), 0);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert_eq!(e.journal_state().previous.as_ref().unwrap().version, ver(B));
}

#[tokio::test]
async fn restore_previous_without_previous_refused() {
    let (h, mut e, _a) = with_a().await;
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused);
    assert!(h.fake.log().is_empty());
}

#[tokio::test]
async fn restore_previous_failure_rolls_back_to_active() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    h.fake.clear_log();
    h.fake.fail("wait_healthy", 0, "A unhealthy");
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(active_version(&e), Some(ver(B)));
    assert_eq!(h.fake.running().unwrap(), b.images.server);
    assert_eq!(
        h.fake.db_format(),
        Some(2),
        "B's data restored from the fresh backup"
    );
    assert!(
        e.journal_state().active_backup.is_some(),
        "unchanged on failure"
    );
}

#[tokio::test]
async fn backups_pruned_keeping_active_backup() {
    let h = H::new();
    std::fs::create_dir_all(&h.backup_dir).unwrap();
    for i in 0..4 {
        std::fs::write(
            h.backup_dir
                .join(format!("2020010{i}T000000Z-0.0.{i}-0.0.9.dump")),
            b"x",
        )
        .unwrap();
    }
    std::fs::write(h.backup_dir.join("notes.txt"), b"keep me").unwrap();
    let settings = EngineSettings {
        backups_keep: 2,
        ..h.settings()
    };
    let mut e = h.engine_with(settings);
    h.publish(&manifest(A, 1, &[1]));
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    let a_backup = e.journal_state().active_backup.clone().unwrap();
    assert_eq!(h.dumps().len(), 2);
    // B fails: its backup is the newest, A's is protected as active_backup.
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("wait_healthy", 1, "no");
    assert_eq!(
        e.install(req(B)).await.unwrap().outcome,
        Outcome::RolledBack
    );
    h.publish(&manifest("1.2.0", 1, &[1]));
    assert_eq!(
        e.install(req("1.2.0")).await.unwrap().outcome,
        Outcome::Succeeded
    );
    let dumps = h.dumps();
    assert!(dumps.contains(&a_backup), "{dumps:?}");
    assert!(dumps.contains(e.journal_state().active_backup.as_ref().unwrap()));
    assert!(
        !dumps.iter().any(|d| d.to_str().unwrap().contains("2020")),
        "{dumps:?}"
    );
    assert!(h.backup_dir.join("notes.txt").exists());
}

#[tokio::test]
async fn set_available_appears_in_status() {
    let (_h, mut e, a) = with_a().await;
    assert!(e.status().available.is_none());
    let summary = e.status().active.unwrap();
    e.set_available(Some(summary.clone()));
    assert_eq!(e.status().available, Some(summary));
    assert_eq!(e.status().active.unwrap().server_image, a.images.server);
}

#[test]
fn install_future_is_send() {
    fn assert_send<T: Send>(_: T) {}
    let h = H::new();
    let mut e = h.engine();
    assert_send(e.install(req(A)));
    assert_send(e.recover());
    assert_send(e.restore_previous(false));
}

#[tokio::test]
async fn auth_mode_must_match_settings() {
    // Matching mode passes.
    let h = H::new();
    h.publish(&manifest(A, 1, &[1]));
    h.fake.set_auth_mode("dev");
    let settings = EngineSettings {
        expected_auth_mode: "dev".into(),
        ..h.settings()
    };
    let mut e = h.engine_with(settings);
    let r = e.install(req(A)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);

    // B comes up with dev auth where Access is expected: rolled back.
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_report(
        &b.images.server,
        ReadinessReport {
            version: ver(B),
            source_revision: b.source_revision.clone(),
            state_format: 1,
            protocol_min: 1,
            protocol_max: 1,
            database: "ok".into(),
            auth_mode: "dev".into(),
        },
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert!(r.detail.contains("auth_mode"), "{}", r.detail);
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn unknown_recorded_outcome_holds_for_operator() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.crash("wait_healthy", 0);
    let request = req(B);
    assert!(e.install(request.clone()).await.is_err());
    drop(e);
    // A newer updater recorded an outcome this one does not know.
    let line = serde_json::json!({
        "request_id": request.id,
        "from": A,
        "to": B,
        "outcome": "some_future_outcome",
        "detail": "?",
        "started_at": Utc::now(),
        "finished_at": Utc::now()
    });
    let path = h.state_dir.join("history.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&path, text).unwrap();
    h.fake.clear_log();

    let mut e = h.engine();
    let out = e.recover().await.unwrap().unwrap();
    assert_eq!(out.outcome, Outcome::FailedNeedsOperator, "{}", out.detail);
    assert_eq!(h.fake.log(), vec!["enter".to_string()], "no runtime action");
    assert!(h.fake.maintenance_on());
    let cur = e.journal_state().current.clone().expect("current kept");
    assert!(cur.rollback_attempted);
    assert!(matches!(
        e.status().state,
        UpdateState::FailedNeedsOperator { .. }
    ));
    drop(e);
    let mut e = h.engine();
    assert_eq!(e.recover().await.unwrap(), None);
}

/// Crash points inside `restore_previous(true)` from B (format 2) back to A
/// (format 1); recovery must leave a consistent release and its data.
#[tokio::test]
async fn restore_previous_recovers_after_crash() {
    struct Case {
        name: &'static str,
        script: fn(&FakeRuntime),
        step: Step,
        outcome: Outcome,
        active: &'static str,
    }
    let cases = [
        Case {
            name: "after backup started",
            script: |f| f.crash("backup", 0),
            step: Step::BackedUp,
            outcome: Outcome::Aborted,
            active: B,
        },
        Case {
            name: "after restore(active_backup), before start_server(A)",
            script: |f| f.crash("start_server", 0),
            step: Step::Replacing,
            outcome: Outcome::RolledBack,
            active: B,
        },
        Case {
            name: "while probing A",
            script: |f| f.crash("wait_healthy", 0),
            step: Step::Probing,
            outcome: Outcome::RolledBack,
            active: B,
        },
        Case {
            name: "after the swap",
            script: |f| f.crash("leave", 0),
            step: Step::Active,
            outcome: Outcome::Succeeded,
            active: A,
        },
    ];
    for c in cases {
        let (h, mut e, a) = with_a().await;
        let b = manifest(B, 2, &[1, 2]);
        h.publish(&b);
        assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
        let a_backup = e.journal_state().active_backup.clone().unwrap();
        h.fake.clear_log();
        (c.script)(&h.fake);
        assert!(e.restore_previous(true).await.is_err(), "{}: crash", c.name);
        if c.step == Step::Replacing {
            assert!(
                h.fake
                    .position(&format!("restore {}", a_backup.display()))
                    .is_some()
            );
            assert_eq!(h.fake.db_format(), Some(1), "{}: A's data restored", c.name);
        }
        drop(e);
        let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
        assert_eq!(cur.step, c.step, "{}", c.name);

        h.fake.clear_log();
        let mut e = h.engine();
        // A crash in leave() comes after the outcome line: nothing to redo.
        let out = match e.recover().await.unwrap() {
            Some(out) => out,
            None if c.step == Step::Active => h.history()[0].clone(),
            None => panic!("{}: no outcome", c.name),
        };
        assert_eq!(out.outcome, c.outcome, "{}: {}", c.name, out.detail);
        assert_eq!(active_version(&e), Some(ver(c.active)), "{}", c.name);
        let (image, format) = if c.active == A {
            (&a.images.server, 1)
        } else {
            (&b.images.server, 2)
        };
        assert_eq!(h.fake.running().as_ref(), Some(image), "{}", c.name);
        assert_eq!(h.fake.db_format(), Some(format), "{}: data", c.name);
        assert!(!h.fake.maintenance_on(), "{}", c.name);
        assert!(e.journal_state().current.is_none(), "{}", c.name);
        if c.outcome == Outcome::RolledBack {
            // B's data comes back from the fresh pre-restore backup.
            let log = h.fake.log();
            let stop = h.fake.position("stop_server").unwrap();
            let restore = h.fake.position("restore").unwrap();
            let start_b = h
                .fake
                .position(&format!("start_server {}", b.images.server))
                .unwrap();
            assert!(stop < restore && restore < start_b, "{}: {log:?}", c.name);
            assert_ne!(log[restore], format!("restore {}", a_backup.display()));
            assert_eq!(e.journal_state().active_backup.as_ref(), Some(&a_backup));
        }
        if c.outcome == Outcome::Succeeded {
            assert!(e.journal_state().active_backup.is_none(), "{}", c.name);
        }
    }
}

#[tokio::test]
async fn first_install_recovers_after_crash() {
    struct Case {
        script: fn(&FakeRuntime),
        step: Step,
        outcome: Outcome,
        installed: bool,
    }
    let cases = [
        Case {
            script: |f| f.crash("pull", 0),
            step: Step::Pulled,
            outcome: Outcome::Aborted,
            installed: false,
        },
        Case {
            script: |f| f.crash("wait_healthy", 0),
            step: Step::Probing,
            outcome: Outcome::Aborted,
            installed: false,
        },
        Case {
            script: |f| f.crash("leave", 0),
            step: Step::Active,
            outcome: Outcome::Succeeded,
            installed: true,
        },
    ];
    for c in cases {
        let h = H::new();
        let a = manifest(A, 1, &[1]);
        h.publish(&a);
        (c.script)(&h.fake);
        let mut e = h.engine();
        assert!(e.install(req(A)).await.is_err());
        drop(e);
        assert_eq!(
            Journal::read(&h.state_dir).unwrap().current.unwrap().step,
            c.step
        );
        h.fake.clear_log();
        let mut e = h.engine();
        let out = match e.recover().await.unwrap() {
            Some(out) => out,
            None if c.step == Step::Active => h.history()[0].clone(),
            None => panic!("{:?}: no outcome", c.step),
        };
        assert_eq!(out.outcome, c.outcome, "{:?}: {}", c.step, out.detail);
        assert_eq!(
            e.journal_state().active.is_some(),
            c.installed,
            "{:?}",
            c.step
        );
        assert!(e.journal_state().current.is_none());
        assert!(!h.fake.maintenance_on());
        if c.installed {
            assert_eq!(h.fake.running().unwrap(), a.images.server);
        } else {
            // Nothing to roll back to: no release left running, none started.
            assert!(h.fake.running().is_none(), "{:?}", c.step);
            assert_eq!(h.fake.count("start_server"), 0, "{:?}", c.step);
        }
    }
}

#[tokio::test]
async fn restore_previous_refuses_missing_backup_file_without_downtime() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    let backup = e.journal_state().active_backup.clone().unwrap();
    std::fs::remove_file(&backup).unwrap();
    h.fake.clear_log();
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("missing"), "{}", r.detail);
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert!(e.journal_state().current.is_none());
}

#[tokio::test]
async fn restore_previous_refuses_unreadable_current_data() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    // Same manifest format, but the database says 3, which A cannot read.
    h.fake.set_db_format(Some(3));
    h.fake.clear_log();
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("state format 3"), "{}", r.detail);
    assert_eq!(h.fake.log(), vec!["server_state_format".to_string()]);
}

/// How history looks when recovery finds the journal at `Active`.
#[derive(Debug, Clone, Copy)]
enum BadHistory {
    /// The finished operation's line carries an outcome this updater
    /// does not know.
    Unknown,
    /// history.jsonl cannot be read (nor appended to).
    Unreadable,
}

fn spoil_history(h: &H, how: BadHistory, request_id: Uuid) {
    let path = h.state_dir.join("history.jsonl");
    match how {
        BadHistory::Unknown => {
            let text = std::fs::read_to_string(&path).unwrap();
            let mut lines: Vec<String> = text.lines().map(String::from).collect();
            let mut last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
            assert_eq!(last["request_id"], serde_json::json!(request_id));
            last["outcome"] = "some_future_outcome".into();
            *lines.last_mut().unwrap() = last.to_string();
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        }
        BadHistory::Unreadable => {
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
        }
    }
}

/// Crash in the final leave() of an install of B (format 2, so its
/// in-flight backup holds format-1 data) or of restore_previous back to A
/// (whose in-flight backup holds B's format-2 data). The journal is at
/// `Active` either way. Returns the harness, the expected active release
/// and the in-flight request id.
async fn crash_at_active(restore_mode: bool) -> (H, ReleaseManifest, Uuid) {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    if restore_mode {
        assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
        h.fake.clear_log();
        h.fake.crash("leave", 0);
        assert!(e.restore_previous(true).await.is_err());
    } else {
        h.fake.crash("leave", 0);
        assert!(e.install(req(B)).await.is_err());
    }
    drop(e);
    let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
    assert_eq!(cur.step, Step::Active);
    assert!(cur.needs_restore_on_rollback && cur.backup.is_some());
    let expected = if restore_mode { a } else { b };
    assert!(h.fake.maintenance_on());
    (h, expected, cur.request_id)
}

#[tokio::test]
async fn active_step_finishes_from_the_journal_whatever_history_says() {
    for restore_mode in [false, true] {
        for how in [BadHistory::Unknown, BadHistory::Unreadable] {
            let label = format!("restore_mode {restore_mode}, {how:?}");
            let (h, expected, id) = crash_at_active(restore_mode).await;
            spoil_history(&h, how, id);
            h.fake.clear_log();

            let mut e = h.engine();
            let out = e.recover().await.unwrap().expect("an outcome");
            assert_eq!(out.outcome, Outcome::Succeeded, "{label}: {}", out.detail);
            assert_eq!(out.request_id, Some(id), "{label}");
            assert_eq!(h.fake.count("restore"), 0, "{label}: {:?}", h.fake.log());
            assert_eq!(h.fake.count("start_server"), 0, "{label}");
            assert_eq!(
                active_version(&e),
                Some(expected.version.clone()),
                "{label}"
            );
            assert_eq!(
                h.fake.running().as_ref(),
                Some(&expected.images.server),
                "{label}"
            );
            assert_eq!(h.fake.db_format(), Some(expected.state_format), "{label}");
            assert!(!h.fake.maintenance_on(), "{label}");
            assert!(e.journal_state().current.is_none(), "{label}");
            if let BadHistory::Unknown = how {
                assert_eq!(h.history()[0], out, "{label}: outcome recorded");
            }
        }
    }
}

#[tokio::test]
async fn retry_active_never_restores_at_active_step() {
    for restore_mode in [false, true] {
        let label = format!("restore_mode {restore_mode}");
        let (h, expected, _id) = crash_at_active(restore_mode).await;
        // The state an older build could leave: Active held for the operator.
        let mut state = Journal::read(&h.state_dir).unwrap();
        state.current.as_mut().unwrap().rollback_attempted = true;
        Journal::open(lock(&h.state_dir).unwrap())
            .unwrap()
            .record(state)
            .unwrap();
        h.fake.clear_log();

        let mut e = h.engine();
        let r = e.restore_previous(true).await.unwrap();
        assert_eq!(r.outcome, Outcome::RolledBack, "{label}: {}", r.detail);
        assert_eq!(h.fake.count("restore"), 0, "{label}: {:?}", h.fake.log());
        assert_eq!(h.fake.count("stop_server"), 0, "{label}");
        assert_eq!(
            active_version(&e),
            Some(expected.version.clone()),
            "{label}"
        );
        assert_eq!(
            h.fake.running().as_ref(),
            Some(&expected.images.server),
            "{label}"
        );
        assert_eq!(h.fake.db_format(), Some(expected.state_format), "{label}");
        assert!(!h.fake.maintenance_on(), "{label}");
        assert!(e.journal_state().current.is_none(), "{label}");
    }
}

#[tokio::test]
async fn observer_sees_every_step_of_an_install() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        sink.lock().unwrap().push(s.clone());
    }));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    let seen = seen.lock().unwrap().clone();
    let steps: Vec<Step> = seen
        .iter()
        .filter_map(|s| match &s.state {
            UpdateState::InProgress { step, target, .. } => {
                assert_eq!(*target, Some(ver(B)));
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert!(steps.windows(2).all(|w| w[0] <= w[1]), "{steps:?}");
    for want in [
        Step::Requested,
        Step::Validated,
        Step::Pulled,
        Step::Maintenance,
        Step::BackedUp,
        Step::Replacing,
        Step::Probing,
        Step::Active,
    ] {
        assert!(steps.contains(&want), "{want:?} not observed in {steps:?}");
    }
    let last = seen.last().unwrap();
    assert_eq!(last.state, UpdateState::Idle);
    assert_eq!(last.active.as_ref().unwrap().version, ver(B));
    assert_eq!(last.history[0], r);
    // A rejection (history only) is observed too.
    let before = seen.len();
    let n = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let c = n.clone();
    e.set_observer(Box::new(move |_| *c.lock().unwrap() += 1));
    e.reject(None, None, "bad request".into()).unwrap();
    assert_eq!(*n.lock().unwrap(), 1);
    assert!(before > 8);
}

#[tokio::test]
async fn refresh_available_sets_only_newer_compatible_releases() {
    let (h, mut e, a) = with_a().await;
    // Channel on the active release: nothing available.
    h.fake.set_channel(a.version.clone());
    assert_eq!(e.refresh_available().await, Ok(None));
    assert!(e.status().available.is_none());
    // Newer and compatible.
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    let got = e.refresh_available().await.unwrap().unwrap();
    assert_eq!(got.version, ver(B));
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Fetch failure: unchanged.
    h.fake.fail(
        "fetch_release",
        h.fake.count("fetch_release"),
        "registry down",
    );
    assert!(e.refresh_available().await.is_err());
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Bad signature: unchanged.
    let c = manifest("1.2.0", 1, &[1]);
    h.publish_untrusted(&c);
    h.fake.set_channel(ver("1.2.0"));
    assert!(e.refresh_available().await.is_err());
    assert_eq!(e.status().available.unwrap().version, ver(B));
    // Newer but cannot read the installed data: not available.
    let d = manifest("1.3.0", 3, &[3]);
    h.publish(&d);
    h.fake.set_channel(ver("1.3.0"));
    assert_eq!(e.refresh_available().await, Ok(None));
    assert!(e.status().available.is_none());
    // Polling never touches the running release.
    assert_eq!(h.fake.count("start_server"), 0);
    assert_eq!(h.fake.count("pull"), 0);
}

#[tokio::test]
async fn sync_maintenance_follows_the_journal() {
    let (h, e, _a) = with_a().await;
    h.fake.set_maintenance(true);
    e.sync_maintenance().unwrap();
    assert!(!h.fake.maintenance_on(), "idle: marker removed");
}

#[tokio::test]
async fn reaching_active_clears_an_available_release_it_covers() {
    let (h, mut e, _a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_channel(ver(B));
    assert_eq!(
        e.refresh_available().await.unwrap().unwrap().version,
        ver(B)
    );
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert!(
        e.status().available.is_none(),
        "B is installed, not available"
    );

    // A newer available release survives an install of an older one.
    let (h, mut e, _a) = with_a().await;
    let c = manifest("1.2.0", 1, &[1]);
    h.publish(&c);
    h.publish(&b);
    h.fake.set_channel(ver("1.2.0"));
    e.refresh_available().await.unwrap();
    e.install(req(B)).await.unwrap();
    assert_eq!(e.status().available.unwrap().version, ver("1.2.0"));
}

#[tokio::test]
async fn seeded_available_must_be_newer_than_active() {
    let (_h, mut e, a) = with_a().await;
    let mut s = e.status().active.unwrap();
    e.seed_available(Some(s.clone()));
    assert!(e.status().available.is_none(), "same as active: dropped");
    s.version = ver("0.9.0");
    e.seed_available(Some(s.clone()));
    assert!(e.status().available.is_none(), "older than active: dropped");
    s.version = ver(B);
    e.seed_available(Some(s.clone()));
    assert_eq!(e.status().available.unwrap().version, ver(B));
    assert_eq!(
        e.journal_state().active.as_ref().unwrap().version,
        a.version
    );
}

/// B changes the data without changing its state format (a migration added
/// without a format bump): the rollback starts A without a restore, A cannot
/// read the data, and the update ends in `FailedNeedsOperator`.
async fn needs_operator_after_unflagged_migration() -> (H, TestEngine, ReleaseManifest) {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    h.fake.set_start_migrates(&b.images.server, 2);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    let cur = e.journal_state().current.clone().unwrap();
    assert!(!cur.needs_restore_on_rollback, "the formats say no restore");
    assert!(cur.backup.as_ref().is_some_and(|b| b.exists()));
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.db_format(), Some(2));
    h.fake.clear_log();
    (h, e, a)
}

#[tokio::test]
async fn restore_backup_flag_forces_the_in_flight_restore() {
    let (h, mut e, a) = needs_operator_after_unflagged_migration().await;
    let backup = e.journal_state().current.clone().unwrap().backup.unwrap();

    // Without the flag the retry brings A back on the data as it is, and
    // fails again the same way.
    let r = e.restore_previous(false).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    h.fake.clear_log();

    // With it, the recorded in-flight backup is restored first.
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    let log = h.fake.log();
    let pos = |p: &str| log.iter().position(|c| c.starts_with(p)).unwrap();
    assert_eq!(
        h.fake.count(&format!("restore {}", backup.display())),
        1,
        "{log:?}"
    );
    assert!(pos("select_release") < pos("stop_server"), "{log:?}");
    assert!(pos("stop_server") < pos("restore"), "{log:?}");
    assert!(pos("restore") < pos("start_server"), "{log:?}");
    assert_eq!(h.fake.db_format(), Some(1));
    assert_eq!(h.fake.running().unwrap(), a.images.server);
    assert_eq!(active_version(&e), Some(ver(A)));
    assert!(e.journal_state().current.is_none());
    assert!(!h.fake.maintenance_on());
}

#[tokio::test]
async fn restore_backup_flag_needs_the_backup_file() {
    let (h, mut e, _a) = needs_operator_after_unflagged_migration().await;
    let backup = e.journal_state().current.clone().unwrap().backup.unwrap();
    std::fs::remove_file(&backup).unwrap();
    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(r.detail.contains("missing"), "{}", r.detail);
    assert!(h.fake.log().is_empty(), "{:?}", h.fake.log());
    assert!(e.journal_state().current.is_some());
}

#[tokio::test]
async fn restore_backup_flag_never_restores_before_the_target_ran() {
    // Held for the operator at `BackedUp` (history holds an outcome this
    // updater does not know): the target never started and the dump may be
    // partial, so the flag restores nothing.
    let (h, mut e, a) = with_a().await;
    h.publish(&manifest(B, 2, &[1, 2]));
    h.fake.crash("backup", 0);
    let request = req(B);
    assert!(e.install(request.clone()).await.is_err());
    drop(e);
    let cur = Journal::read(&h.state_dir).unwrap().current.unwrap();
    assert_eq!(cur.step, Step::BackedUp);
    assert!(cur.needs_restore_on_rollback);
    // A (partial) dump file at the recorded path.
    std::fs::write(cur.backup.as_ref().unwrap(), b"fmt:1\npartial").unwrap();
    let line = serde_json::json!({
        "request_id": request.id,
        "from": A,
        "to": B,
        "outcome": "some_future_outcome",
        "detail": "?",
        "started_at": Utc::now(),
        "finished_at": Utc::now()
    });
    let path = h.state_dir.join("history.jsonl");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}{line}\n")).unwrap();
    let mut e = h.engine();
    let out = e.recover().await.unwrap().unwrap();
    assert_eq!(out.outcome, Outcome::FailedNeedsOperator, "{}", out.detail);
    let cur = e.journal_state().current.clone().unwrap();
    assert_eq!(cur.step, Step::BackedUp);
    assert!(
        !out.detail.contains("--restore-backup"),
        "no restore is offered: {}",
        out.detail
    );
    h.fake.clear_log();

    let r = e.restore_previous(true).await.unwrap();
    assert_eq!(r.outcome, Outcome::RolledBack, "{}", r.detail);
    assert_eq!(h.fake.count("restore"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.count("stop_server"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.running().unwrap(), a.images.server);
}

#[tokio::test]
async fn rollback_restore_selects_the_active_release_before_stopping() {
    let (h, mut e, a) = with_a().await;
    let b = manifest(B, 2, &[1, 2]);
    h.publish(&b);
    h.fake.fail("wait_healthy", 0, "B unhealthy");
    h.fake.fail("restore", 0, "pg_restore failed");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::FailedNeedsOperator, "{}", r.detail);
    // B was started (and selected), then the rollback selected A again
    // before stopping B, so a manual `compose up` now starts A.
    let select_a = format!("select_release {}", a.images.server);
    let log = h.fake.log();
    let sel = log.iter().position(|c| *c == select_a).expect("A selected");
    let start_b = log
        .iter()
        .position(|c| *c == format!("start_server {}", b.images.server))
        .unwrap();
    let stop = log.iter().position(|c| c == "stop_server").unwrap();
    assert!(start_b < sel && sel < stop, "{log:?}");
    assert_eq!(h.fake.selected().as_deref(), Some(a.images.server.as_str()));
    assert!(h.fake.running().is_none());
}

#[tokio::test]
async fn early_step_recovery_leaves_a_healthy_active_release_running() {
    for healthy in [true, false] {
        let (h, mut e, a) = with_a().await;
        h.publish(&manifest(B, 1, &[1]));
        h.fake.crash("pull", 0);
        assert!(e.install(req(B)).await.is_err());
        drop(e);
        assert_eq!(
            Journal::read(&h.state_dir).unwrap().current.unwrap().step,
            Step::Pulled
        );
        h.fake.clear_log();
        if !healthy {
            h.fake.fail("wait_healthy", 0, "A unhealthy");
        }
        let mut e = h.engine();
        let out = e.recover().await.unwrap().unwrap();
        assert_eq!(out.outcome, Outcome::Aborted, "{}", out.detail);
        assert!(
            out.detail.contains(&format!("{A} is running")),
            "{}",
            out.detail
        );
        let starts = h.fake.count("start_server");
        if healthy {
            assert_eq!(starts, 0, "A was recreated: {:?}", h.fake.log());
            assert_eq!(h.fake.count("wait_healthy"), 1);
        } else {
            assert_eq!(starts, 1, "{:?}", h.fake.log());
            let log = h.fake.log();
            let first_probe = log.iter().position(|c| c == "wait_healthy").unwrap();
            let start = log
                .iter()
                .position(|c| c.starts_with("start_server"))
                .unwrap();
            assert!(first_probe < start, "{log:?}");
        }
        assert_eq!(h.fake.running().unwrap(), a.images.server);
    }
}

#[tokio::test]
async fn taken_request_shows_in_progress_before_the_fetch() {
    let (h, mut e, _a) = with_a().await;
    let fake = h.fake.clone();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        sink.lock()
            .unwrap()
            .push((s.state.clone(), fake.count("fetch_release")));
    }));

    // A version request that is then refused (nothing published).
    let request = req(B);
    let r = e.install(request.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        match &seen[0] {
            (
                UpdateState::InProgress {
                    request_id,
                    target,
                    step,
                    ..
                },
                0,
            ) => {
                assert_eq!(*request_id, request.id);
                assert_eq!(*target, Some(ver(B)));
                assert_eq!(*step, Step::Requested);
            }
            other => panic!("expected InProgress before any fetch, got {other:?}"),
        }
        assert_eq!(
            seen[1],
            (UpdateState::Idle, 1),
            "idle again after the refusal"
        );
    }
    assert!(e.journal_state().current.is_none());

    // A channel request: the target is not known yet.
    seen.lock().unwrap().clear();
    let mut channel = req(B);
    channel.version = None;
    let r = e.install(channel.clone()).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    let seen = seen.lock().unwrap();
    assert!(
        matches!(
            &seen[0],
            (UpdateState::InProgress { request_id, target: None, step: Step::Requested, .. }, _)
                if *request_id == channel.id
        ),
        "{seen:?}"
    );
    assert_eq!(seen.last().unwrap().0, UpdateState::Idle);
}

#[tokio::test]
async fn backup_waits_for_the_maintenance_drain() {
    let drain = Duration::from_millis(400);
    let (h, _e, _a) = with_a().await;
    drop(_e);
    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    let mut e = h.engine_with(EngineSettings {
        maintenance_drain: drain,
        ..h.settings()
    });
    let fake = h.fake.clone();
    let marks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = marks.clone();
    e.set_observer(Box::new(move |s: &aq_release::UpdateStatus| {
        if let UpdateState::InProgress { step, .. } = s.state {
            sink.lock().unwrap().push((
                step,
                std::time::Instant::now(),
                fake.maintenance_on(),
                fake.count("backup"),
            ));
        }
    }));
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    let marks = marks.lock().unwrap();
    let at = |want: Step| marks.iter().find(|m| m.0 == want).cloned().unwrap();
    let (_, entered, on_before, _) = at(Step::Maintenance);
    let (_, backed_up, on, backups) = at(Step::BackedUp);
    assert!(!on_before, "Maintenance is persisted before the marker");
    assert!(on, "the marker is on before the backup step");
    assert_eq!(backups, 0, "the dump starts after the drain");
    assert!(
        backed_up.duration_since(entered) >= drain,
        "{:?}",
        backed_up.duration_since(entered)
    );
}

// ------------------------------------------- storage budget (DEP7c, DEP8)

#[tokio::test]
async fn backup_rotation_keeps_five_by_default() {
    let h = H::new();
    std::fs::create_dir_all(&h.backup_dir).unwrap();
    for i in 0..7 {
        std::fs::write(
            h.backup_dir
                .join(format!("2020010{i}T000000Z-0.0.{i}-0.0.9.dump")),
            b"x",
        )
        .unwrap();
    }
    let settings = h.settings();
    assert_eq!(settings.backups_keep, 5);
    let mut e = h.engine_with(settings);
    h.publish(&manifest(A, 1, &[1]));
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    let dumps = h.dumps();
    assert_eq!(dumps.len(), 5, "{dumps:?}");
    // The new backup and the four newest old ones survive.
    assert!(dumps.contains(e.journal_state().active_backup.as_ref().unwrap()));
    for i in 0..3 {
        let old = format!("2020010{i}T");
        assert!(
            !dumps.iter().any(|d| d.to_str().unwrap().contains(&old)),
            "{dumps:?}"
        );
    }
}

#[test]
fn backup_space_check_needs_newest_plus_ten_percent() {
    use aq_updater::engine::{InsufficientBackupSpace, backup_space_needed, check_backup_space};
    let dir = std::path::Path::new("/var/lib/agent-q/backups");
    assert_eq!(backup_space_needed(0), 0);
    assert_eq!(backup_space_needed(1000), 1100);
    assert_eq!(backup_space_needed(1001), 1102, "10% rounds up");
    assert_eq!(backup_space_needed(u64::MAX), u64::MAX);
    assert_eq!(check_backup_space(dir, 1100, 1000), Ok(()));
    assert_eq!(check_backup_space(dir, 0, 0), Ok(()));
    let err = check_backup_space(dir, 1099, 1000).unwrap_err();
    assert_eq!(
        err,
        InsufficientBackupSpace {
            dir: dir.to_path_buf(),
            free: 1099,
            newest: 1000,
            need: 1100,
        }
    );
    assert!(err.to_string().contains("plus 10%"), "{err}");
}

#[tokio::test]
async fn backup_refused_when_free_space_below_newest_backup_plus_ten_percent() {
    let h = H::new();
    let a = manifest(A, 1, &[1]);
    h.publish(&a);
    let settings = EngineSettings {
        min_free_bytes: 10,
        ..h.settings()
    };
    h.fake.set_backup_size(1000);
    let mut e = h.engine_with(settings);
    assert_eq!(e.install(req(A)).await.unwrap().outcome, Outcome::Succeeded);
    h.publish(&manifest(B, 1, &[1]));
    h.fake.clear_log();
    // Enough at validation; the pull then leaves 1099 bytes, below the
    // 1000-byte newest backup plus 10%.
    h.fake.set_free_seq(&[1_000_000, 1099]);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Refused, "{}", r.detail);
    assert!(
        r.detail.contains("1099 bytes free, 1100 needed"),
        "{}",
        r.detail
    );
    assert_eq!(h.fake.count("pull"), 1);
    assert_eq!(h.fake.count("backup"), 0, "{:?}", h.fake.log());
    assert_eq!(h.fake.count("enter"), 0, "nothing stopped");
    assert_eq!(h.fake.running().as_deref(), Some(a.images.server.as_str()));
    assert!(e.journal_state().current.is_none());
    assert_eq!(h.dumps().len(), 1);

    // Exactly the newest backup plus 10% is enough.
    h.fake.set_free_seq(&[1_000_000, 1100]);
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
}

#[test]
fn image_prune_selects_all_but_current_and_previous() {
    use aq_updater::engine::{image_repository, images_to_prune};
    let img = |d: char| {
        format!(
            "ghcr.io/electricjack/agent-q-server@sha256:{}",
            d.to_string().repeat(64)
        )
    };
    let (old, prev, cur) = (img('a'), img('b'), img('c'));
    assert_eq!(
        image_repository(&cur),
        Some("ghcr.io/electricjack/agent-q-server")
    );
    let present = vec![old.clone(), prev.clone(), cur.clone(), old.clone()];
    let keep = vec![cur.clone(), prev.clone()];
    assert_eq!(images_to_prune(&present, &keep), vec![old]);
    assert!(images_to_prune(&keep, &keep).is_empty());
}

#[tokio::test]
async fn image_prune_after_update_keeps_current_and_previous_release() {
    let (h, mut e, a) = with_a().await;
    let stale = "ghcr.io/electricjack/agent-q-server@sha256:9999999999999999999999999999999999999999999999999999999999999999";
    let postgres = "docker.io/library/postgres@sha256:8888888888888888888888888888888888888888888888888888888888888888";
    h.fake.add_images(&[stale, postgres]);

    let b = manifest(B, 1, &[1]);
    h.publish(&b);
    assert_eq!(e.install(req(B)).await.unwrap().outcome, Outcome::Succeeded);
    let images = h.fake.images();
    assert!(!images.iter().any(|i| i == stale), "{images:?}");
    for kept in [&a.images.server, &b.images.server] {
        assert!(images.contains(kept), "{kept}: {images:?}");
    }
    assert!(
        images.iter().any(|i| i == postgres),
        "other repositories untouched"
    );

    let c = manifest("1.2.0", 1, &[1]);
    h.publish(&c);
    assert_eq!(
        e.install(req("1.2.0")).await.unwrap().outcome,
        Outcome::Succeeded
    );
    let images = h.fake.images();
    assert!(!images.contains(&a.images.server), "{images:?}");
    assert!(images.contains(&b.images.server) && images.contains(&c.images.server));
}

#[tokio::test]
async fn image_prune_failure_does_not_change_the_outcome() {
    let (h, mut e, _a) = with_a().await;
    h.publish(&manifest(B, 1, &[1]));
    h.fake.fail("list_images", 0, "docker down");
    let r = e.install(req(B)).await.unwrap();
    assert_eq!(r.outcome, Outcome::Succeeded, "{}", r.detail);
    assert_eq!(active_version(&e), Some(ver(B)));
}
