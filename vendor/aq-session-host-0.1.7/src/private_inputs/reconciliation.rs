//! Desktop-owned reconciliation. This is a local transaction journal, not a
//! credential-vault wire protocol. All desktop auth writers must use the same
//! permanent `.aq-auth.lock`; the caller must retain the host root until Complete.
use super::*;
use std::ffi::{CStr, CString};
use std::path::Component;

/// Implemented by the trusted credential owner. Return an acknowledgement only
/// after validating a durable server revision for this exact request. Retries
/// MUST use operation_id as an idempotency key and reject changed payloads.
/// The adapter owns lease authentication, CAS, lineage and recovery authority;
/// a stopped proof does not extend an expired credential lease.
/// The call must have a bounded timeout: the desktop lock is held until return.
pub trait LoginWriteBack {
    fn write_back(&mut self, request: &LoginRefreshRequest<'_>) -> Result<LoginRefreshAck>;
}

/// In-process, deliberately non-serializable input to the future C5b adapter.
/// Private bytes must only enter the adapter's protected transport body.
pub struct LoginRefreshRequest<'a> {
    record: &'a RefreshRecord,
    binding: &'a aq_session_protocol::LaunchBinding,
    material: &'a [u8],
}
impl std::fmt::Debug for LoginRefreshRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginRefreshRequest(<private>)")
    }
}
impl LoginRefreshRequest<'_> {
    pub fn operation_id(&self) -> Uuid {
        self.record.status.operation_id
    }
    pub fn launch_id(&self) -> Uuid {
        self.record.launch_id
    }
    pub fn binding(&self) -> &aq_session_protocol::LaunchBinding {
        self.binding
    }
    pub fn original_record_digest(&self) -> &Digest {
        &self.record.original_record_digest
    }
    pub fn original_auth_digest(&self) -> &Digest {
        &self.record.original_auth_digest
    }
    pub fn refreshed_digest(&self) -> &Digest {
        &self.record.refreshed_digest
    }
    pub fn private_material(&self) -> &[u8] {
        self.material
    }

    /// The trusted adapter calls this only after checking the server's exact
    /// request/revision binding. A local success or accepted HTTP status is not
    /// sufficient. revision is a non-secret reference, never a credential.
    pub fn acknowledge(&self, revision: &str) -> Result<LoginRefreshAck> {
        validate_revision(revision)?;
        Ok(LoginRefreshAck {
            operation_id: self.operation_id(),
            refreshed_digest: self.record.refreshed_digest.clone(),
            revision: revision.to_owned(),
        })
    }
}
pub struct LoginRefreshAck {
    operation_id: Uuid,
    refreshed_digest: Digest,
    revision: String,
}
impl std::fmt::Debug for LoginRefreshAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginRefreshAck(<private>)")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginRefreshPhase {
    PendingOwner,
    OwnerUpdated,
    Acknowledged,
    Complete,
    /// Requires owner intervention. Neither copy is automatically overwritten.
    Conflict,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginRefreshStatus {
    pub operation_id: Uuid,
    pub phase: LoginRefreshPhase,
    pub server_revision: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshRecord {
    schema_version: u32,
    launch_id: Uuid,
    original_auth_digest: Digest,
    original_record_digest: Digest,
    owner_device: u64,
    owner_inode: u64,
    auth_device: u64,
    auth_inode: u64,
    refreshed_digest: Digest,
    refreshed_bytes: u64,
    status: LoginRefreshStatus,
}
const STATE: &CStr = c"chatgpt-refresh.json";
const STATE_NEXT: &CStr = c"chatgpt-refresh.next";
const OWNER_NEXT: &CStr = c"auth.json.aq-refresh";

fn validate_revision(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b)),
        "invalid non-secret refresh revision reference"
    );
    Ok(())
}
fn exists(dir: &File, name: &CStr) -> Result<bool> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if e.kind() == std::io::ErrorKind::NotFound {
        Ok(false)
    } else {
        Err(e.into())
    }
}
fn open_directory(path: &Path) -> Result<File> {
    ensure!(
        path.is_absolute(),
        "reconciliation requires absolute directory"
    );
    let mut dir = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt;
                dir = directory(&dir, &CString::new(name.as_bytes())?)?;
            }
            _ => bail!("invalid reconciliation directory component"),
        }
    }
    private_directory(&dir)?;
    Ok(dir)
}
// Lock files are permanent: unlinking a lock could create two serialization
// domains. File closure releases flock, including on process death.
fn lock(dir: &File, name: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    ensure!(fd >= 0, "credential reconciliation lock unavailable");
    let file = unsafe { File::from_raw_fd(fd) };
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o7777 == 0o600,
        "credential lock identity/mode"
    );
    ensure!(
        unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "credential reconciliation already locked"
    );
    dir.sync_all()?;
    Ok(file)
}
fn unlink(dir: &File, name: &CStr, flags: i32) -> Result<()> {
    ensure!(
        unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } == 0,
        "credential reconciliation cleanup failed"
    );
    dir.sync_all()?;
    Ok(())
}
fn replace(dir: &File, name: &CStr, next: &CStr, bytes: &[u8], secret: bool) -> Result<()> {
    if exists(dir, next)? {
        let (previous, _) = read_member(dir, next, MAX_MATERIAL)?;
        if secret {
            ensure!(
                previous.as_slice() == bytes,
                "different pending owner credential preserved"
            );
        } else {
            // Only metadata staging is discardable; a crash before publication
            // may require replaying the exact operation from the committed state.
            unlink(dir, next, 0)?;
        }
    }
    if !exists(dir, next)? {
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                next.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "credential reconciliation staging unavailable");
        let mut file = unsafe { File::from_raw_fd(fd) };
        // Do not depend on the process umask allowing owner read/write.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    } else {
        // A prior crash left exactly this private owner replacement staged.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                next.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "credential staging unavailable");
        unsafe { File::from_raw_fd(fd) }.sync_all()?;
    }
    ensure!(
        unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                next.as_ptr(),
                dir.as_raw_fd(),
                name.as_ptr(),
            )
        } == 0,
        "credential reconciliation publication failed"
    );
    dir.sync_all()?;
    Ok(())
}
fn save(root: &File, record: &RefreshRecord) -> Result<()> {
    replace(root, STATE, STATE_NEXT, &serde_json::to_vec(record)?, false)
}
fn load(root: &File, pin: &PrivateInputRecord) -> Result<Option<RefreshRecord>> {
    if !exists(root, STATE)? {
        return Ok(None);
    }
    let (bytes, _) = read_member(root, STATE, 16384)?;
    let record: RefreshRecord = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid credential refresh state"))?;
    let chatgpt = pin.chatgpt.as_ref().context("ChatGPT input required")?;
    ensure!(
        record.schema_version == 1
            && record.launch_id == chatgpt.launch_id
            && record.original_auth_digest == pin.auth_digest
            && record.original_record_digest == pin.source.record_digest
            && !record.status.operation_id.is_nil()
            && record.refreshed_bytes > 0
            && record.refreshed_bytes <= MAX_MATERIAL,
        "credential refresh state binding changed"
    );
    if let Some(revision) = &record.status.server_revision {
        validate_revision(revision)?;
    }
    ensure!(
        record.status.phase == LoginRefreshPhase::Conflict
            || record.status.server_revision.is_some()
                == matches!(
                    record.status.phase,
                    LoginRefreshPhase::Acknowledged | LoginRefreshPhase::Complete
                ),
        "credential refresh revision/phase mismatch"
    );
    Ok(Some(record))
}
pub(super) fn cleanup_guard(root: &File, pin: &PrivateInputRecord) -> Result<Option<File>> {
    if pin.chatgpt.is_none() {
        return Ok(None);
    }
    let guard = lock(root, c".aq-refresh.lock")?;
    if let Some(record) = load(root, pin)? {
        ensure!(
            record.status.phase == LoginRefreshPhase::Complete,
            "credential reconciliation incomplete; private cleanup refused"
        );
    }
    Ok(Some(guard))
}
fn private_home(root: &File, pin: &PrivateInputRecord) -> Result<File> {
    let home = directory(root, c"codex-home")?;
    let m = private_directory(&home)?;
    ensure!(
        m.dev() == pin.home_device && m.ino() == pin.home_inode,
        "private refresh home replaced"
    );
    Ok(home)
}
fn refreshed(home: &File, record: &RefreshRecord) -> Result<Zeroizing<Vec<u8>>> {
    let (bytes, m) = read_member(home, c"auth.json", MAX_MATERIAL)?;
    ensure!(
        m.dev() == record.auth_device
            && m.ino() == record.auth_inode
            && bytes.len() as u64 == record.refreshed_bytes
            && Digest::of(&bytes) == record.refreshed_digest,
        "rotated credential changed; preserved"
    );
    PrivateLaunchInput::validate_chatgpt_auth_json(&bytes)?;
    Ok(bytes)
}
fn owner_digest(owner: &File) -> Result<Digest> {
    let (bytes, _) = read_member(owner, c"auth.json", MAX_MATERIAL)?;
    PrivateLaunchInput::validate_chatgpt_auth_json(&bytes)?;
    Ok(Digest::of(&bytes))
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    OwnerPublished,
    AckPersisted,
    BeforeUnlink,
    AuthUnlinked,
}

