//! Necessary kernel premises for an independently qualified readonly image.
//! These checks do not qualify secret visibility or prove that user namespaces
//! cannot be created. Provisioning must qualify immutable mount locking and
//! keep outside privileged actors from changing active image/namespace topology.
use crate::*;
use std::sync::Arc;
const MAX_MOUNTINFO: u64 = 1024 * 1024;
const MAX_FDS: usize = 4096;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}
impl NamespaceIdentity {
    fn of(file: &File) -> Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
        })
    }
}
struct Environment {
    pid: u32,
    reference: ArtifactRef,
    mount: File,
    user: File,
    mount_identity: NamespaceIdentity,
    user_identity: NamespaceIdentity,
}
/// Opaque live kernel observation plus an independently selected environment
/// qualification. Not deserializable and not execution authority. The reference
/// must qualify provisioning/no external writable aliases/namespace transitions;
/// these local checks alone cannot supply that external qualification.
#[derive(Clone)]
pub struct NamedReadonlyEnvironment(Arc<Environment>);
impl NamedReadonlyEnvironment {
    pub fn inspect(reference: ArtifactRef) -> Result<Self> {
        reference.id.validate()?;
        reference.version.validate()?;
        privileges()?;
        let mount = File::open("/proc/thread-self/ns/mnt")?;
        let user = File::open("/proc/thread-self/ns/user")?;
        let mount_identity = NamespaceIdentity::of(&mount)?;
        let user_identity = NamespaceIdentity::of(&user)?;
        Ok(Self(Arc::new(Environment {
            pid: std::process::id(),
            reference,
            mount,
            user,
            mount_identity,
            user_identity,
        })))
    }
    pub fn qualification(&self) -> &ArtifactRef {
        &self.0.reference
    }
    pub(super) fn verify_owner(&self) -> Result<()> {
        ensure!(
            std::process::id() == self.0.pid,
            "named environment belongs to original process"
        );
        ensure!(
            NamespaceIdentity::of(&self.0.mount)? == self.0.mount_identity
                && NamespaceIdentity::of(&self.0.user)? == self.0.user_identity,
            "retained named namespaces changed"
        );
        verify_namespace(&self.0.mount_identity, &self.0.user_identity)
    }
    pub(super) fn identities(&self) -> (NamespaceIdentity, NamespaceIdentity) {
        (self.0.mount_identity.clone(), self.0.user_identity.clone())
    }
    pub(super) fn verify_aliases(&self, device: u64, inode: u64) -> Result<()> {
        self.verify_owner()?;
        no_writable_mount_alias(device)?;
        no_writable_descriptor_alias(device, inode)
    }
}
fn no_writable_mount_alias(device: u64) -> Result<()> {
    let mut input = File::open("/proc/thread-self/mountinfo")?.take(MAX_MOUNTINFO + 1);
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= MAX_MOUNTINFO,
        "named mount topology bound"
    );
    let device_name = format!("{}:{}", libc::major(device), libc::minor(device));
    let mut count = 0;
    for row in text.lines() {
        count += 1;
        ensure!(count <= 16384, "named mount entry bound");
        let fields: Vec<_> = row.split_whitespace().collect();
        ensure!(fields.len() >= 10, "named mount topology malformed");
        if fields[2] == device_name {
            ensure!(
                fields[5].split(',').any(|p| p == "ro"),
                "named image device has a visible writable mount alias"
            );
        }
    }
    Ok(())
}
fn no_writable_descriptor_alias(device: u64, inode: u64) -> Result<()> {
    let mut count = 0;
    for entry in std::fs::read_dir("/proc/thread-self/fd")? {
        count += 1;
        ensure!(count <= MAX_FDS, "named descriptor scan bound");
        let entry = entry?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::EBADF),
                "named descriptor metadata unavailable"
            );
            continue;
        }
        let stat = unsafe { stat.assume_init() };
        if stat.st_dev == device && stat.st_ino == inode {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            ensure!(
                flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY,
                "named image has an inherited writable descriptor"
            );
        }
    }
    Ok(())
}
pub(super) fn verify_namespace(mount: &NamespaceIdentity, user: &NamespaceIdentity) -> Result<()> {
    privileges()?;
    ensure!(
        NamespaceIdentity::of(&File::open("/proc/thread-self/ns/mnt")?)? == *mount
            && NamespaceIdentity::of(&File::open("/proc/thread-self/ns/user")?)? == *user,
        "named readonly execution namespace changed"
    );
    Ok(())
}
fn privileges() -> Result<()> {
    let mut input = File::open("/proc/thread-self/status")?.take(65537);
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    ensure!(text.len() <= 65536, "named process status bound");
    for name in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        let value = text
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .context("named privilege state absent")?;
        ensure!(
            u64::from_str_radix(value.trim(), 16)? == 0,
            "named readonly image requires all capability sets empty"
        );
    }
    ensure!(
        text.lines()
            .find_map(|l| l.strip_prefix("NoNewPrivs:"))
            .is_some_and(|s| s.trim() == "1"),
        "named readonly image requires no_new_privs"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visible_writable_filesystem_refuses_image_device() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"fixture").unwrap();
        let m = std::fs::metadata(path).unwrap();
        assert!(no_writable_mount_alias(m.dev()).is_err());
    }
    #[test]
    fn inherited_writable_descriptor_to_exact_inode_refuses_even_without_path_lookup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        let writer = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let m = writer.metadata().unwrap();
        assert!(no_writable_descriptor_alias(m.dev(), m.ino()).is_err());
        // Existing O_RDONLY owners are permitted; unrelated writable files do
        // not substitute for the exact admitted inode.
        let reader = File::open(&path).unwrap();
        drop(writer);
        let _other = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(root.path().join("other"))
            .unwrap();
        no_writable_descriptor_alias(m.dev(), m.ino()).unwrap();
        drop(reader);
    }
    #[test]
    fn caller_thread_writer_is_not_hidden_by_leader_descriptor_table() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"fixture").unwrap();
        let reader = File::open(&path).unwrap();
        let metadata = reader.metadata().unwrap();
        std::thread::spawn(move || {
            assert_eq!(unsafe { libc::unshare(libc::CLONE_FILES) }, 0);
            let writer = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            let fd = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 256) };
            assert!(fd >= 256);
            let _writer = unsafe { File::from_raw_fd(fd) };
            drop(writer);
            assert!(
                std::fs::read_link(format!("/proc/self/fd/{fd}")).is_err(),
                "leader unexpectedly owns the private calling-thread descriptor"
            );
            assert!(std::fs::read_link(format!("/proc/thread-self/fd/{fd}")).is_ok());
            assert!(no_writable_descriptor_alias(metadata.dev(), metadata.ino()).is_err());
        })
        .join()
        .unwrap();
        drop(reader);
    }
}
