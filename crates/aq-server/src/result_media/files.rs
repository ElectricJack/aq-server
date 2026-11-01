//! Private generated files only; no external path/URL or result authority.
use super::{Error, Result};
use aq_result_media::Digest;
use nix::{
    errno::Errno,
    fcntl::{Flock, FlockArg, OFlag, RenameFlags, openat, renameat2},
    sys::stat::{Mode, mkdirat},
    unistd::{UnlinkatFlags, unlinkat},
};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};
use uuid::Uuid;

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|_| Error::Storage)
}
fn member(parent: &File, name: &std::ffi::OsStr, flags: OFlag) -> Result<File> {
    let fd = openat(
        Some(parent.as_raw_fd()),
        name,
        flags | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|e| {
        if e == Errno::ENOENT {
            Error::Missing
        } else {
            Error::Storage
        }
    })?;
    // openat returns a new owned descriptor, converted exactly once.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn directory(parent: &File, name: &std::ffi::OsStr) -> Result<File> {
    member(parent, name, OFlag::O_RDONLY | OFlag::O_DIRECTORY)
}
fn root_at(path: &Path) -> Result<File> {
    if !path.is_absolute() {
        return Err(Error::Storage);
    }
    let mut current = io(File::open("/"))?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => current = directory(&current, name)?,
            _ => return Err(Error::Storage),
        }
    }
    private(&current, true)?;
    Ok(current)
}
fn private(file: &File, dir: bool) -> Result<Metadata> {
    let m = io(file.metadata())?;
    if m.uid() != nix::unistd::geteuid().as_raw()
        || m.mode() & 0o777 != if dir { 0o700 } else { 0o600 }
        || if dir {
            !m.is_dir()
        } else {
            !m.is_file() || m.nlink() != 1
        }
    {
        return Err(Error::Storage);
    }
    Ok(m)
}
fn identity(a: &Metadata, b: &Metadata) -> bool {
    (a.dev(), a.ino(), a.uid()) == (b.dev(), b.ino(), b.uid())
}
fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    identity(a, b)
        && (
            a.len(),
            a.mtime(),
            a.mtime_nsec(),
            a.ctime(),
            a.ctime_nsec(),
            a.nlink(),
        ) == (
            b.len(),
            b.mtime(),
            b.mtime_nsec(),
            b.ctime(),
            b.ctime_nsec(),
            b.nlink(),
        )
}
fn child(parent: &File, name: &str) -> Result<File> {
    match mkdirat(
        Some(parent.as_raw_fd()),
        name,
        Mode::from_bits_truncate(0o700),
    ) {
        Ok(()) => io(parent.sync_all())?,
        Err(Errno::EEXIST) => {}
        Err(_) => return Err(Error::Storage),
    }
    let dir = directory(parent, name.as_ref())?;
    private(&dir, true)?;
    Ok(dir)
}
fn read(file: &File, expected: usize, digest: &Digest) -> Result<Vec<u8>> {
    let before = private(file, false)?;
    if before.len() != expected as u64 {
        return Err(Error::Integrity);
    }
    let mut bytes = Vec::with_capacity(expected);
    io(file.take(expected as u64 + 1).read_to_end(&mut bytes))?;
    if bytes.len() != expected
        || Digest::of(&bytes) != *digest
        || !unchanged(&before, &private(file, false)?)
    {
        return Err(Error::Integrity);
    }
    Ok(bytes)
}
fn named_read(parent: &File, name: &str, expected: usize, digest: &Digest) -> Result<Vec<u8>> {
    let file = member(parent, name.as_ref(), OFlag::O_RDONLY)?;
    let bytes = read(&file, expected, digest)?;
    let current = member(parent, name.as_ref(), OFlag::O_RDONLY)?;
    if !identity(&private(&file, false)?, &private(&current, false)?) {
        return Err(Error::Storage);
    }
    Ok(bytes)
}

