//! `aq result submit|status`: the worker-local result claim (spec 0006 §9
//! step 4), contract `aq-agent-result-local-1`. It is not a server command:
//! the CLI writes one bounded claim into the attempt's local outbox
//! (`AQ_AGENT_RESULT_DIR`) and reads the worker's acknowledgment. No network,
//! no credential and no execution, stop, source or Graph-completion authority.
//!
//! This is a port of aq-worker `aq-agent-client` `completion.rs` (and the parts
//! of `files.rs` it uses) on the vendored contract crate, so the files written,
//! the documents printed and the exit statuses are the same as
//! `aq-agent result` (pinned by `tests/golden/result.json`).
use anyhow::{Context, Result, ensure};
use aq_agent_result_local::{
    self as wire, Acknowledgment, Binding, Phase, Presence, Submission, SubmissionInput,
};
use nix::libc;
use serde_json::{Value, json};
use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

/// One outcome: the local result document and its exit status.
pub struct Outcome {
    pub value: Value,
    pub exit_code: u8,
}

/// What an unusable channel or request prints (stderr, exit 15), as `aq-agent`
/// did: never a raw parser, filesystem or path error.
pub fn refused() -> Value {
    json!({"code":"invalid_contract_or_private_state","effect":"not_asserted","retryable":false})
}

