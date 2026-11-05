#![cfg(feature = "test-support")]
use aq_session_host::{capsule::*, ownership};
use aq_session_protocol::{ArtifactRef, Digest, Text};
use std::{
    fs::File,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
const _: () = assert!(!PRODUCTION_ELIGIBLE);
#[test]
#[ignore = "explicit rootless FD-root and real submount canary; no agent/provider"]
fn pinned_root_survives_path_replacement_and_real_submount_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::set_permissions(scratch.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aq-session-capsule-candidate"))
        .arg("probe-root-mount")
        .arg(scratch.path())
        .env_clear()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_dir(scratch.path().join("root"))
            .unwrap()
            .next()
            .is_none()
    );
    assert!(
        std::fs::read_dir(scratch.path().join("moved-root"))
            .unwrap()
            .next()
            .is_none()
    );
}
struct Fixture {
    _root: tempfile::TempDir,
    plan: StaticCapsulePlan,
    helper: PinnedHelper,
    workspace: PathBuf,
    _original: File,
}
fn reference(id: &str, path: &Path) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(path).unwrap()),
    }
}
fn fixture_with_lifetime(lifetime_ms: u64) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "root",
        "workspace",
        "auth",
        "allowed",
        "git",
        "private-node",
    ] {
        let path = root.path().join(name);
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let program = root.path().join("peer");
    assert!(
        std::process::Command::new("/usr/bin/cc")
            .args(["-static", "-O2", "-Wall", "-Wextra"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/capsule-static-peer.c"))
            .arg("-o")
            .arg(&program)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        root.path().join("auth/auth.json"),
        b"derived-local-canary\n",
    )
    .unwrap();
    std::fs::write(root.path().join("allowed/item"), b"allowed-canary\n").unwrap();
    std::fs::write(root.path().join("git/item"), b"git-canary\n").unwrap();
    let source = root.path().join("private-node/original");
    std::fs::write(&source, uuid::Uuid::new_v4().as_bytes()).unwrap();
    let original = File::open(&source).unwrap();
    let metadata = original.metadata().unwrap();
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_aq-session-capsule-candidate"));
    let helper = PinnedHelper::open(
        &std::fs::canonicalize(&helper).unwrap(),
        &reference("source-helper", &helper),
    )
    .unwrap();
    let workspace = root.path().join("workspace");
    let grants = vec![
        MountGrant::open(&workspace, MountTarget::Workspace, true).unwrap(),
        MountGrant::open(&root.path().join("auth"), MountTarget::DerivedAuth, false).unwrap(),
        MountGrant::open(&root.path().join("allowed"), MountTarget::Allowed(0), false).unwrap(),
        MountGrant::open(&root.path().join("git"), MountTarget::GitMetadata, false).unwrap(),
    ];
    let tids = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>()
        .join(",");
    let plan = StaticCapsulePlan::new(
        &root.path().join("root"),
        &program,
        &reference("static-peer", &program),
        b"capsule exact stdin bytes\n",
        grants,
        ownership::raw_boot_ms() + lifetime_ms,
    )
    .unwrap()
    .arguments(vec![
        std::process::id().to_string(),
        tids,
        original.as_raw_fd().to_string(),
        source.to_str().unwrap().into(),
        metadata.dev().to_string(),
        metadata.ino().to_string(),
    ])
    .unwrap();
    Fixture {
        _root: root,
        plan,
        helper,
        workspace,
        _original: original,
    }
}
fn fixture() -> Fixture {
    fixture_with_lifetime(8000)
}
fn ready(handle: &mut CapsuleHandle) -> std::net::TcpStream {
    let listener = handle.take_listener().unwrap();
    ready_listener(listener)
}
fn ready_listener(listener: std::net::TcpListener) -> std::net::TcpStream {
    let until = Instant::now() + Duration::from_secs(3);
    let mut connection = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("{e}"),
        }
    };
    connection
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut bytes = [0u8; 18];
    connection.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"static-proxy-ready");
    connection.write_all(b"x").unwrap();
    connection
}

