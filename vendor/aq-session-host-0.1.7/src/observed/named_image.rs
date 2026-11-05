//! Separately qualified named readonly image; never a memfd fallback.
use super::{
    executable::{ElfIdentity, ImageProfile},
    named_environment::{NamedReadonlyEnvironment, NamespaceIdentity, verify_namespace},
};
use crate::*;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    os::{fd::AsRawFd, unix::fs::FileExt},
    sync::{Arc, Mutex},
};
pub const MAX_NAMED_EXEC_BYTES: u64 = 512 * 1024 * 1024;
pub const MAX_NAMED_POOL_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_NAMED_POOL_ENTRIES: usize = 2;
const MAX_AUXILIARY_BYTES: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NamedImageRole {
    Program,
    Auxiliary,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamedReadonlyIdentity {
    pub source: PathBuf,
    pub qualification: ArtifactRef,
    pub mount_namespace: NamespaceIdentity,
    pub user_namespace: NamespaceIdentity,
    pub mount_id: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub modified: i64,
    pub modified_ns: i64,
    pub changed: i64,
    pub changed_ns: i64,
    pub role: NamedImageRole,
}
struct Image {
    identity: ElfIdentity,
    file: File,
}
struct State {
    bytes: u64,
    programs: usize,
    auxiliaries: usize,
    images: BTreeMap<(bool, String, String, String), Arc<Image>>,
}
struct Pool {
    pid: u32,
    bytes: u64,
    entries: usize,
    environment: NamedReadonlyEnvironment,
    state: Mutex<State>,
}
/// At most two program images and two fixed auxiliary files per installation.
/// All actual bytes share one explicit budget; no copies, eviction or renewal.
#[derive(Clone)]
pub struct NamedReadonlyExecPool {
    inner: Arc<Pool>,
}
pub struct NamedReadonlyFile {
    image: Arc<Image>,
    pool: Arc<Pool>,
}
pub struct NamedReadonlyExecImage(pub(super) NamedReadonlyFile);
impl NamedReadonlyExecPool {
    pub fn new(bytes: u64, entries: usize, environment: NamedReadonlyEnvironment) -> Result<Self> {
        ensure!(
            bytes > 0
                && bytes <= MAX_NAMED_POOL_BYTES
                && entries > 0
                && entries <= MAX_NAMED_POOL_ENTRIES,
            "named readonly pool bounds"
        );
        environment.verify_owner()?;
        Ok(Self {
            inner: Arc::new(Pool {
                pid: std::process::id(),
                bytes,
                entries,
                environment,
                state: Mutex::new(State {
                    bytes: 0,
                    programs: 0,
                    auxiliaries: 0,
                    images: BTreeMap::new(),
                }),
            }),
        })
    }
    pub fn qualification(&self) -> &ArtifactRef {
        self.inner.environment.qualification()
    }
    pub fn load_verified(
        &self,
        source: &Path,
        expected: &ArtifactRef,
    ) -> Result<NamedReadonlyExecImage> {
        Ok(NamedReadonlyExecImage(self.load(
            source,
            expected,
            NamedImageRole::Program,
        )?))
    }
    pub fn load_auxiliary_verified(
        &self,
        source: &Path,
        expected: &ArtifactRef,
    ) -> Result<NamedReadonlyFile> {
        self.load(source, expected, NamedImageRole::Auxiliary)
    }
    fn load(
        &self,
        source: &Path,
        expected: &ArtifactRef,
        role: NamedImageRole,
    ) -> Result<NamedReadonlyFile> {
        ensure!(
            self.inner.pid == std::process::id(),
            "named pool belongs to original process"
        );
        self.inner.environment.verify_owner()?;
        expected.id.validate()?;
        expected.version.validate()?;
        ensure!(
            source.is_absolute()
                && source.components().count() <= 64
                && std::fs::canonicalize(source)? == source,
            "named source must be a bounded canonical route"
        );
        let program = role == NamedImageRole::Program;
        let key = (
            program,
            expected.id.as_str().into(),
            expected.version.as_str().into(),
            expected.digest.as_str().into(),
        );
        let mut state = self
            .inner
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("named pool busy or poisoned"))?;
        if let Some(image) = state.images.get(&key) {
            ensure!(
                named(&image.identity)?.source == source,
                "named image source binding changed"
            );
            verify_received(&image.file, &image.identity)?;
            return Ok(NamedReadonlyFile {
                image: image.clone(),
                pool: self.inner.clone(),
            });
        }
        ensure!(
            if program {
                state.programs < self.inner.entries
            } else {
                state.auxiliaries < self.inner.entries * 2
            },
            "named image entry quota"
        );
        let file = open(source)?;
        let m = file.metadata()?;
        let limit = if program {
            MAX_NAMED_EXEC_BYTES
        } else {
            MAX_AUXILIARY_BYTES
        };
        ensure!(
            m.is_file()
                && (64..=limit).contains(&m.len())
                && m.nlink() == 1
                && m.mode() & 0o111 != 0
                && m.mode() & 0o6022 == 0
                && (m.uid() == 0 || m.uid() == unsafe { libc::geteuid() }),
            "named image file type/size/ownership/mode/link bound"
        );
        let reserved = state
            .bytes
            .checked_add(m.len())
            .context("named byte quota overflow")?;
        ensure!(reserved <= self.inner.bytes, "named image byte quota");
        readonly(&file)?;
        readonly_ancestors(source)?;
        self.inner.environment.verify_aliases(m.dev(), m.ino())?;
        let (mount_namespace, user_namespace) = self.inner.environment.identities();
        let identity = ElfIdentity {
            artifact: expected.clone(),
            bytes: m.len(),
            device: m.dev(),
            inode: m.ino(),
            profile: Some(ImageProfile::NamedReadonlyExecStdinV1(Box::new(
                NamedReadonlyIdentity {
                    source: source.into(),
                    qualification: self.qualification().clone(),
                    mount_namespace,
                    user_namespace,
                    mount_id: mount_id(&file)?,
                    mode: m.mode(),
                    uid: m.uid(),
                    gid: m.gid(),
                    modified: m.mtime(),
                    modified_ns: m.mtime_nsec(),
                    changed: m.ctime(),
                    changed_ns: m.ctime_nsec(),
                    role: role.clone(),
                },
            ))),
        };
        hash_once(&file, &identity)?;
        verify_received(&file, &identity)?;
        let image = Arc::new(Image { identity, file });
        state.bytes = reserved;
        if program {
            state.programs += 1
        } else {
            state.auxiliaries += 1
        }
        state.images.insert(key, image.clone());
        Ok(NamedReadonlyFile {
            image,
            pool: self.inner.clone(),
        })
    }
    pub fn charged_bytes(&self) -> Result<u64> {
        ensure!(
            self.inner.pid == std::process::id(),
            "named pool owner changed"
        );
        Ok(self
            .inner
            .state
            .try_lock()
            .map_err(|_| anyhow::anyhow!("named pool busy or poisoned"))?
            .bytes)
    }
}
impl NamedReadonlyFile {
    pub fn artifact(&self) -> &ArtifactRef {
        &self.image.identity.artifact
    }
    pub fn bytes(&self) -> u64 {
        self.image.identity.bytes
    }
    pub fn source(&self) -> &Path {
        &named(&self.image.identity)
            .expect("private named owner")
            .source
    }
    pub fn revalidate(&self) -> Result<()> {
        self.pool.environment.verify_owner()?;
        verify_received(&self.image.file, &self.image.identity)
    }
    pub(super) fn identity(&self) -> &ElfIdentity {
        &self.image.identity
    }
    pub(super) fn file(&self) -> &File {
        &self.image.file
    }
}
impl NamedReadonlyExecImage {
    pub fn artifact(&self) -> &ArtifactRef {
        self.0.artifact()
    }
    pub fn bytes(&self) -> u64 {
        self.0.bytes()
    }
    pub fn revalidate(&self) -> Result<()> {
        self.0.revalidate()
    }
}
pub(super) fn named(identity: &ElfIdentity) -> Result<&NamedReadonlyIdentity> {
    match &identity.profile {
        Some(ImageProfile::NamedReadonlyExecStdinV1(e)) => Ok(e),
        _ => bail!("named image profile absent"),
    }
}
fn open(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?)
}
fn readonly(file: &File) -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) } == 0,
        "named filesystem state unavailable"
    );
    ensure!(
        unsafe { stat.assume_init() }.f_flag & libc::ST_RDONLY != 0,
        "named image is not on a readonly mount"
    );
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    ensure!(
        flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY,
        "named retained descriptor is writable"
    );
    Ok(())
}
fn readonly_ancestors(path: &Path) -> Result<()> {
    for parent in path.ancestors().skip(1) {
        readonly(&open(parent)?)?;
    }
    Ok(())
}
fn mount_id(file: &File) -> Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::uninit();
    ensure!(
        unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_STATX_DONT_SYNC,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } == 0,
        "named statx unavailable"
    );
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.stx_mask & libc::STATX_MNT_ID != 0 && stat.stx_mnt_id > 0,
        "named mount identity absent"
    );
    Ok(stat.stx_mnt_id)
}
/// O(1) in executable byte count. No hashing/copying/recovery adoption here.
pub(super) fn verify_received(file: &File, identity: &ElfIdentity) -> Result<()> {
    let e = named(identity)?;
    e.qualification.id.validate()?;
    e.qualification.version.validate()?;
    verify_namespace(&e.mount_namespace, &e.user_namespace)?;
    readonly(file)?;
    let m = file.metadata()?;
    let limit = if e.role == NamedImageRole::Program {
        MAX_NAMED_EXEC_BYTES
    } else {
        MAX_AUXILIARY_BYTES
    };
    ensure!(
        m.is_file()
            && m.dev() == identity.device
            && m.ino() == identity.inode
            && m.len() == identity.bytes
            && (64..=limit).contains(&m.len())
            && m.nlink() == 1
            && m.mode() == e.mode
            && m.uid() == e.uid
            && m.gid() == e.gid
            && m.mtime() == e.modified
            && m.mtime_nsec() == e.modified_ns
            && m.ctime() == e.changed
            && m.ctime_nsec() == e.changed_ns
            && mount_id(file)? == e.mount_id,
        "named retained image identity changed"
    );
    ensure!(
        e.source.is_absolute()
            && e.source.components().count() <= 64
            && std::fs::canonicalize(&e.source)? == e.source,
        "named canonical route changed"
    );
    let route = open(&e.source)?;
    readonly(&route)?;
    let current = route.metadata()?;
    ensure!(
        current.dev() == identity.device
            && current.ino() == identity.inode
            && current.len() == identity.bytes
            && mount_id(&route)? == e.mount_id,
        "named route no longer resolves to retained image"
    );
    Ok(())
}
fn hash_once(file: &File, identity: &ElfIdentity) -> Result<()> {
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut buffer = [0; 65536];
    let mut header = [0; 64];
    while offset < identity.bytes {
        let take = (identity.bytes - offset).min(buffer.len() as u64) as usize;
        let n = file.read_at(&mut buffer[..take], offset)?;
        ensure!(n > 0, "named image truncated");
        if offset == 0 {
            ensure!(n >= 64, "named ELF header truncated");
            header.copy_from_slice(&buffer[..64]);
        }
        hash.update(&buffer[..n]);
        offset += n as u64;
    }
    ensure!(
        format!("{:x}", hash.finalize()) == identity.artifact.digest.as_str(),
        "named exact byte digest mismatch"
    );
    ensure!(
        header[..7] == *b"\x7fELF\x02\x01\x01"
            && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3),
        "named ELF header/type unsupported"
    );
    #[cfg(target_arch = "x86_64")]
    ensure!(
        u16::from_le_bytes([header[18], header[19]]) == 62,
        "named ELF architecture mismatch"
    );
    #[cfg(target_arch = "aarch64")]
    ensure!(
        u16::from_le_bytes([header[18], header[19]]) == 183,
        "named ELF architecture mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_session_protocol::Text;
    fn artifact(bytes: &[u8]) -> ArtifactRef {
        ArtifactRef {
            id: Text::new("aq.named-kernel-fixture").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(bytes),
        }
    }
    #[test]
    fn named_preflight_checks_actual_bytes_architecture_and_executable_type() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("elf");
        let mut bytes = vec![0; 128];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16] = 2;
        bytes[18] = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        let metadata = file.metadata().unwrap();
        let mut identity = ElfIdentity {
            artifact: artifact(&bytes),
            bytes: bytes.len() as u64,
            device: metadata.dev(),
            inode: metadata.ino(),
            profile: None,
        };
        hash_once(&file, &identity).unwrap();
        identity.artifact.digest = Digest::of(b"wrong");
        assert!(hash_once(&file, &identity).is_err());
        bytes[18] = 0;
        std::fs::write(&path, &bytes).unwrap();
        identity.artifact = artifact(&bytes);
        assert!(hash_once(&file, &identity).is_err());
        bytes[18] = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        bytes[16] = 1;
        std::fs::write(&path, &bytes).unwrap();
        identity.artifact = artifact(&bytes);
        assert!(hash_once(&file, &identity).is_err());
    }
    #[test]
    #[ignore = "requires explicit readonly namespace fixture; hashes host ELF only, never launches Codex"]
    fn named_readonly_kernel_image_is_charged_once_and_bound_to_live_route() {
        assert_eq!(
            std::env::var("AQ_NATIVE_FIXTURE_CONFINED").as_deref(),
            Ok("v1")
        );
        let qualification = artifact(b"independent fixture environment only; no runtime authority");
        let environment = NamedReadonlyEnvironment::inspect(qualification).unwrap();
        let pool = NamedReadonlyExecPool::new(64 * 1024 * 1024, 1, environment).unwrap();
        let source = Path::new("/opt/aq-qualification/aq-session-host");
        let mut reference = artifact(b"unused");
        reference.digest = serde_json::from_value(serde_json::json!(
            std::env::var("AQ_NATIVE_HOST_SHA256").unwrap()
        ))
        .unwrap();
        let size = source.metadata().unwrap().len();
        let mut wrong = reference.clone();
        wrong.digest = Digest::of(b"wrong");
        assert!(pool.load_verified(source, &wrong).is_err());
        assert_eq!(pool.charged_bytes().unwrap(), 0);
        let image = pool.load_verified(source, &reference).unwrap();
        assert_eq!(image.bytes(), size);
        assert_eq!(pool.charged_bytes().unwrap(), size);
        for _ in 0..32 {
            let again = pool.load_verified(source, &reference).unwrap();
            assert!(Arc::ptr_eq(&again.0.image, &image.0.image));
            assert_eq!(pool.charged_bytes().unwrap(), size);
        }
        assert!(image.0.file().write_at(b"x", 0).is_err());
        assert!(image.0.file().set_len(size + 1).is_err());
        let writable = OpenOptions::new().write(true).open(source).unwrap_err();
        // Owner0500 fixtures can reject at DAC before the mount check. The
        // opaque constructor has independently verified ST_RDONLY on this fd.
        assert!(matches!(
            writable.raw_os_error(),
            Some(libc::EROFS | libc::EACCES)
        ));
        let mut substitution = image.0.identity().clone();
        if let Some(ImageProfile::NamedReadonlyExecStdinV1(e)) = &mut substitution.profile {
            e.mount_id += 1;
        }
        assert!(verify_received(image.0.file(), &substitution).is_err());
        let alias = Path::new("/var/lib/aq-qualification/image-alias");
        std::os::unix::fs::symlink(source, alias).unwrap();
        assert!(pool.load_verified(alias, &reference).is_err());
        std::fs::remove_file(alias).unwrap();
        let mut new = reference;
        new.id = Text::new("another-program").unwrap();
        assert!(pool.load_verified(source, &new).is_err());
    }
}