fn reconcile(
    root: &File,
    pin: &PrivateInputRecord,
    owner: &File,
    writer: &mut impl LoginWriteBack,
    mut boundary: impl FnMut(Boundary) -> Result<()>,
) -> Result<LoginRefreshStatus> {
    let chatgpt = pin.chatgpt.as_ref().context("ChatGPT input required")?;
    ensure!(pin.scoped.is_none(), "mixed credential profiles");
    let owner_meta = private_directory(owner)?;
    let root_meta = private_directory(root)?;
    ensure!(
        (owner_meta.dev(), owner_meta.ino()) != (pin.home_device, pin.home_inode)
            && (owner_meta.dev(), owner_meta.ino()) != (root_meta.dev(), root_meta.ino()),
        "desktop credential owner must be separate from disposable session storage"
    );
    let _root_lock = lock(root, c".aq-refresh.lock")?;
    let _owner_lock = lock(owner, c".aq-auth.lock")?;
    let mut record = if let Some(record) = load(root, pin)? {
        record
    } else {
        let home = private_home(root, pin)?;
        let (bytes, m) = read_member(&home, c"auth.json", MAX_MATERIAL)?;
        PrivateLaunchInput::validate_chatgpt_auth_json(&bytes)?;
        let record = RefreshRecord {
            schema_version: 1,
            launch_id: chatgpt.launch_id,
            original_auth_digest: pin.auth_digest.clone(),
            original_record_digest: pin.source.record_digest.clone(),
            owner_device: owner_meta.dev(),
            owner_inode: owner_meta.ino(),
            auth_device: m.dev(),
            auth_inode: m.ino(),
            refreshed_digest: Digest::of(&bytes),
            refreshed_bytes: bytes.len() as u64,
            status: LoginRefreshStatus {
                operation_id: Uuid::new_v4(),
                phase: LoginRefreshPhase::PendingOwner,
                server_revision: None,
            },
        };
        save(root, &record)?;
        record
    };
    ensure!(
        owner_meta.dev() == record.owner_device && owner_meta.ino() == record.owner_inode,
        "desktop owner directory changed; preserved"
    );
    ensure!(
        record.status.phase != LoginRefreshPhase::Conflict,
        "desktop credential conflict requires owner intervention"
    );
    if record.status.phase == LoginRefreshPhase::Complete {
        ensure!(
            !exists(root, c"codex-home")?,
            "completed credential home reappeared"
        );
        return Ok(record.status);
    }
    let current = owner_digest(owner)?;
    let pending = record.status.phase == LoginRefreshPhase::PendingOwner;
    if current != record.refreshed_digest && !(pending && current == record.original_auth_digest) {
        record.status.phase = LoginRefreshPhase::Conflict;
        save(root, &record)?;
        bail!("desktop credential changed; both copies preserved");
    }
    if pending {
        let bytes = refreshed(&private_home(root, pin)?, &record)?;
        // Even same-byte recovery repeats the fsync boundary after an uncertain
        // rename. The original material installation remains immutable.
        replace(owner, c"auth.json", OWNER_NEXT, &bytes, true)?;
        boundary(Boundary::OwnerPublished)?;
        record.status.phase = LoginRefreshPhase::OwnerUpdated;
        save(root, &record)?;
    }
    if record.status.phase == LoginRefreshPhase::OwnerUpdated {
        let bytes = refreshed(&private_home(root, pin)?, &record)?;
        let request = LoginRefreshRequest {
            record: &record,
            binding: &chatgpt.binding,
            material: &bytes,
        };
        // An adapter error may contain a response body. Never expose it through
        // the host's diagnostics, and never infer acknowledgement from failure.
        let ack = writer.write_back(&request).map_err(|_| {
            anyhow::anyhow!("credential write-back unacknowledged; rotated copy retained")
        })?;
        ensure!(
            ack.operation_id == record.status.operation_id
                && ack.refreshed_digest == record.refreshed_digest,
            "credential write-back acknowledgement binding mismatch"
        );
        validate_revision(&ack.revision)?;
        record.status.server_revision = Some(ack.revision);
        record.status.phase = LoginRefreshPhase::Acknowledged;
        save(root, &record)?;
        boundary(Boundary::AckPersisted)?;
    }
    // Cooperating writers are excluded by the lock. Also catch an operator or
    // non-cooperating writer changing the owner during the protected exchange.
    if owner_digest(owner)? != record.refreshed_digest {
        record.status.phase = LoginRefreshPhase::Conflict;
        save(root, &record)?;
        bail!("desktop credential changed during write-back; rotated copy retained");
    }
    if exists(root, c"codex-home")? {
        let home = private_home(root, pin)?;
        if exists(&home, c"auth.json")? {
            drop(refreshed(&home, &record)?);
            boundary(Boundary::BeforeUnlink)?;
            unlink(&home, c"auth.json", 0)?;
            boundary(Boundary::AuthUnlinked)?;
        } else {
            home.sync_all()?;
        }
        // Never recursively remove unknown generated files. A cleanup failure
        // leaves Acknowledged durable and retries cleanup without another write.
        unlink(root, c"codex-home", libc::AT_REMOVEDIR)?;
    } else {
        root.sync_all()?;
    }
    record.status.phase = LoginRefreshPhase::Complete;
    save(root, &record)?;
    Ok(record.status)
}