struct PreparedFixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    _helper: PinnedHelper,
    _original: File,
}
fn prepared_fixture(
    lifetime: u64,
) -> (
    PreparedFixture,
    CandidatePreparedHandle,
    std::net::TcpListener,
) {
    let fixture = fixture_with_lifetime(lifetime);
    std::fs::remove_file(fixture._root.path().join("auth/auth.json")).unwrap();
    // Keep fixture plan ownership separate while reusing the accepted static peer.
    let Fixture {
        _root,
        plan,
        helper,
        workspace,
        _original,
    } = fixture;
    let owner = ArtifactRef {
        id: Text::new("synthetic-owner-intent").unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(b"immutable synthetic owner, no authority"),
    };
    let mut prepared = CandidatePreparedHandle::prepare(&helper, plan, owner).unwrap();
    let listener = prepared.take_listener().unwrap();
    assert!(!workspace.join("checks-passed").exists());
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    (
        PreparedFixture {
            root: _root,
            _helper: helper,
            workspace,
            _original,
        },
        prepared,
        listener,
    )
}
fn publish_commit(
    fixture: &PreparedFixture,
    prepared: &CandidatePreparedHandle,
) -> CandidateCommit {
    let cap = format!(
        "aq-responses-{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let published = CandidatePublishedInputs::publish_synthetic(
        prepared.info(),
        &fixture.root.path().join("auth"),
        cap.as_bytes(),
    )
    .unwrap();
    CandidateCommit::new(
        prepared.info(),
        published,
        CandidateStaticRecipe::new(prepared.info()),
        Digest::of(b"synthetic parent transport scope"),
    )
    .unwrap()
}
fn changed_commit(
    commit: CandidateCommit,
    change: impl FnOnce(&mut serde_json::Value),
) -> CandidateCommit {
    let mut value = serde_json::to_value(commit).unwrap();
    change(&mut value);
    let mut tuple = serde_json::json!([
        value["scope"],
        value["published"],
        value["recipe"],
        value["transport_scope_digest"]
    ]);
    tuple.sort_all_objects();
    value["digest"] =
        serde_json::to_value(Digest::of(&serde_json::to_vec(&tuple).unwrap())).unwrap();
    serde_json::from_value(value).unwrap()
}
fn prepared_pidfds(info: &CandidatePreparedInfo) -> Vec<std::os::fd::OwnedFd> {
    vec![
        ownership::verified_pidfd(info.namespace_init()).unwrap(),
        ownership::verified_pidfd(info.exec_stub()).unwrap(),
    ]
}
fn assert_ended(fds: &[std::os::fd::OwnedFd]) {
    for fd in fds {
        assert!(!ownership::pidfd_is_live(fd).unwrap());
    }
}

#[test]
#[ignore = "explicit synthetic prepared static capsule; no real key/provider"]
fn prepared_pause_publishes_no_guest_execution_then_exact_commit_executes_once() {
    let (fixture, prepared, listener) = prepared_fixture(8000);
    let info = prepared.info().clone();
    let fds = prepared_pidfds(&info);
    std::thread::sleep(Duration::from_millis(20));
    assert!(!fixture.workspace.join("checks-passed").exists());
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
    let commit = publish_commit(&fixture, &prepared);
    let running = prepared.commit(commit).unwrap();
    assert_eq!(running.witness().harness, *info.exec_stub());
    let _proxy = ready_listener(listener);
    assert!(fixture.workspace.join("checks-passed").exists());
    let report = running.stop().unwrap();
    assert!(report.kernel_echild());
    assert_ended(&fds);
}

#[test]
#[ignore = "explicit missing-commit/control-loss static capsule; no provider"]
fn prepared_missing_commit_abort_and_drop_kill_the_paused_stub_without_running() {
    for drop_control in [false, true] {
        let (fixture, prepared, _listener) = prepared_fixture(8000);
        let info = prepared.info().clone();
        let fds = prepared_pidfds(&info);
        if drop_control {
            drop(prepared);
        } else {
            let report = prepared.abort().unwrap();
            assert!(report.kernel_echild() && report.reaped_count() > 0);
            assert_eq!(report.info(), &info);
        }
        assert_ended(&fds);
        assert!(!fixture.workspace.join("checks-passed").exists());
    }
}

