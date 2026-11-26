//! Update engine against `FakeRuntime`. Signed with a `TestKey` generated
//! here, so this file needs `--features test-verifier`.
#![cfg(feature = "test-verifier")]

mod common;

#[path = "engine/active_recovery.rs"]
mod active_recovery;
#[path = "engine/backup.rs"]
mod backup;
#[path = "engine/crash_table.rs"]
mod crash_table;
#[path = "engine/images.rs"]
mod images;
#[path = "engine/operator.rs"]
mod operator;
#[path = "engine/recovery.rs"]
mod recovery;
#[path = "engine/restore.rs"]
mod restore;
#[path = "engine/rollback.rs"]
mod rollback;
#[path = "engine/status.rs"]
mod status;
#[path = "engine/success.rs"]
mod success;
#[path = "engine/validation.rs"]
mod validation;

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

fn assert_no_calls(runtime: &FakeRuntime, actions: &[&str]) {
    for action in actions {
        assert_eq!(runtime.count(action), 0, "{action}: {:?}", runtime.log());
    }
}

fn assert_release_running(harness: &H, engine: &TestEngine, release: &ReleaseManifest) {
    assert_eq!(active_version(engine).as_ref(), Some(&release.version));
    assert_eq!(
        harness.fake.running().as_deref(),
        Some(release.images.server.as_str())
    );
}