impl SessionHandle {
    fn refresh_pin(&self, proof: &StoppedProof) -> Result<PrivateInputRecord> {
        ensure!(
            proof.guardian() == &self.identity
                && proof.observation().phase == HostPhase::Stopped
                && proof.observation().kernel_wait_echild,
            "credential reconciliation requires exact ECHILD proof"
        );
        let config: SealedLaunch = read_json(&self.directory.join("launch.json"))?;
        ensure!(
            config.intent()? == self.identity.intent,
            "credential reconciliation configuration changed"
        );
        let pin = config
            .private_input
            .context("private ChatGPT input absent")?;
        ensure!(
            pin.chatgpt.is_some() && pin.scoped.is_none(),
            "private ChatGPT input required"
        );
        Ok(pin)
    }
    /// Explicit desktop-only operation; owner_home is its configured Codex home,
    /// never a sandbox-supplied path. Both directories must be private (0700),
    /// owned by this uid and free of symlink components. All owner writers must
    /// cooperate on `.aq-auth.lock`. Retain container/state on every error.
    /// Only Complete permits the credential portion of a disposal gate to pass;
    /// other source/artifact retention requirements still apply.
    pub fn reconcile_chatgpt_login(
        &mut self,
        proof: &StoppedProof,
        owner_home: &Path,
        writer: &mut impl LoginWriteBack,
    ) -> Result<LoginRefreshStatus> {
        let pin = self.refresh_pin(proof)?;
        let root = open_directory(&self.directory)?;
        let owner = open_directory(owner_home)?;
        let status = reconcile(&root, &pin, &owner, writer, |_| Ok(()))?;
        self.private_source.take();
        Ok(status)
    }
    /// Durable status only, not disposal authority or renewed execution rights.
    pub fn chatgpt_refresh_status(
        &self,
        proof: &StoppedProof,
    ) -> Result<Option<LoginRefreshStatus>> {
        let pin = self.refresh_pin(proof)?;
        let root = open_directory(&self.directory)?;
        let _lock = lock(&root, c".aq-refresh.lock")?;
        Ok(load(&root, &pin)?.map(|record| record.status))
    }
}

#[cfg(test)]
mod tests;
