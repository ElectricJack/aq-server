use super::*;
use crate::private_inputs::tests::{login, root, scoped, source};

struct Fixture {
    temp: tempfile::TempDir,
    root: File,
    owner: File,
    pin: PrivateInputRecord,
    _source: File,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let (source, _, _, record_digest) = source(temp.path());
        let original = login("original-refresh");
        std::fs::write(temp.path().join("source/material.bin"), &original).unwrap();
        let (scope, _) = scoped();
        let input = PrivateLaunchInput::codex_chatgpt_auth_json(
            source,
            OriginalPrivateMaterial {
                digest: Digest::of(&original),
                bytes: original.len() as u64,
                record_digest,
            },
            scope.binding,
            scope.launch_id,
            scope.deadline_boot_ms,
        )
        .unwrap();
        let path = root(temp.path());
        let publication = Publication::reserve(&path, input).unwrap();
        let pin = publication.record();
        let source = publication.publish().unwrap();
        let root = open_directory(&path).unwrap();
        let owner_path = temp.path().join("owner");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&owner_path)
            .unwrap();
        let owner = open_directory(&owner_path).unwrap();
        replace(&owner, c"auth.json", OWNER_NEXT, &original, true).unwrap();
        let home = private_home(&root, &pin).unwrap();
        replace(
            &home,
            c"auth.json",
            c"next",
            &login("rotated-refresh"),
            true,
        )
        .unwrap();
        Self {
            temp,
            root,
            owner,
            pin,
            _source: source,
        }
    }
    fn run(&self, writer: &mut impl LoginWriteBack) -> Result<LoginRefreshStatus> {
        reconcile(&self.root, &self.pin, &self.owner, writer, |_| Ok(()))
    }
    fn state(&self) -> LoginRefreshStatus {
        load(&self.root, &self.pin).unwrap().unwrap().status
    }
    fn auth(&self) -> PathBuf {
        self.temp.path().join("launch/codex-home/auth.json")
    }
    fn owner_auth(&self) -> PathBuf {
        self.temp.path().join("owner/auth.json")
    }
    fn assert_retained(&self) {
        assert_eq!(
            std::fs::read(self.auth()).unwrap(),
            login("rotated-refresh")
        );
    }
    fn handle(&self) -> (SessionHandle, StoppedProof) {
        // Synthetic sealed stopped host: no process launch or real credential.
        let (_, manifest) = scoped();
        let config = SealedLaunch {
            launch_id: self.pin.chatgpt.as_ref().unwrap().launch_id,
            permit: ExecutionPermit {
                session: manifest.bindings.session.clone(),
                manifest_digest: canonical_digest(&manifest).unwrap(),
                sequence: 1,
                issued_boot_ms: 1,
                expires_boot_ms: 10000,
            },
            resource_policy: ResourcePolicy::CallerAdmission(manifest.harness.environment.clone()),
            manifest: manifest.into(),
            workspace: self.temp.path().into(),
            helper: "/synthetic-helper".into(),
            tmux: "/synthetic-tmux".into(),
            tmux_version: "synthetic".into(),
            command: LocalCommand {
                executable: "/synthetic-codex".into(),
                arguments: vec![],
                environment: Default::default(),
            },
            resource_qualification: None,
            rendered_startup: None,
            observed: None,
            private_input: Some(self.pin.clone()),
        };
        durable_new(
            &self.temp.path().join("launch/launch.json"),
            &serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let identity = GuardianIdentity {
            intent: config.intent().unwrap(),
            nonce: Uuid::new_v4(),
            process: ownership::process_evidence(std::process::id()).unwrap(),
            machine_boot_id: "synthetic".into(),
            pid_namespace: "synthetic".into(),
        };
        let proof = StoppedProof {
            observation: HostObservation {
                guardian: identity.clone(),
                phase: HostPhase::Stopped,
                pane: None,
                permit_sequence: 1,
                permit_deadline_boot_ms: 10000,
                observed_boot_ms: 10001,
                reaped_count: 1,
                kernel_wait_echild: true,
                root_status: Some(0),
                stop_reason: None,
            },
        };
        (
            SessionHandle {
                directory: self.temp.path().join("launch"),
                identity,
                private_source: None,
            },
            proof,
        )
    }
}
#[derive(Default)]
struct Server {
    operations: Vec<(Uuid, Digest)>,
    lose_ack: bool,
}
impl LoginWriteBack for Server {
    fn write_back(&mut self, request: &LoginRefreshRequest<'_>) -> Result<LoginRefreshAck> {
        assert_eq!(request.private_material(), login("rotated-refresh"));
        assert_eq!(
            request.original_auth_digest(),
            &Digest::of(&login("original-refresh"))
        );
        request.binding().validate().unwrap();
        assert!(!request.launch_id().is_nil());
        assert_eq!(
            request.original_record_digest(),
            &Digest::of(b"{\"schema_version\":1}")
        );
        assert!(!format!("{request:?}").contains("rotated-refresh"));
        let operation = (request.operation_id(), request.refreshed_digest().clone());
        if let Some(previous) = self.operations.first() {
            assert_eq!(&operation, previous);
        }
        self.operations.push(operation);
        if self.lose_ack {
            bail!("private-response-canary");
        }
        request.acknowledge("fake-refresh-revision:2")
    }
}

