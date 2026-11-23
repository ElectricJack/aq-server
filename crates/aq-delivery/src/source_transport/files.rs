//! Bounded reads of an aq-git installation and its exported artifacts.
use super::{GitManifest, Uuid, Value};
use sha2::{Digest as _, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Debug, thiserror::Error)]
#[error("invalid or inconsistent aq-git artifact")]
pub struct InvalidArtifact;
type Result<T> = std::result::Result<T, InvalidArtifact>;

pub struct Installation(File);
pub struct ArtifactDirectory(File);
impl Installation {
    pub fn open(path: &Path) -> Result<Self> {
        open_root(path).map(Self)
    }
    pub fn id(&self) -> Result<Uuid> {
        let format = bounded_file(&self.0, "format.json", 65_536)?;
        let format: Value = serde_json::from_slice(&format).map_err(|_| InvalidArtifact)?;
        if format["format_version"] != 1 {
            return Err(InvalidArtifact);
        }
        format["installation_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .filter(|id| !id.is_nil())
            .ok_or(InvalidArtifact)
    }
    pub fn artifact(&self, operation: Uuid) -> Result<ArtifactDirectory> {
        let artifacts = open_member(&self.0, std::ffi::OsStr::new("artifacts"), true)?;
        private_metadata(&artifacts, true)?;
        let directory = open_member(
            &artifacts,
            std::ffi::OsStr::new(&operation.to_string()),
            true,
        )?;
        private_metadata(&directory, true)?;
        Ok(ArtifactDirectory(directory))
    }
}
impl ArtifactDirectory {
    pub fn manifest(&self, max: usize) -> Result<Vec<u8>> {
        bounded_file(&self.0, "manifest.json", max)
    }
    pub fn verify_bundle(&self, manifest: &GitManifest) -> Result<(u64, String)> {
        let mut bundle = open_member(&self.0, std::ffi::OsStr::new("objects.bundle"), false)?;
        let before = private_metadata(&bundle, false)?;
        if before.len() != manifest.bundle_size {
            return Err(InvalidArtifact);
        }
        let (total, hash) = hash_stream(&mut bundle, manifest.bundle_size)?;
        let contents_match =
            total == manifest.bundle_size && manifest.bundle_sha256.as_deref() == Some(&hash);
        if !contents_match || !unchanged(&before, &bundle.metadata().map_err(|_| InvalidArtifact)?)
        {
            return Err(InvalidArtifact);
        }
        Ok((total, hash))
    }
    pub fn read_bundle(&self, expected: u64, offset: u64, count: usize) -> Result<Vec<u8>> {
        if count > 1_048_576
            || offset
                .checked_add(count as u64)
                .is_none_or(|end| end > expected)
        {
            return Err(InvalidArtifact);
        }
        let mut file = open_member(&self.0, std::ffi::OsStr::new("objects.bundle"), false)?;
        let before = private_metadata(&file, false)?;
        if before.len() != expected {
            return Err(InvalidArtifact);
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| InvalidArtifact)?;
        let mut bytes = vec![0; count];
        file.read_exact(&mut bytes).map_err(|_| InvalidArtifact)?;
        if !unchanged(&before, &file.metadata().map_err(|_| InvalidArtifact)?) {
            return Err(InvalidArtifact);
        }
        Ok(bytes)
    }
}
fn hash_stream(bundle: &mut File, size: u64) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65_536];
    let mut total = 0u64;
    loop {
        let remaining = size
            .checked_add(1)
            .and_then(|n| n.checked_sub(total))
            .ok_or(InvalidArtifact)?;
        let cap = buffer
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if cap == 0 {
            return Err(InvalidArtifact);
        }
        let count = bundle
            .read(&mut buffer[..cap])
            .map_err(|_| InvalidArtifact)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > size {
            return Err(InvalidArtifact);
        }
        hasher.update(&buffer[..count]);
    }
    Ok((total, hex::encode(hasher.finalize())))
}

fn open_member(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> Result<std::fs::File> {
    use nix::{
        fcntl::{OFlag, openat},
        sys::stat::Mode,
    };
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    if directory {
        flags |= OFlag::O_DIRECTORY;
    }
    let fd = openat(Some(parent.as_raw_fd()), name, flags, Mode::empty())
        .map_err(|_| InvalidArtifact)?;
    // openat returned a newly owned descriptor. No other owner is constructed.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}
fn private_metadata(file: &std::fs::File, directory: bool) -> Result<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|_| InvalidArtifact)?;
    let right_kind = if directory {
        meta.is_dir()
    } else {
        meta.is_file()
    };
    let private = meta.uid() == nix::unistd::geteuid().as_raw() && meta.mode() & 0o022 == 0;
    if !private || !right_kind {
        return Err(InvalidArtifact);
    }
    Ok(meta)
}
fn open_root(path: &std::path::Path) -> Result<std::fs::File> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(InvalidArtifact);
    }
    let mut dir = std::fs::File::open("/").map_err(|_| InvalidArtifact)?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => dir = open_member(&dir, name, true)?,
            _ => return Err(InvalidArtifact),
        }
    }
    private_metadata(&dir, true)?;
    Ok(dir)
}
fn unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    (
        before.dev(),
        before.ino(),
        before.len(),
        before.mtime(),
        before.mtime_nsec(),
        before.ctime(),
        before.ctime_nsec(),
    ) == (
        after.dev(),
        after.ino(),
        after.len(),
        after.mtime(),
        after.mtime_nsec(),
        after.ctime(),
        after.ctime_nsec(),
    )
}
fn bounded_file(parent: &std::fs::File, name: &str, cap: usize) -> Result<Vec<u8>> {
    let mut file = open_member(parent, std::ffi::OsStr::new(name), false)?;
    let before = private_metadata(&file, false)?;
    if before.len() == 0 || before.len() > cap as u64 {
        return Err(InvalidArtifact);
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.by_ref()
        .take((cap + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| InvalidArtifact)?;
    let size_matches = bytes.len() <= cap && bytes.len() as u64 == before.len();
    if !size_matches || !unchanged(&before, &file.metadata().map_err(|_| InvalidArtifact)?) {
        return Err(InvalidArtifact);
    }
    Ok(bytes)
}
