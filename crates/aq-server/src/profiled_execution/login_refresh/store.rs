//! Owner-only material store. One file per lineage revision, named by its
//! revision id, created exclusively with mode 0600 inside a 0700 directory
//! owned by the server's effective user. No path, byte or digest from here is
//! logged; callers record digests in SQL only after the file is durable.
use crate::profiled_execution::material::{MaterialError, ProtectedMaterial};
use std::{
    fs::{DirBuilder, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const MAX_BYTES: usize = 65_536;

fn private(meta: &std::fs::Metadata) -> bool {
    meta.mode() & 0o077 == 0 && meta.uid() == nix::unistd::geteuid().as_raw()
}
fn file(dir: &Path, revision_id: Uuid) -> PathBuf {
    dir.join(format!("{revision_id}.material"))
}
fn checked_dir(dir: &Path) -> io::Result<()> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || !private(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "material store is not an owner-only directory",
        ));
    }
    Ok(())
}

/// Exclusive create; never overwrites. A crash leaves at most an orphan file.
pub fn write(dir: &Path, revision_id: Uuid, bytes: &[u8]) -> io::Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "material size"));
    }
    checked_dir(dir)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(file(dir, revision_id))?;
    out.write_all(bytes)?;
    out.sync_all()?;
    std::fs::File::open(dir)?.sync_all()
}

pub fn read(
    dir: &Path,
    revision_id: Uuid,
    digest: &str,
    len: u64,
) -> Result<ProtectedMaterial, MaterialError> {
    let input = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(file(dir, revision_id))
        .map_err(|_| MaterialError::Unavailable)?;
    let meta = input.metadata().map_err(|_| MaterialError::Unavailable)?;
    if !meta.is_file() || !private(&meta) {
        return Err(MaterialError::Unavailable);
    }
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| MaterialError::Unavailable)?;
    if bytes.len() as u64 != len || aq_admission::Digest::of(&bytes).as_str() != digest {
        return Err(MaterialError::Changed);
    }
    Ok(ProtectedMaterial::owned(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn store_is_exclusive_owner_only_and_digest_checked() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("store");
        let id = Uuid::new_v4();
        write(&dir, id, b"login").unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(file(&dir, id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(write(&dir, id, b"other").is_err(), "never overwrites");
        let digest = aq_admission::Digest::of(b"login");
        assert_eq!(
            read(&dir, id, digest.as_str(), 5).unwrap().bytes(),
            b"login"
        );
        assert!(read(&dir, id, digest.as_str(), 4).is_err());
        assert!(write(&dir, Uuid::new_v4(), &vec![0; MAX_BYTES + 1]).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write(&dir, Uuid::new_v4(), b"x").is_err(), "open directory");
    }
}