#[test]
fn durable_owner_and_ack_precede_cleanup_and_completed_replay_is_idempotent() {
    let f = Fixture::new();
    let old_inode = std::fs::metadata(f.owner_auth()).unwrap().ino();
    let mut server = Server::default();
    let status = f.run(&mut server).unwrap();
    assert_eq!(status.phase, LoginRefreshPhase::Complete);
    assert_eq!(
        status.server_revision.as_deref(),
        Some("fake-refresh-revision:2")
    );
    assert_eq!(f.state(), status);
    assert!(!f.auth().parent().unwrap().exists());
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("rotated-refresh")
    );
    let m = std::fs::metadata(f.owner_auth()).unwrap();
    assert_eq!(m.mode() & 0o7777, 0o600);
    assert_ne!(
        m.ino(),
        old_inode,
        "must replace atomically, not write in place"
    );
    assert_eq!(
        std::fs::read(f.temp.path().join("source/material.bin")).unwrap(),
        login("original-refresh")
    );
    let raw = std::fs::read_to_string(f.temp.path().join("launch/chatgpt-refresh.json")).unwrap();
    assert!(!raw.contains("rotated-refresh") && !raw.contains("original-refresh"));
    assert!(cleanup_guard(&f.root, &f.pin).is_ok());
    // Another successfully reconciled rotation must not prevent replay of this
    // completed transaction, or restore the older credential.
    std::fs::write(f.owner_auth(), login("later-refresh")).unwrap();
    assert_eq!(f.run(&mut server).unwrap(), status);
    assert_eq!(server.operations.len(), 1);
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("later-refresh")
    );
}

#[test]
fn lost_ack_retains_rotated_copy_and_restarts_with_identical_operation() {
    let f = Fixture::new();
    let mut server = Server {
        lose_ack: true,
        ..Default::default()
    };
    let error = f.run(&mut server).unwrap_err();
    assert!(!format!("{error:#}").contains("private-response-canary"));
    assert_eq!(f.state().phase, LoginRefreshPhase::OwnerUpdated);
    f.assert_retained();
    assert!(cleanup_guard(&f.root, &f.pin).is_err());
    server.lose_ack = false;
    // Fresh descriptors and no retained transaction object model restart.
    let root = open_directory(&f.temp.path().join("launch")).unwrap();
    let owner = open_directory(&f.temp.path().join("owner")).unwrap();
    assert_eq!(
        reconcile(&root, &f.pin, &owner, &mut server, |_| Ok(()))
            .unwrap()
            .phase,
        LoginRefreshPhase::Complete
    );
    assert_eq!(server.operations.len(), 2);
    assert_eq!(server.operations[0], server.operations[1]);
}

