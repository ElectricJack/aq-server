//! Bounded retained credential placement. Errors and Debug never expose bytes.
use anyhow::{Result, ensure};
use aq_responses_transport::RetainedUpstreamCredential;
use aq_session_protocol::Digest;
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
use zeroize::Zeroizing;

/// Wait for the controller's post-Running installation, then stage a private
/// copy for the supervisor native UID. This directory is never a worker mount.
pub async fn stage_supervisor_credential(
    source: &std::path::Path,
    home: &std::path::Path,
    deadline_boot_ms: u64,
) -> Result<PathBuf> {
    loop {
        ensure!(
            crate::clock()?.boot_ms < deadline_boot_ms,
            "supervisor credential deadline"
        );
        match std::fs::symlink_metadata(source) {
            // Installation uses create-new followed by write; do not consume the
            // brief empty-file interval between those two operations.
            Ok(m) if m.file_type().is_file() && m.len() == 0 => {}
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let secret = SecretFile::open(source.into())?.read_current()?;
    ensure!(
        secret.len() == 70
            && secret.starts_with(b"aqsv1_")
            && secret[6..].iter().all(u8::is_ascii_hexdigit),
        "invalid supervisor credential"
    );
    let path = home.join("supervisor.credential");
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    file.write_all(&secret)?;
    file.sync_all()?;
    Ok(path)
}

pub struct SecretFile {
    path: PathBuf,
    digest: Digest,
    device: u64,
    inode: u64,
}
impl std::fmt::Debug for SecretFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretFile([retained])")
    }
}
fn read(path: &std::path::Path) -> Result<(Zeroizing<Vec<u8>>, u64, u64)> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path,
        "credential placement rejected"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o777 == 0o600
            && metadata.nlink() == 1
            && metadata.len() <= 4096,
        "credential file rejected"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(4097).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 4096,
        "credential size rejected"
    );
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    ensure!(
        !bytes.is_empty() && bytes.iter().all(u8::is_ascii_graphic),
        "credential encoding rejected"
    );
    Ok((bytes, metadata.dev(), metadata.ino()))
}
impl SecretFile {
    pub fn open(path: PathBuf) -> Result<Self> {
        let (bytes, device, inode) =
            read(&path).map_err(|_| anyhow::anyhow!("credential placement unavailable"))?;
        Ok(Self {
            path,
            digest: Digest::of(&bytes),
            device,
            inode,
        })
    }
}
impl RetainedUpstreamCredential for SecretFile {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        let (bytes, device, inode) =
            read(&self.path).map_err(|_| anyhow::anyhow!("credential placement unavailable"))?;
        ensure!(
            self.device == device && self.inode == inode && self.digest == Digest::of(&bytes),
            "credential placement changed"
        );
        Ok(bytes)
    }
}