/// `aq result submit --request-file F` or `aq result status`, against
/// `AQ_AGENT_RESULT_DIR`. An absent variable is refused, never a fallback.
pub fn run(submit: Option<&Path>) -> Result<Outcome> {
    let root = std::env::var_os(wire::RESULT_DIR_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("result runtime channel unavailable")?;
    let client = ResultClient::open(&root)?;
    match submit {
        Some(request) => client.submit(read_input(request)?),
        None => client.status(),
    }
}

fn member(value: &str) -> Result<CString> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            && !matches!(value, "." | ".."),
        "invalid private member"
    );
    Ok(CString::new(value)?)
}
fn not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}
fn euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// A private (owner-only, 0700) directory held by descriptor; members are
/// opened relative to it without following links.
struct PrivateDir {
    path: PathBuf,
    dir: File,
}
impl PrivateDir {
    fn open(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && std::fs::canonicalize(path)? == path,
            "private root must be canonical and have no symlink components"
        );
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let m = dir.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == euid() && m.mode() & 0o077 == 0,
            "private root ownership or permissions refused"
        );
        Ok(Self {
            path: path.into(),
            dir,
        })
    }
    fn check_route(&self) -> Result<()> {
        let held = self.dir.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            std::fs::canonicalize(&self.path)? == self.path
                && named.is_dir()
                && held.dev() == named.dev()
                && held.ino() == named.ino()
                && named.uid() == euid()
                && named.mode() & 0o077 == 0,
            "private directory route changed"
        );
        Ok(())
    }
    fn child(&self, name: &str) -> Result<Self> {
        member(name)?;
        self.check_route()?;
        let child = Self::open(&self.path.join(name))?;
        self.check_route()?;
        child.check_route()?;
        Ok(child)
    }
    /// Existing member only; never creates a worker-owned control/lock file.
    fn existing(&self, name: &str) -> Result<File> {
        self.check_route()?;
        self.open_member(name, false)
    }
    fn open_member(&self, name: &str, create: bool) -> Result<File> {
        let c = member(name)?;
        let flags = if create {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        };
        let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        let fd = unsafe { libc::openat(self.dir.as_raw_fd(), c.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let f = unsafe { File::from_raw_fd(fd) };
        let m = f.metadata()?;
        ensure!(
            m.is_file() && m.uid() == euid() && m.mode() & 0o077 == 0 && m.nlink() == 1,
            "private file ownership or permissions refused"
        );
        Ok(f)
    }
    fn read(&self, name: &str, limit: usize) -> Result<Vec<u8>> {
        let f = self.open_member(name, false)?;
        ensure!(
            f.metadata()?.len() <= limit as u64,
            "private file exceeds bound"
        );
        let mut bytes = vec![];
        f.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "private file exceeds bound");
        Ok(bytes)
    }
    fn optional(&self, name: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        match self.read(name, limit) {
            Ok(b) => Ok(Some(b)),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
    /// Publish `bytes` as `name` only if it does not exist (`RENAME_NOREPLACE`).
    fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.check_route()?;
        let dest = member(name)?;
        let temp = member(&format!(".{}.tmp", Uuid::new_v4()))?;
        let flags =
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let fd = unsafe { libc::openat(self.dir.as_raw_fd(), temp.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let mut f = unsafe { File::from_raw_fd(fd) };
            f.write_all(bytes)?;
            f.sync_all()?;
            // Linux is the qualified platform. Refuse rather than fall back to
            // a replacing rename if NOREPLACE is unavailable. The raw syscall,
            // because the static musl build's libc has no `renameat2` wrapper.
            let d = self.dir.as_raw_fd() as libc::c_long;
            let (from, to) = (temp.as_ptr(), dest.as_ptr());
            let no_replace = libc::RENAME_NOREPLACE as libc::c_long;
            let renamed = unsafe { libc::syscall(libc::SYS_renameat2, d, from, d, to, no_replace) };
            if renamed < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            self.dir.sync_all()?;
            Ok(())
        })();
        let _ = unsafe { libc::unlinkat(self.dir.as_raw_fd(), temp.as_ptr(), 0) };
        result
    }
    fn lock_named(&self, name: &str) -> Result<File> {
        self.check_route()?;
        let f = self.open_member(name, true)?;
        flock(&f, libc::LOCK_EX)?;
        self.dir.sync_all()?;
        Ok(f)
    }
}
/// Non-blocking `flock` (fs2's `try_lock_*`): `WouldBlock` when another holds it.
fn flock(f: &File, operation: libc::c_int) -> std::io::Result<()> {
    if unsafe { libc::flock(f.as_raw_fd(), operation | libc::LOCK_NB) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub struct ResultClient {
    root: PrivateDir,
    submissions: PrivateDir,
    binding: Binding,
    binding_file: File,
    binding_metadata: Metadata,
    binding_bytes: Vec<u8>,
}
impl ResultClient {
    /// Only opens an actually installed channel. Never provisions a directory,
    /// grant, control lock or replacement binding from agent input.
    pub fn open(root: &Path) -> Result<Self> {
        let root = PrivateDir::open(root)?;
        let mut binding_file = root.existing(wire::BINDING_FILE)?;
        let binding_metadata = binding_file.metadata()?;
        ensure!(
            binding_metadata.len() <= wire::MAX_BYTES as u64,
            "result binding byte bound"
        );
        let mut binding_bytes = vec![];
        Read::by_ref(&mut binding_file)
            .take(wire::MAX_BYTES as u64 + 1)
            .read_to_end(&mut binding_bytes)?;
        let binding: Binding = wire::decode(&binding_bytes)?;
        let submissions = root.child(wire::SUBMISSIONS_DIRECTORY)?;
        let result = Self {
            root,
            submissions,
            binding,
            binding_file,
            binding_metadata,
            binding_bytes,
        };
        result.check_binding()?;
        Ok(result)
    }
    fn check_binding(&self) -> Result<()> {
        self.root.check_route()?;
        self.submissions.check_route()?;
        let held = self.binding_file.metadata()?;
        let named = self.root.existing(wire::BINDING_FILE)?.metadata()?;
        ensure!(
            same_file(&held, &self.binding_metadata)
                && same_file(&named, &self.binding_metadata)
                && self.root.read(wire::BINDING_FILE, wire::MAX_BYTES)? == self.binding_bytes,
            "result binding or route changed"
        );
        Ok(())
    }
    fn existing_submission(&self) -> Result<Option<Submission>> {
        self.submissions
            .optional(wire::SUBMISSION_FILE, wire::MAX_BYTES)?
            .map(|bytes| {
                let value: Submission = wire::decode(&bytes)?;
                ensure!(
                    value.binding == self.binding,
                    "result submission belongs to another scope"
                );
                Ok(value)
            })
            .transpose()
    }
    fn acknowledgment(&self, submission: &Submission) -> Result<Option<Acknowledgment>> {
        self.root
            .optional(wire::ACK_FILE, wire::MAX_BYTES)?
            .map(|bytes| {
                let ack: Acknowledgment = wire::decode(&bytes)?;
                ack.validate_for(submission)?;
                Ok(ack)
            })
            .transpose()
    }
    fn owner_present(&self, new_submission: bool) -> Result<bool> {
        let Some(bytes) = self.root.optional(wire::PRESENCE_FILE, wire::MAX_BYTES)? else {
            return Ok(false);
        };
        let presence: Presence = wire::decode(&bytes)?;
        let (boot, now) = kernel_clock()?;
        let matches = if new_submission {
            presence.matches_at(&self.binding, boot, now)?
        } else {
            presence.matches_owner_projection(&self.binding, boot)?
        };
        if !matches {
            return Ok(false);
        }
        // Existing lock only. Acquiring a lock here would be owner ABSENCE,
        // never permission to act as the owner. No PID JSON is trusted.
        let lock = match self.root.existing(wire::OWNER_LOCK_FILE) {
            Ok(file) => file,
            Err(error) if not_found(&error) => return Ok(false),
            Err(error) => return Err(error),
        };
        match flock(&lock, libc::LOCK_SH) {
            Ok(()) => {
                flock(&lock, libc::LOCK_UN)?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
    fn ids(&self) -> Value {
        json!({"contract":wire::CONTRACT,"operation_id":self.binding.operation_id,"report_id":self.binding.report_id})
    }
    fn with(&self, fields: Value) -> Value {
        let mut value = self.ids();
        for (k, v) in fields.as_object().into_iter().flatten() {
            value[k] = v.clone();
        }
        value
    }
    fn view(
        &self,
        submission: Option<&Submission>,
        ack: Option<&Acknowledgment>,
        present: bool,
    ) -> Result<Outcome> {
        let (phase, exit_code) = match ack {
            Some(ack) if matches!(ack.phase, Phase::Recorded { .. }) => {
                (serde_json::to_value(&ack.phase)?, 0)
            }
            Some(ack) if matches!(ack.phase, Phase::NeedsReconciliation { .. }) => {
                (serde_json::to_value(&ack.phase)?, 13)
            }
            Some(ack) if present => (serde_json::to_value(&ack.phase)?, 10),
            _ if !present => (
                json!({"kind":"needs_reconciliation","code":"owner_unavailable"}),
                13,
            ),
            _ if submission.is_some() => (json!({"kind":"pending_local"}), 10),
            _ => (json!({"kind":"ready_for_claim"}), 0),
        };
        let value = self.with(json!({
            "binding_digest":self.binding.digest()?,"expected_outcome":self.binding.expected_outcome,
            "submission_digest":submission.map(Submission::digest).transpose()?,"phase":phase,
            "owner_present":present,"projection":"local_result_data_only"}));
        Ok(Outcome { value, exit_code })
    }
    pub fn status(&self) -> Result<Outcome> {
        self.check_binding()?;
        let submission = self.existing_submission()?;
        let ack = submission
            .as_ref()
            .map(|s| self.acknowledgment(s))
            .transpose()?
            .flatten();
        // A worker ack without its retained original local claim is not a
        // replacement claim or proof that an absent file had no prior effect.
        ensure!(
            submission.is_some()
                || self
                    .root
                    .optional(wire::ACK_FILE, wire::MAX_BYTES)?
                    .is_none(),
            "result acknowledgment has no retained original claim"
        );
        // Without an original claim, Ready must still mean the original
        // execution ceiling permits publication. A retained claim may continue
        // finalizing under its completion owner after that ceiling expires.
        let present = self.owner_present(submission.is_none())?;
        self.check_binding()?;
        self.view(submission.as_ref(), ack.as_ref(), present)
    }
    fn conflict(&self) -> Outcome {
        Outcome {
            value: self.with(json!({"code":"result_claim_conflict","effect":"original_preserved","retryable":false})),
            exit_code: 11,
        }
    }
    pub fn submit(&self, input: SubmissionInput) -> Result<Outcome> {
        self.check_binding()?;
        let candidate = Submission::new(self.binding.clone(), input)?;
        let _writer = self.submissions.lock_named(wire::WRITER_LOCK_FILE)?;
        if let Some(existing) = self.existing_submission()? {
            return self.replay_submission(&candidate, Some(&existing));
        }
        if self
            .root
            .optional(wire::ACK_FILE, wire::MAX_BYTES)?
            .is_some()
        {
            return Ok(Outcome {
                value: self.with(json!({"phase":{"kind":"needs_reconciliation","code":"storage_uncertain"},"projection":"local_result_data_only"})),
                exit_code: 13,
            });
        }
        if !self.owner_present(true)? {
            return self.view(None, None, false);
        }
        self.check_binding()?;
        let bytes = wire::encode(&candidate)?;
        if let Err(error) = self.submissions.write_new(wire::SUBMISSION_FILE, &bytes) {
            return self.write_failure(&candidate, error);
        }
        if self.check_binding().is_err() {
            return self.unknown(&candidate);
        }
        // Publication can race cancellation/expiry. Only the actual worker may
        // accept it; do not manufacture an Accepted acknowledgment here.
        self.wait_for_acknowledgment()
            .or_else(|_| self.unknown(&candidate))
    }
    fn replay_submission(
        &self,
        candidate: &Submission,
        existing: Option<&Submission>,
    ) -> Result<Outcome> {
        if existing == Some(candidate) {
            self.status()
        } else {
            Ok(self.conflict())
        }
    }
    fn write_failure(&self, candidate: &Submission, error: anyhow::Error) -> Result<Outcome> {
        // A competing publication keeps the original identity and bytes available for recovery.
        let exists = error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists);
        if exists {
            return self.replay_submission(candidate, self.existing_submission()?.as_ref());
        }
        self.unknown(candidate)
    }
    /// One bounded local wait after NEW publication only. It never retries,
    /// replaces an intent or renews its original execution ceiling. Pending on
    /// timeout is honest uncertainty; only worker-published matching ACKs count.
    fn wait_for_acknowledgment(&self) -> Result<Outcome> {
        let presence: Presence =
            wire::decode(&self.root.read(wire::PRESENCE_FILE, wire::MAX_BYTES)?)?;
        let (boot, now) = kernel_clock()?;
        if !presence.matches_at(&self.binding, boot, now)? {
            return self.status();
        }
        let until = presence
            .original_execution_deadline_boot_ns
            .min(now.saturating_add(2_000_000_000));
        loop {
            let outcome = self.status()?;
            if outcome.value["phase"]["kind"] != "pending_local" {
                return Ok(outcome);
            }
            let (current_boot, now) = kernel_clock()?;
            if current_boot != boot || now >= until {
                return Ok(outcome);
            }
            std::thread::sleep(Duration::from_nanos((until - now).min(20_000_000)));
        }
    }
    fn unknown(&self, submission: &Submission) -> Result<Outcome> {
        Ok(Outcome {
            value: self.with(json!({"submission_digest":submission.digest()?,"code":"local_submission_unknown","effect":"possible","retryable":false})),
            exit_code: 13,
        })
    }
}
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
/// The kernel boot ID and `CLOCK_BOOTTIME` in nanoseconds.
fn kernel_clock() -> Result<(Uuid, u64)> {
    let mut boot = vec![];
    File::open("/proc/sys/kernel/random/boot_id")?
        .take(65)
        .read_to_end(&mut boot)?;
    ensure!(boot.len() <= 64, "kernel boot identity bound");
    let boot = std::str::from_utf8(&boot)?.trim().parse()?;
    let time = nix::time::clock_gettime(nix::time::ClockId::CLOCK_BOOTTIME)?;
    ensure!(
        time.tv_sec() >= 0 && (0..1_000_000_000).contains(&time.tv_nsec()),
        "boottime unavailable"
    );
    let now = (time.tv_sec() as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(time.tv_nsec() as u64))
        .context("boottime overflow")?;
    Ok((boot, now))
}

/// Untrusted input is a bounded regular file, not a FIFO/symlink or an arbitrary
/// transport. Its bytes never select framework operations or credentials.
fn read_input(path: &Path) -> Result<SubmissionInput> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.nlink() == 1
            && metadata.len() <= wire::MAX_INPUT_BYTES as u64,
        "result request file shape/bound"
    );
    let mut bytes = vec![];
    file.take(wire::MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(wire::decode_input(&bytes)?)
}