#[test]
fn owner_publication_crash_is_recovered_without_rollback_or_new_operation() {
    let f = Fixture::new();
    let mut server = Server::default();
    assert!(
        reconcile(&f.root, &f.pin, &f.owner, &mut server, |point| {
            if point == Boundary::OwnerPublished {
                bail!("simulated crash");
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(f.state().phase, LoginRefreshPhase::PendingOwner);
    let id = f.state().operation_id;
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("rotated-refresh")
    );
    f.assert_retained();
    assert!(server.operations.is_empty());
    assert_eq!(f.run(&mut server).unwrap().operation_id, id);
}

#[test]
fn durable_ack_and_unlink_failures_retry_cleanup_without_server_resubmission() {
    for crash in [
        Boundary::AckPersisted,
        Boundary::BeforeUnlink,
        Boundary::AuthUnlinked,
    ] {
        let f = Fixture::new();
        let mut server = Server::default();
        assert!(
            reconcile(&f.root, &f.pin, &f.owner, &mut server, |point| {
                if point == crash {
                    bail!("simulated cleanup failure");
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(f.state().phase, LoginRefreshPhase::Acknowledged);
        if crash != Boundary::AuthUnlinked {
            f.assert_retained();
        }
        assert!(cleanup_guard(&f.root, &f.pin).is_err());
        assert_eq!(
            f.run(&mut server).unwrap().phase,
            LoginRefreshPhase::Complete
        );
        assert_eq!(server.operations.len(), 1);
    }
}

#[test]
fn unknown_home_files_block_completion_and_are_never_recursively_deleted() {
    let f = Fixture::new();
    let unknown = f.auth().parent().unwrap().join("keep");
    std::fs::write(&unknown, b"keep me").unwrap();
    let mut server = Server::default();
    assert!(f.run(&mut server).is_err());
    assert_eq!(f.state().phase, LoginRefreshPhase::Acknowledged);
    assert!(!f.auth().exists());
    assert_eq!(std::fs::read(&unknown).unwrap(), b"keep me");
    std::fs::remove_file(unknown).unwrap();
    assert_eq!(
        f.run(&mut server).unwrap().phase,
        LoginRefreshPhase::Complete
    );
    assert_eq!(server.operations.len(), 1);
}

#[test]
fn concurrent_owner_rotation_is_preserved_before_write_and_on_retry() {
    for after_owner_update in [false, true] {
        let f = Fixture::new();
        let mut server = Server {
            lose_ack: true,
            ..Default::default()
        };
        if after_owner_update {
            assert!(f.run(&mut server).is_err());
        }
        std::fs::write(f.owner_auth(), login("operator-new-login")).unwrap();
        let before_calls = server.operations.len();
        assert!(f.run(&mut server).is_err());
        assert_eq!(f.state().phase, LoginRefreshPhase::Conflict);
        assert_eq!(server.operations.len(), before_calls);
        assert_eq!(
            std::fs::read(f.owner_auth()).unwrap(),
            login("operator-new-login")
        );
        f.assert_retained();
        assert!(f.run(&mut server).is_err(), "conflict must remain held");
    }
}

#[test]
fn competing_desktop_and_session_locks_refuse_without_touching_auth() {
    let f = Fixture::new();
    for (dir, name) in [(&f.owner, c".aq-auth.lock"), (&f.root, c".aq-refresh.lock")] {
        let guard = lock(dir, name).unwrap();
        let mut server = Server::default();
        assert!(f.run(&mut server).is_err());
        assert!(server.operations.is_empty());
        assert_eq!(
            std::fs::read(f.owner_auth()).unwrap(),
            login("original-refresh")
        );
        f.assert_retained();
        drop(guard);
    }
    assert!(f.run(&mut Server::default()).is_ok());
}

#[test]
fn foreign_acknowledgement_cannot_authorize_cleanup() {
    struct Foreign;
    impl LoginWriteBack for Foreign {
        fn write_back(&mut self, r: &LoginRefreshRequest<'_>) -> Result<LoginRefreshAck> {
            let mut ack = r.acknowledge("foreign:2")?;
            ack.operation_id = Uuid::new_v4();
            Ok(ack)
        }
    }
    let f = Fixture::new();
    assert!(f.run(&mut Foreign).is_err());
    assert_eq!(f.state().phase, LoginRefreshPhase::OwnerUpdated);
    f.assert_retained();
}

#[test]
fn mutation_during_server_exchange_preserves_both_copies() {
    struct Mutating {
        owner: PathBuf,
    }
    impl LoginWriteBack for Mutating {
        fn write_back(&mut self, r: &LoginRefreshRequest<'_>) -> Result<LoginRefreshAck> {
            std::fs::write(&self.owner, login("operator-during-exchange"))?;
            r.acknowledge("refresh:2")
        }
    }
    let f = Fixture::new();
    assert!(
        f.run(&mut Mutating {
            owner: f.owner_auth()
        })
        .is_err()
    );
    assert_eq!(f.state().phase, LoginRefreshPhase::Conflict);
    assert_eq!(f.state().server_revision.as_deref(), Some("refresh:2"));
    f.assert_retained();
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("operator-during-exchange")
    );
}

#[test]
fn changed_rotated_file_after_ack_is_preserved() {
    let f = Fixture::new();
    let mut server = Server::default();
    assert!(
        reconcile(&f.root, &f.pin, &f.owner, &mut server, |point| {
            if point == Boundary::AckPersisted {
                std::fs::write(f.auth(), login("unexpected-local-change"))?;
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(f.state().phase, LoginRefreshPhase::Acknowledged);
    assert_eq!(
        std::fs::read(f.auth()).unwrap(),
        login("unexpected-local-change")
    );
    assert!(f.run(&mut server).is_err());
    assert_eq!(server.operations.len(), 1);
}

#[test]
fn symlink_hardlink_and_nonprivate_files_are_refused() {
    for target_owner in [true, false] {
        for attack in ["symlink", "hardlink", "mode"] {
            let f = Fixture::new();
            let path = if target_owner {
                f.owner_auth()
            } else {
                f.auth()
            };
            let saved = f.temp.path().join("saved");
            if attack == "mode" {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            } else {
                std::fs::rename(&path, &saved).unwrap();
                if attack == "symlink" {
                    std::os::unix::fs::symlink(&saved, &path).unwrap();
                } else {
                    std::fs::hard_link(&saved, &path).unwrap();
                }
            }
            let mut server = Server::default();
            assert!(f.run(&mut server).is_err(), "{target_owner}/{attack}");
            assert!(server.operations.is_empty());
            assert!(path.exists());
        }
    }
}

#[test]
fn replay_refuses_retargeted_owner_and_changed_journal_binding() {
    let f = Fixture::new();
    let mut server = Server {
        lose_ack: true,
        ..Default::default()
    };
    assert!(f.run(&mut server).is_err());
    let path = f.temp.path().join("other-owner");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    let other = open_directory(&path).unwrap();
    replace(
        &other,
        c"auth.json",
        OWNER_NEXT,
        &login("rotated-refresh"),
        true,
    )
    .unwrap();
    assert!(reconcile(&f.root, &f.pin, &other, &mut server, |_| Ok(())).is_err());
    let mut record = load(&f.root, &f.pin).unwrap().unwrap();
    record.original_record_digest = Digest::of(b"foreign installation");
    save(&f.root, &record).unwrap();
    assert!(f.run(&mut server).is_err());
    f.assert_retained();
    assert_eq!(server.operations.len(), 1);
}

#[test]
fn directory_symlink_components_and_pending_foreign_secrets_are_not_followed() {
    let f = Fixture::new();
    let link = f.temp.path().join("owner-link");
    std::os::unix::fs::symlink(f.temp.path().join("owner"), &link).unwrap();
    assert!(open_directory(&link).is_err());
    let next = f.temp.path().join("owner/auth.json.aq-refresh");
    durable_new(&next, &login("previous-retained-refresh")).unwrap();
    assert!(f.run(&mut Server::default()).is_err());
    assert_eq!(
        std::fs::read(next).unwrap(),
        login("previous-retained-refresh")
    );
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("original-refresh")
    );
    f.assert_retained();
}

#[test]
fn public_host_api_requires_exact_stopped_proof_and_pinned_configuration() {
    let f = Fixture::new();
    let (mut handle, proof) = f.handle();
    let owner = f.temp.path().join("owner");
    let mut server = Server::default();
    for attack in ["live", "no-echild", "foreign"] {
        let mut invalid = proof.clone();
        match attack {
            "live" => invalid.observation.phase = HostPhase::Running,
            "no-echild" => invalid.observation.kernel_wait_echild = false,
            _ => invalid.observation.guardian.nonce = Uuid::new_v4(),
        }
        assert!(
            handle
                .reconcile_chatgpt_login(&invalid, &owner, &mut server)
                .is_err()
        );
    }
    assert!(server.operations.is_empty());
    assert!(handle.chatgpt_refresh_status(&proof).unwrap().is_none());
    assert!(handle.cleanup_private_inputs(&proof).is_err());
    let status = handle
        .reconcile_chatgpt_login(&proof, &owner, &mut server)
        .unwrap();
    assert_eq!(status.phase, LoginRefreshPhase::Complete);
    assert_eq!(handle.chatgpt_refresh_status(&proof).unwrap(), Some(status));
    assert!(handle.cleanup_private_inputs(&proof).unwrap().home_removed);
    // A changed sealed launch must not reuse the previous stopped proof.
    let path = f.temp.path().join("launch/launch.json");
    let mut config: SealedLaunch = read_json(&path).unwrap();
    config.launch_id = Uuid::new_v4();
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(
        handle
            .reconcile_chatgpt_login(&proof, &owner, &mut server)
            .is_err()
    );
}

#[test]
fn ordinary_cleanup_cannot_bypass_pending_reconciliation_even_for_unchanged_bytes() {
    struct Unavailable;
    impl LoginWriteBack for Unavailable {
        fn write_back(&mut self, _: &LoginRefreshRequest<'_>) -> Result<LoginRefreshAck> {
            bail!("offline")
        }
    }
    let f = Fixture::new();
    std::fs::write(f.auth(), login("original-refresh")).unwrap();
    let (mut handle, proof) = f.handle();
    assert!(
        handle
            .reconcile_chatgpt_login(&proof, &f.temp.path().join("owner"), &mut Unavailable)
            .is_err()
    );
    assert_eq!(f.state().phase, LoginRefreshPhase::OwnerUpdated);
    assert!(handle.cleanup_private_inputs(&proof).is_err());
    assert!(f.auth().exists());
}

#[test]
fn actual_rotated_file_unlink_failure_keeps_ack_and_private_bytes_for_recovery() {
    let f = Fixture::new();
    let held = f.auth().with_file_name("auth-held.json");
    let mut server = Server::default();
    let error = reconcile(&f.root, &f.pin, &f.owner, &mut server, |point| {
        if point == Boundary::BeforeUnlink {
            // Force unlinkat(..., 0) to fail with EISDIR, independently of uid.
            // Keep the original inode and bytes to model corrective recovery.
            std::fs::rename(f.auth(), &held)?;
            std::fs::DirBuilder::new().mode(0o700).create(f.auth())?;
        }
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("cleanup failed"));
    assert_eq!(f.state().phase, LoginRefreshPhase::Acknowledged);
    assert_eq!(std::fs::read(&held).unwrap(), login("rotated-refresh"));
    assert_eq!(
        std::fs::read(f.owner_auth()).unwrap(),
        login("rotated-refresh")
    );
    std::fs::remove_dir(f.auth()).unwrap();
    std::fs::rename(held, f.auth()).unwrap();
    assert_eq!(
        f.run(&mut server).unwrap().phase,
        LoginRefreshPhase::Complete
    );
    assert_eq!(server.operations.len(), 1);
}

#[test]
fn disposable_private_home_cannot_be_selected_as_the_desktop_owner() {
    let f = Fixture::new();
    let home = private_home(&f.root, &f.pin).unwrap();
    let mut server = Server::default();
    for owner in [&home, &f.root] {
        assert!(reconcile(&f.root, &f.pin, owner, &mut server, |_| Ok(())).is_err());
        assert!(server.operations.is_empty());
        assert!(load(&f.root, &f.pin).unwrap().is_none());
        assert!(!exists(owner, c".aq-auth.lock").unwrap());
        f.assert_retained();
    }
}
