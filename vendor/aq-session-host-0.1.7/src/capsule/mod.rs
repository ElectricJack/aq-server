//! Isolated test-support-only static capsule assembly. No production authority.
//! Caller mount grants are trusted input, not private-root authorization.
mod guardian;
mod mounts;
mod prepared;
pub use prepared::{
    CandidateCommit, CandidateCommitFailure, CandidatePreparedHandle, CandidatePreparedInfo,
    CandidatePreparedStopReport, CandidatePublishedInputs, CandidateStaticRecipe,
};
mod seal;
mod setup;
mod wire;

use crate::{ownership, ownership::ProcessEvidence};
use anyhow::{Context, Result, bail, ensure};
use aq_session_protocol::{ArtifactRef, Digest, Validate};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    net::TcpListener,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

pub const PROFILE: &str = "linux-x86_64-static-capsule-candidate-v1";
pub const PRODUCTION_ELIGIBLE: bool = false;
const MAX_PROGRAM: u64 = 4 * 1024 * 1024;
const MAX_HELPER: u64 = 64 * 1024 * 1024;
const MAX_STDIN: usize = 65536;
const MAX_LIFETIME_MS: u64 = 10000;
const CLEANUP_MS: u64 = 5000;

/// Fixed guest destinations; this does not authorize source directory contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MountTarget {
    Workspace,
    DerivedAuth,
    GitMetadata,
    Allowed(u8),
    TrustedUsr,
    TrustedLib,
    TrustedLib64,
}
impl MountTarget {
    fn relative(self) -> Result<String> {
        Ok(match self {
            Self::Workspace => "workspace".into(),
            Self::DerivedAuth => "auth".into(),
            Self::GitMetadata => "git-metadata".into(),
            Self::Allowed(index) => {
                ensure!(index < 4, "capsule allowed-root bound");
                format!("allowed/{index}")
            }
            Self::TrustedUsr => "usr".into(),
            Self::TrustedLib => "lib".into(),
            Self::TrustedLib64 => "lib64".into(),
        })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryPin {
    device: u64,
    inode: u64,
}
impl DirectoryPin {
    fn of(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        ensure!(m.is_dir(), "capsule directory FD");
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
        })
    }
    fn verify(&self, file: &File) -> Result<()> {
        let actual = Self::of(file)?;
        ensure!(
            actual.device == self.device && actual.inode == self.inode,
            "capsule directory substitution"
        );
        Ok(())
    }
}
pub struct MountGrant {
    source: PathBuf,
    file: File,
    target: MountTarget,
    writable: bool,
    pin: DirectoryPin,
}
impl MountGrant {
    pub fn open(source: &Path, target: MountTarget, writable: bool) -> Result<Self> {
        target.relative()?;
        ensure!(
            !writable
                || matches!(
                    target,
                    MountTarget::Workspace | MountTarget::GitMetadata | MountTarget::Allowed(_)
                ),
            "capsule readonly target required"
        );
        ensure!(
            source.is_absolute() && std::fs::canonicalize(source)? == source,
            "capsule grant canonical source"
        );
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(source)?;
        let pin = DirectoryPin::of(&file)?;
        Ok(Self {
            source: source.into(),
            file,
            target,
            writable,
            pin,
        })
    }
}
pub struct PinnedHelper {
    file: File,
}
impl PinnedHelper {
    pub fn open(path: &Path, expected: &ArtifactRef) -> Result<Self> {
        let (file, _) = seal::source(path, expected, MAX_HELPER, false)?;
        Ok(Self { file })
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantRecord {
    source: PathBuf,
    target: MountTarget,
    writable: bool,
    pin: DirectoryPin,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prepared: Option<prepared::PreparedSpec>,
    nonce: Uuid,
    deadline: u64,
    root: PathBuf,
    root_pin: DirectoryPin,
    program: seal::Pin,
    stdin: seal::Pin,
    grants: Vec<GrantRecord>,
    arguments: Vec<String>,
}
pub struct StaticCapsulePlan {
    record: PlanRecord,
    files: Vec<File>,
}
impl StaticCapsulePlan {
    pub fn new(
        root: &Path,
        program: &Path,
        expected: &ArtifactRef,
        exact_stdin: &[u8],
        grants: Vec<MountGrant>,
        deadline_boot_ms: u64,
    ) -> Result<Self> {
        ensure!(
            cfg!(target_arch = "x86_64") && unsafe { libc::geteuid() } != 0,
            "capsule rootless x86_64 candidate only"
        );
        let now = ownership::raw_boot_ms();
        ensure!(
            deadline_boot_ms > now && deadline_boot_ms - now <= MAX_LIFETIME_MS,
            "capsule bounded original deadline"
        );
        ensure!(
            !exact_stdin.is_empty()
                && exact_stdin.len() <= MAX_STDIN
                && !grants.is_empty()
                && grants.len() <= 10,
            "capsule bounded input/grants"
        );
        ensure!(
            root.is_absolute()
                && std::fs::canonicalize(root)? == root
                && std::fs::read_dir(root)?.next().is_none(),
            "capsule fresh canonical root"
        );
        let root_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        let meta = root_file.metadata()?;
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o777 == 0o700,
            "capsule private root mode"
        );
        let mut targets = Vec::new();
        for grant in &grants {
            ensure!(
                !root.starts_with(&grant.source)
                    && !grant.source.starts_with(root)
                    && !targets.contains(&grant.target),
                "capsule root/grant overlap or duplicate"
            );
            targets.push(grant.target);
        }
        ensure!(
            targets.contains(&MountTarget::Workspace),
            "capsule workspace required"
        );
        let (program, program_pin) = seal::source(program, expected, MAX_PROGRAM, true)?;
        let (stdin, stdin_pin) = seal::bytes(exact_stdin, false)?;
        let mut files = vec![program, stdin, root_file];
        let records = grants
            .into_iter()
            .map(|g| {
                files.push(g.file);
                GrantRecord {
                    source: g.source,
                    target: g.target,
                    writable: g.writable,
                    pin: g.pin,
                }
            })
            .collect();
        Ok(Self {
            record: PlanRecord {
                prepared: None,
                nonce: Uuid::new_v4(),
                deadline: deadline_boot_ms,
                root: root.into(),
                root_pin: DirectoryPin::of(&files[2])?,
                program: program_pin,
                stdin: stdin_pin,
                grants: records,
                arguments: Vec::new(),
            },
            files,
        })
    }
    pub fn arguments(mut self, arguments: Vec<String>) -> Result<Self> {
        ensure!(
            arguments.len() <= 16
                && arguments
                    .iter()
                    .all(|a| a.len() <= 2048 && !a.contains('\0')),
            "capsule argument bounds"
        );
        self.record.arguments = arguments;
        Ok(self)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleWitness {
    pub guardian: ProcessEvidence,
    pub init: ProcessEvidence,
    pub harness: ProcessEvidence,
    pub origin: String,
    nonce: Uuid,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Message {
    Plan(PlanRecord),
    Descriptor {
        nonce: Uuid,
        index: usize,
    },
    Init {
        nonce: Uuid,
        inner_pid: u32,
    },
    Listener {
        nonce: Uuid,
        port: u16,
    },
    Harness {
        nonce: Uuid,
        inner_pid: u32,
    },
    Release {
        nonce: Uuid,
    },
    Ready(CapsuleWitness),
    Stop {
        nonce: Uuid,
    },
    Stopped {
        nonce: Uuid,
        reaped: u64,
    },
    StoppedAck {
        nonce: Uuid,
    },
    Prepared(CandidatePreparedInfo),
    Commit(prepared::CommitRecord),
    PreparedStopped {
        info: CandidatePreparedInfo,
        reaped: u64,
    },
}
/// Candidate-only proof; cannot be promoted into an existing stopped-writer wire.
pub struct CapsuleStopReport {
    witness: CapsuleWitness,
    reaped: u64,
}
impl CapsuleStopReport {
    pub fn witness(&self) -> &CapsuleWitness {
        &self.witness
    }
    pub fn kernel_echild(&self) -> bool {
        true
    }
    pub fn reaped_count(&self) -> u64 {
        self.reaped
    }
}
struct ChildOwner {
    guardian: Option<(Child, crate::reaper::Slot)>,
    control: Option<wire::Socket>,
}
impl ChildOwner {
    fn cleanup_until(&mut self, until: u64) {
        self.control.take();
        if self.guardian.is_some() && self.wait(until).is_err() {
            // Never block on wait after the deadline, nor turn this handoff
            // into stop evidence. Existing reaper ownership/capacity remains
            // reserved until the unknown helper is actually reaped.
            if let Some((mut child, slot)) = self.guardian.take() {
                let _ = child.kill();
                let _ = slot.track(child);
            }
        }
    }
    fn wait(&mut self, until: u64) -> Result<()> {
        let (child, _) = self.guardian.as_mut().context("capsule guardian absent")?;
        loop {
            if let Some(status) = child.try_wait()? {
                self.guardian = None;
                ensure!(
                    status.success(),
                    "capsule guardian exited without clean report"
                );
                return Ok(());
            }
            ensure!(
                ownership::raw_boot_ms() < until,
                "capsule cleanup uncertain"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        self.cleanup_until(ownership::raw_boot_ms() + CLEANUP_MS);
    }
}
pub struct CapsuleHandle {
    owner: ChildOwner,
    witness: CapsuleWitness,
    listener: Option<TcpListener>,
    deadline: u64,
    _program: File,
}
impl CapsuleHandle {
    pub fn spawn(helper: &PinnedHelper, plan: StaticCapsulePlan) -> Result<Self> {
        let (owner, guardian_pid) = spawn_guardian(helper, &plan)?;
        let socket = owner.control.as_ref().unwrap();
        let packet = socket.recv::<Message>(plan.record.deadline)?;
        ensure!(
            packet.credentials.pid as u32 == guardian_pid,
            "capsule actual guardian sender"
        );
        let Message::Ready(witness) = packet.value else {
            bail!("capsule missing exact exec witness")
        };
        ensure!(
            witness.nonce == plan.record.nonce && witness.guardian.pid == guardian_pid,
            "capsule witness binding"
        );
        guardian::verify_image(&witness.harness, &plan.record.program)?;
        let listener = guardian::listener(
            packet.fd.context("capsule listener absent")?,
            &witness.origin,
        )?;
        let program = plan.files.into_iter().next().unwrap();
        Ok(Self {
            owner,
            witness,
            listener: Some(listener),
            deadline: plan.record.deadline,
            _program: program,
        })
    }
    pub fn witness(&self) -> &CapsuleWitness {
        &self.witness
    }
    pub fn take_listener(&mut self) -> Result<TcpListener> {
        self.listener
            .take()
            .context("capsule listener already taken")
    }
    pub fn stop(mut self) -> Result<CapsuleStopReport> {
        self.listener.take();
        let until = (ownership::raw_boot_ms() + CLEANUP_MS).min(self.deadline + CLEANUP_MS);
        let socket = self
            .owner
            .control
            .as_ref()
            .context("capsule control absent")?;
        // Expiry/natural exit may already have queued a genuine ECHILD report
        // and closed the peer. A failed request is never itself stop evidence.
        let _ = socket.send(
            &Message::Stop {
                nonce: self.witness.nonce,
            },
            None,
            until,
        );
        let packet = socket.recv::<Message>(until)?;
        ensure!(
            packet.credentials.pid as u32 == self.witness.guardian.pid && packet.fd.is_none(),
            "capsule stopped sender"
        );
        let Message::Stopped { nonce, reaped } = packet.value else {
            bail!("capsule uncertain stop")
        };
        ensure!(
            nonce == self.witness.nonce && reaped > 0,
            "capsule stop binding"
        );
        let _ = socket.send(&Message::StoppedAck { nonce }, None, until);
        self.owner.control.take();
        self.owner.wait(until)?;
        Ok(CapsuleStopReport {
            witness: self.witness.clone(),
            reaped,
        })
    }
}

fn spawn_guardian(helper: &PinnedHelper, plan: &StaticCapsulePlan) -> Result<(ChildOwner, u32)> {
    let reaper = crate::reaper::Slot::reserve()?;
    let (control, child_socket) = wire::Socket::pair()?;
    let child_fd = child_socket.raw();
    let helper_fd = helper.file.as_raw_fd();
    let mut command = Command::new("/proc/self/fd/4");
    command
        .arg("3")
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::syscall(
                libc::SYS_close_range,
                3u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let image = libc::fcntl(helper_fd, libc::F_DUPFD_CLOEXEC, 64);
            let channel = libc::fcntl(child_fd, libc::F_DUPFD_CLOEXEC, 64);
            if image < 0
                || channel < 0
                || libc::dup2(image, 4) < 0
                || libc::fcntl(4, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                || libc::dup2(channel, 3) < 0
                || libc::fcntl(3, libc::F_SETFD, 0) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let guardian_pid = child.id();
    drop(child_socket);
    let owner = ChildOwner {
        guardian: Some((child, reaper)),
        control: Some(control),
    };
    let socket = owner.control.as_ref().unwrap();
    socket.send(
        &Message::Plan(plan.record.clone()),
        None,
        plan.record.deadline,
    )?;
    for (index, file) in plan.files.iter().enumerate() {
        socket.send(
            &Message::Descriptor {
                nonce: plan.record.nonce,
                index,
            },
            Some(file.as_raw_fd()),
            plan.record.deadline,
        )?;
    }
    Ok((owner, guardian_pid))
}

pub fn helper_main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() == 3 && args[1] == "probe-root-mount" {
        return mounts::probe(Path::new(&args[2]));
    }
    ensure!(
        cfg!(target_arch = "x86_64")
            && std::env::args().collect::<Vec<_>>()
                == vec![std::env::args().next().unwrap(), "3".to_owned()],
        "capsule helper invocation"
    );
    let control = wire::Socket(unsafe { OwnedFd::from_raw_fd(3) });
    guardian::run(control)
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    #[test]
    fn cleanup_timeout_hands_child_to_existing_reaper_without_blocking_wait_or_stop_proof() {
        let slot = crate::reaper::Slot::reserve().unwrap();
        let child = Command::new("/usr/bin/sleep")
            .arg("30")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pidfd = ownership::pidfd(child.id()).unwrap();
        let mut owner = ChildOwner {
            guardian: Some((child, slot)),
            control: None,
        };
        let started = std::time::Instant::now();
        owner.cleanup_until(ownership::raw_boot_ms());
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(owner.guardian.is_none());
        let until = std::time::Instant::now() + Duration::from_secs(1);
        while ownership::pidfd_is_live(&pidfd).unwrap() {
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