#[test]
#[ignore = "explicit original deadline and late commit static capsule; no provider"]
fn prepared_original_deadline_refuses_late_commit_without_extension() {
    let (fixture, prepared, _listener) = prepared_fixture(400);
    let info = prepared.info().clone();
    let fds = prepared_pidfds(&info);
    let commit = publish_commit(&fixture, &prepared);
    while ownership::raw_boot_ms() <= info.deadline_boot_ms() {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(prepared.commit(commit).is_err());
    assert_ended(&fds);
    assert!(!fixture.workspace.join("checks-passed").exists());
}

#[test]
#[ignore = "explicit conflicting metadata/closed recipe static capsule; no provider"]
fn prepared_changed_intent_nonce_origin_deadline_and_recipe_are_terminal() {
    for field in [
        "owner_intent",
        "capsule_id",
        "origin",
        "original_deadline_boot_ms",
        "recipe",
    ] {
        let (fixture, prepared, _listener) = prepared_fixture(8000);
        let fds = prepared_pidfds(prepared.info());
        let commit = publish_commit(&fixture, &prepared);
        let commit = changed_commit(commit, |value| match field {
            "owner_intent" => {
                value["scope"][field]["digest"] =
                    serde_json::json!(Digest::of(b"conflicting-intent"))
            }
            "capsule_id" => value["scope"][field] = serde_json::json!(uuid::Uuid::new_v4()),
            "origin" => value["scope"][field] = serde_json::json!("http://127.0.0.1:1"),
            "original_deadline_boot_ms" => {
                value["scope"][field] =
                    serde_json::json!(value["scope"][field].as_u64().unwrap() + 1)
            }
            _ => value["recipe"]["environment"]["PATH"] = serde_json::json!("/unqualified"),
        });
        assert!(prepared.commit(commit).is_err(), "accepted changed {field}");
        assert_ended(&fds);
        assert!(!fixture.workspace.join("checks-passed").exists());
    }
}

#[test]
#[ignore = "explicit actual derived-file mutation static capsule; no provider"]
fn prepared_actual_auth_config_mode_symlink_and_extra_member_are_refused() {
    for mutation in ["auth", "config", "mode", "symlink", "extra"] {
        let (fixture, prepared, _listener) = prepared_fixture(8000);
        let fds = prepared_pidfds(prepared.info());
        let commit = publish_commit(&fixture, &prepared);
        let auth = fixture.root.path().join("auth/auth.json");
        match mutation {
            "auth" => std::fs::write(&auth, b"changed synthetic auth").unwrap(),
            "config" => std::fs::write(
                fixture.root.path().join("auth/candidate-config.json"),
                b"{}",
            )
            .unwrap(),
            "mode" => {
                std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o644)).unwrap()
            }
            "symlink" => {
                std::fs::remove_file(&auth).unwrap();
                std::os::unix::fs::symlink(
                    fixture.root.path().join("private-node/original"),
                    &auth,
                )
                .unwrap();
            }
            _ => std::fs::write(
                fixture.root.path().join("auth/extra"),
                b"unqualified member",
            )
            .unwrap(),
        }
        assert!(
            prepared.commit(commit).is_err(),
            "accepted changed {mutation}"
        );
        assert_ended(&fds);
        assert!(!fixture.workspace.join("checks-passed").exists());
    }
}
fn descendant(pid: u32) -> ownership::ProcessEvidence {
    let children = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
    let pid = children.split_whitespace().next().unwrap().parse().unwrap();
    ownership::process_evidence(pid).unwrap()
}
#[test]
#[ignore = "explicit rootless namespace/static helper qualification; no Codex/provider"]
fn assembled_static_capsule_has_private_views_trusted_proxy_and_actual_echild() {
    let fixture = fixture();
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let witness = handle.witness().clone();
    assert_ne!(witness.guardian.pid, witness.init.pid);
    assert_ne!(witness.init.pid, witness.harness.pid);
    let _proxy = ready(&mut handle);
    assert!(fixture.workspace.join("checks-passed").is_file());
    let descendant = descendant(witness.harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let initfd = ownership::verified_pidfd(&witness.init).unwrap();
    let harnessfd = ownership::verified_pidfd(&witness.harness).unwrap();
    let report = handle.stop().unwrap();
    assert!(report.kernel_echild() && report.reaped_count() > 0);
    assert_eq!(report.witness().init, witness.init);
    assert_eq!(report.witness().harness, witness.harness);
    assert!(!ownership::pidfd_is_live(&pidfd).unwrap());
    assert!(!ownership::pidfd_is_live(&initfd).unwrap());
    assert!(!ownership::pidfd_is_live(&harnessfd).unwrap());
    assert!(ownership::process_evidence(witness.guardian.pid).is_err());
}
#[test]
#[ignore = "explicit rootless namespace/static helper qualification; no Codex/provider"]
fn dropped_handle_closes_control_and_physically_ends_capsule_without_publishing_proof() {
    let fixture = fixture();
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let witness = handle.witness().clone();
    let _proxy = ready(&mut handle);
    let descendant = descendant(witness.harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let initfd = ownership::verified_pidfd(&witness.init).unwrap();
    let harnessfd = ownership::verified_pidfd(&witness.harness).unwrap();
    drop(handle);
    assert!(!ownership::pidfd_is_live(&pidfd).unwrap());
    assert!(!ownership::pidfd_is_live(&initfd).unwrap());
    assert!(!ownership::pidfd_is_live(&harnessfd).unwrap());
    assert!(ownership::process_evidence(witness.guardian.pid).is_err());
}

#[test]
#[ignore = "explicit rootless namespace/static helper refusal; no Codex/provider"]
fn replaced_mount_route_refuses_before_untrusted_exec() {
    let fixture = fixture();
    let original = fixture.workspace.with_file_name("original-workspace");
    std::fs::rename(&fixture.workspace, &original).unwrap();
    std::fs::create_dir(&fixture.workspace).unwrap();
    assert!(CapsuleHandle::spawn(&fixture.helper, fixture.plan).is_err());
    assert!(!fixture.workspace.join("checks-passed").exists());
    assert!(!original.join("checks-passed").exists());
}

#[test]
#[ignore = "explicit rootless namespace/static BOOTTIME expiry; no Codex/provider"]
fn original_deadline_ends_descendants_and_only_actual_queued_echild_counts_as_stop() {
    let fixture = fixture_with_lifetime(1500);
    let mut handle = CapsuleHandle::spawn(&fixture.helper, fixture.plan).unwrap();
    let _proxy = ready(&mut handle);
    let descendant = descendant(handle.witness().harness.pid);
    let pidfd = ownership::verified_pidfd(&descendant).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    while ownership::pidfd_is_live(&pidfd).unwrap() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(2));
    }
    let report = handle.stop().unwrap();
    assert!(report.kernel_echild() && report.reaped_count() > 0);
}

#[test]
fn unsupported_roles_write_modes_and_expired_deadlines_refuse_without_launch() {
    let root = tempfile::tempdir().unwrap();
    assert!(MountGrant::open(root.path(), MountTarget::Allowed(4), false).is_err());
    assert!(MountGrant::open(root.path(), MountTarget::DerivedAuth, true).is_err());
    assert!(
        StaticCapsulePlan::new(
            root.path(),
            Path::new("/nonexistent-static-image"),
            &ArtifactRef {
                id: Text::new("unread-static-fixture").unwrap(),
                version: Text::new("1").unwrap(),
                digest: Digest::of(b"not read")
            },
            b"canary",
            vec![],
            ownership::raw_boot_ms() - 1
        )
        .is_err()
    );
}