pub(super) struct Root {
    path: PathBuf,
    dir: File,
    lock: Flock<File>,
    marker: Vec<u8>,
    staging: File,
    objects: File,
    pub id: Uuid,
    pub fingerprint: String,
    activity: Mutex<BTreeMap<Uuid, Activity>>,
    #[cfg(any(test, feature = "test-support"))]
    pub hook: Mutex<Option<super::TestHook>>,
}
#[derive(Default)]
struct Activity {
    readers: usize,
    writer: bool,
}
impl Root {
    pub fn open(path: &Path, id: Uuid) -> Result<Arc<Self>> {
        if id.is_nil() {
            return Err(Error::Invalid);
        }
        let dir = root_at(path)?;
        let lock = member(&dir, "owner.lock".as_ref(), OFlag::O_RDWR | OFlag::O_CREAT)?;
        private(&lock, false)?;
        let mut lock =
            Flock::lock(lock, FlockArg::LockExclusiveNonblock).map_err(|_| Error::Busy)?;
        let marker = format!("aq.result-media.private.v1\n{id}\n").into_bytes();
        let meta = private(&lock, false)?;
        if meta.len() == 0 {
            io(lock.write_all(&marker))?;
            io(lock.sync_all())?;
            io(dir.sync_all())?;
        } else {
            read(&lock, marker.len(), &Digest::of(&marker))?;
        }
        let m = private(&dir, true)?;
        let fingerprint = Digest::of(
            format!(
                "aq.result-media.root.v1\n{id}\n{}\n{}\n{}\n{}\n{}\n{}\n",
                m.dev(),
                m.ino(),
                m.uid(),
                meta.dev(),
                meta.ino(),
                meta.uid()
            )
            .as_bytes(),
        )
        .as_str()[7..]
            .to_owned();
        let staging = child(&dir, "staging")?;
        let objects = child(&dir, "objects")?;
        let root = Arc::new(Self {
            path: path.into(),
            dir,
            lock,
            marker,
            staging,
            objects,
            id,
            fingerprint,
            activity: Mutex::new(BTreeMap::new()),
            #[cfg(any(test, feature = "test-support"))]
            hook: Mutex::new(None),
        });
        root.check()?;
        Ok(root)
    }
    pub fn check(&self) -> Result<()> {
        let current = root_at(&self.path)?;
        if !identity(&private(&current, true)?, &private(&self.dir, true)?) {
            return Err(Error::Storage);
        }
        let marker = member(&current, "owner.lock".as_ref(), OFlag::O_RDONLY)?;
        if !identity(&private(&marker, false)?, &private(&self.lock, false)?) {
            return Err(Error::Storage);
        }
        named_read(
            &current,
            "owner.lock",
            self.marker.len(),
            &Digest::of(&self.marker),
        )?;
        for (name, dir) in [("staging", &self.staging), ("objects", &self.objects)] {
            if !identity(
                &private(&directory(&current, name.as_ref())?, true)?,
                &private(dir, true)?,
            ) {
                return Err(Error::Storage);
            }
        }
        Ok(())
    }
    pub fn active(self: &Arc<Self>, id: Uuid, writer: bool) -> Result<ActivityGuard> {
        self.check()?;
        let mut all = self.activity.lock().map_err(|_| Error::Storage)?;
        let (writers, readers) = all.values().fold((0, 0), |(w, r), a| {
            (w + usize::from(a.writer), r + a.readers)
        });
        let occupied = all.get(&id);
        if writer && (writers >= 2 || occupied.is_some_and(|a| a.writer || a.readers > 0))
            || !writer && (readers >= 4 || occupied.is_some_and(|a| a.writer))
        {
            return Err(Error::Busy);
        }
        let a = all.entry(id).or_default();
        if writer {
            a.writer = true;
        } else {
            a.readers += 1;
        }
        Ok(ActivityGuard {
            root: self.clone(),
            id,
            writer,
        })
    }
    /// Cleanup uses the same exclusive physical-writer fence. This proof is
    /// independent of a SQL lease and persists until every actual FD/job ends.
    pub fn generation(&self, id: Uuid, published: bool) -> Result<File> {
        self.check()?;
        let parent = if published {
            &self.objects
        } else {
            &self.staging
        };
        let dir = directory(parent, id.to_string().as_ref())?;
        private(&dir, true)?;
        Ok(dir)
    }
    pub fn receive(&self, id: Uuid, bytes: &[u8], digest: &Digest) -> Result<()> {
        self.check()?;
        if bytes.len() > aq_result_media::MAX_INPUT_BYTES || Digest::of(bytes) != *digest {
            return Err(Error::Integrity);
        }
        let dir = child(&self.staging, &id.to_string())?;
        self.write_once(&dir, "input.png", bytes)?;
        named_read(&dir, "input.png", bytes.len(), digest)?;
        self.check()
    }
    fn write_once(&self, dir: &File, name: &str, bytes: &[u8]) -> Result<()> {
        // Failed writes remain accounted. Explicit recovery removes only this
        // known uncommitted directory before retry; no in-place replacement.
        let mut file = member(
            dir,
            name.as_ref(),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
        )?;
        private(&file, false)?;
        io(file.write_all(bytes))?;
        io(file.sync_all())?;
        self.point(name)?;
        io(dir.sync_all())?;
        Ok(())
    }
    pub fn input(
        &self,
        id: Uuid,
        published: bool,
        size: usize,
        digest: &Digest,
    ) -> Result<Vec<u8>> {
        if size > aq_result_media::MAX_INPUT_BYTES {
            return Err(Error::Invalid);
        }
        let dir = self.generation(id, published)?;
        let bytes = named_read(&dir, "input.png", size, digest)?;
        self.check()?;
        Ok(bytes)
    }
    pub fn variants(&self, id: Uuid, image: &aq_result_media::PreparedImage) -> Result<()> {
        let dir = self.generation(id, false)?;
        self.write_once(&dir, "render.png", image.render().bytes())?;
        self.write_once(&dir, "preview.png", image.preview().bytes())?;
        self.check()
    }
    pub fn remove_variants(&self, id: Uuid) -> Result<()> {
        let dir = self.generation(id, false)?;
        for name in ["render.png", "preview.png"] {
            match unlinkat(Some(dir.as_raw_fd()), name, UnlinkatFlags::NoRemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(_) => return Err(Error::Storage),
            }
        }
        io(dir.sync_all())?;
        self.check()
    }
    /// Descriptor consistency only, no media body read or digest attestation.
    pub fn probe(&self, id: Uuid, variants: &aq_result_media::Variants) -> Result<()> {
        self.point("probe")?;
        let dir = self.generation(id, true)?;
        for (name, variant) in [
            ("render.png", &variants.render),
            ("preview.png", &variants.preview),
        ] {
            let file = member(&dir, name.as_ref(), OFlag::O_RDONLY)?;
            if private(&file, false)?.len() != variant.bytes {
                return Err(Error::Integrity);
            }
        }
        if !identity(
            &private(&dir, true)?,
            &private(&self.generation(id, true)?, true)?,
        ) {
            return Err(Error::Storage);
        }
        self.check()
    }
    pub fn publish(&self, id: Uuid) -> Result<()> {
        self.check()?;
        renameat2(
            Some(self.staging.as_raw_fd()),
            id.to_string().as_str(),
            Some(self.objects.as_raw_fd()),
            id.to_string().as_str(),
            RenameFlags::RENAME_NOREPLACE,
        )
        .map_err(|_| Error::Storage)?;
        self.point("renamed")?;
        io(self.objects.sync_all())?;
        io(self.staging.sync_all())?;
        self.point("published")?;
        self.check()
    }
    pub fn variant(
        &self,
        id: Uuid,
        name: &str,
        variant: &aq_result_media::Variant,
    ) -> Result<Vec<u8>> {
        if !matches!(name, "render.png" | "preview.png")
            || variant.bytes > aq_result_media::MAX_RENDER_BYTES as u64
        {
            return Err(Error::Invalid);
        }
        let dir = self.generation(id, true)?;
        let bytes = named_read(&dir, name, variant.bytes as usize, &variant.digest)?;
        if !identity(
            &private(&dir, true)?,
            &private(&self.generation(id, true)?, true)?,
        ) {
            return Err(Error::Storage);
        }
        self.check()?;
        Ok(bytes)
    }
    pub fn charge(&self, id: Uuid) -> Result<u64> {
        let dir = self.generation(id, true)?;
        let mut total = io(dir.metadata())?.blocks() * 512;
        for name in ["input.png", "render.png", "preview.png"] {
            let file = member(&dir, name.as_ref(), OFlag::O_RDONLY)?;
            let m = private(&file, false)?;
            total = total
                .checked_add(m.len().max(m.blocks() * 512))
                .ok_or(Error::Storage)?;
        }
        self.check()?;
        Ok(total)
    }
    pub fn remove(&self, id: Uuid, published: bool) -> Result<()> {
        self.check()?;
        let parent = if published {
            &self.objects
        } else {
            &self.staging
        };
        let name = id.to_string();
        let dir = match directory(parent, name.as_ref()) {
            Ok(dir) => dir,
            Err(_) => {
                // openat's error is deliberately opaque, but only ENOENT from
                // fstatat can confirm absence; symlinks/unreadable is failure.
                return match nix::sys::stat::fstatat(
                    Some(parent.as_raw_fd()),
                    name.as_str(),
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                ) {
                    Err(Errno::ENOENT) => Ok(()),
                    _ => Err(Error::Storage),
                };
            }
        };
        private(&dir, true)?;
        for file in ["input.png", "render.png", "preview.png"] {
            match unlinkat(Some(dir.as_raw_fd()), file, UnlinkatFlags::NoRemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => {}
                Err(_) => return Err(Error::Storage),
            }
        }
        io(dir.sync_all())?;
        if !identity(
            &private(&directory(parent, name.as_ref())?, true)?,
            &private(&dir, true)?,
        ) {
            return Err(Error::Storage);
        }
        unlinkat(
            Some(parent.as_raw_fd()),
            name.as_str(),
            UnlinkatFlags::RemoveDir,
        )
        .map_err(|_| Error::Storage)?;
        io(parent.sync_all())?;
        self.check()?;
        match nix::sys::stat::fstatat(
            Some(parent.as_raw_fd()),
            name.as_str(),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Err(Errno::ENOENT) => Ok(()),
            _ => Err(Error::Storage),
        }
    }
    pub fn point(&self, _name: &str) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        {
            let callback = self.hook.lock().map_err(|_| Error::Storage)?.clone();
            if let Some(callback) = callback {
                callback(_name)?;
            }
        }
        Ok(())
    }
}
pub(super) struct ActivityGuard {
    root: Arc<Root>,
    id: Uuid,
    writer: bool,
}
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if let Ok(mut all) = self.root.activity.lock()
            && let Some(a) = all.get_mut(&self.id)
        {
            if self.writer {
                a.writer = false;
            } else {
                a.readers -= 1;
            }
            if !a.writer && a.readers == 0 {
                all.remove(&self.id);
            }
        }
    }
}
