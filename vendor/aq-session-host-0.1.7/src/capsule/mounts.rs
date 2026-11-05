use super::*;
use std::{ffi::CString, io::Read};

const MAX_MOUNTINFO: usize = 1024 * 1024;
const MAX_MOUNTS: usize = 16384;
fn cpath(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_encoded_bytes())?)
}
fn mount(
    source: Option<&std::ffi::CStr>,
    target: &Path,
    kind: Option<&std::ffi::CStr>,
    flags: libc::c_ulong,
) -> Result<()> {
    let target = cpath(target)?;
    ensure!(
        unsafe {
            libc::mount(
                source.map_or(std::ptr::null(), |s| s.as_ptr()),
                target.as_ptr(),
                kind.map_or(std::ptr::null(), |s| s.as_ptr()),
                flags,
                std::ptr::null(),
            )
        } == 0,
        "capsule mount: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
fn private() -> Result<()> {
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE)
}
fn anchor(path: &Path, pin: &DirectoryPin, original: &File) -> Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: 2 | 4,
    };
    let route = cpath(path)?;
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            route.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    ensure!(
        fd >= 0,
        "capsule checked directory re-anchor: {}",
        std::io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_fd(fd as RawFd) };
    pin.verify(original)?;
    pin.verify(&file)?;
    Ok(file)
}
fn root_anchor(plan: &PlanRecord, original: &File) -> Result<File> {
    let file = anchor(&plan.root, &plan.root_pin, original)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
        "capsule root target ownership/mode"
    );
    ensure!(
        std::fs::read_dir(format!("/proc/thread-self/fd/{}", file.as_raw_fd()))?
            .next()
            .is_none(),
        "capsule root target not empty"
    );
    Ok(file)
}
fn fresh_root(target: &File) -> Result<File> {
    // Construct a detached tmpfs; attach to the checked directory object, never
    // a pathname which an outside filesystem writer can replace after checking.
    let context = unsafe { libc::syscall(libc::SYS_fsopen, c"tmpfs".as_ptr(), 1u32) };
    ensure!(
        context >= 0,
        "capsule fsopen: {}",
        std::io::Error::last_os_error()
    );
    let context = unsafe { OwnedFd::from_raw_fd(context as RawFd) };
    for (key, value) in [(c"size", c"16m"), (c"mode", c"0700")] {
        ensure!(
            unsafe {
                libc::syscall(
                    libc::SYS_fsconfig,
                    context.as_raw_fd(),
                    1u32,
                    key.as_ptr(),
                    value.as_ptr(),
                    0,
                )
            } == 0,
            "capsule tmpfs config"
        );
    }
    ensure!(
        unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                context.as_raw_fd(),
                6u32,
                std::ptr::null::<libc::c_char>(),
                std::ptr::null::<libc::c_char>(),
                0,
            )
        } == 0,
        "capsule tmpfs create"
    );
    let tree = unsafe { libc::syscall(libc::SYS_fsmount, context.as_raw_fd(), 1u32, 6u32) };
    ensure!(
        tree >= 0,
        "capsule fsmount: {}",
        std::io::Error::last_os_error()
    );
    let tree = unsafe { File::from_raw_fd(tree as RawFd) };
    ensure!(
        unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                tree.as_raw_fd(),
                c"".as_ptr(),
                target.as_raw_fd(),
                c"".as_ptr(),
                4u32 | 64u32,
            )
        } == 0,
        "capsule exact FD root attachment: {}",
        std::io::Error::last_os_error()
    );
    Ok(tree)
}
fn mount_id(file: &File) -> Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::uninit();
    ensure!(
        unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } == 0,
        "capsule source mount ID"
    );
    let stat = unsafe { stat.assume_init() };
    ensure!(
        stat.stx_mask & libc::STATX_MNT_ID != 0 && stat.stx_mnt_id > 0,
        "capsule source mount ID unsupported"
    );
    Ok(stat.stx_mnt_id)
}
fn decode_path(value: &str) -> Result<PathBuf> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escaped = bytes
                .get(index..index + 4)
                .context("capsule mountinfo escape")?;
            out.push(match escaped {
                b"\\040" => b' ',
                b"\\011" => b'\t',
                b"\\012" => b'\n',
                b"\\134" => b'\\',
                _ => bail!("capsule unsupported mountinfo escape"),
            });
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    Ok(PathBuf::from(String::from_utf8(out)?))
}
fn no_submounts(path: &Path, file: &File) -> Result<()> {
    let id = mount_id(file)?;
    let mut body = Vec::new();
    File::open("/proc/thread-self/mountinfo")?
        .take(MAX_MOUNTINFO as u64 + 1)
        .read_to_end(&mut body)?;
    ensure!(body.len() <= MAX_MOUNTINFO, "capsule mountinfo byte bound");
    let body = std::str::from_utf8(&body)?;
    let mut seen = false;
    let mut count = 0;
    for line in body.lines() {
        count += 1;
        ensure!(count <= MAX_MOUNTS, "capsule mountinfo row bound");
        let fields = line.split_whitespace().take(6).collect::<Vec<_>>();
        ensure!(fields.len() == 6, "capsule mountinfo shape");
        let row = fields[0].parse::<u64>()?;
        seen |= row == id;
        let mounted = decode_path(fields[4])?;
        ensure!(
            mounted == path || !mounted.starts_with(path),
            "capsule candidate refuses grant submounts"
        );
    }
    ensure!(seen, "capsule re-anchored source not in current mount view");
    Ok(())
}
fn pivot() -> Result<()> {
    ensure!(
        unsafe { libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".old-root".as_ptr()) } == 0,
        "capsule FD-root pivot: {}",
        std::io::Error::last_os_error()
    );
    std::env::set_current_dir("/")?;
    ensure!(
        unsafe { libc::umount2(c"/.old-root".as_ptr(), libc::MNT_DETACH) } == 0,
        "capsule detach old root"
    );
    std::fs::remove_dir("/.old-root")?;
    Ok(())
}
pub(super) fn enter(plan: &PlanRecord, files: &[File]) -> Result<()> {
    private()?;
    let target = root_anchor(plan, &files[2])?;
    let root = fresh_root(&target)?;
    ensure!(
        unsafe { libc::fchdir(root.as_raw_fd()) } == 0,
        "capsule retained tmpfs root cwd"
    );
    for name in ["proc", "dev", "tmp", "allowed", ".old-root"] {
        std::fs::create_dir(name)?;
    }
    for (record, source) in plan.grants.iter().zip(&files[3..]) {
        let anchor = anchor(&record.source, &record.pin, source)?;
        no_submounts(&record.source, &anchor)?;
        let target = PathBuf::from(record.target.relative()?);
        std::fs::create_dir_all(&target)?;
        let tree = unsafe {
            libc::syscall(
                libc::SYS_open_tree,
                anchor.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | 1 | libc::O_CLOEXEC,
            )
        };
        ensure!(
            tree >= 0,
            "capsule retained open_tree: {}",
            std::io::Error::last_os_error()
        );
        let tree = unsafe { OwnedFd::from_raw_fd(tree as RawFd) };
        let target_path = cpath(&target)?;
        ensure!(
            unsafe {
                libc::syscall(
                    libc::SYS_move_mount,
                    tree.as_raw_fd(),
                    c"".as_ptr(),
                    libc::AT_FDCWD,
                    target_path.as_ptr(),
                    4u32,
                )
            } == 0,
            "capsule exact grant attachment"
        );
        mount(
            None,
            &target,
            None,
            libc::MS_BIND
                | libc::MS_REMOUNT
                | libc::MS_NOSUID
                | libc::MS_NODEV
                | if record.writable { 0 } else { libc::MS_RDONLY },
        )?;
    }
    pivot()?;
    mount(
        Some(c"proc"),
        Path::new("/proc"),
        Some(c"proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
    )
}

/// Explicit scratch-only kernel probe using the actual root/submount functions.
pub(super) fn probe(base: &Path) -> Result<()> {
    ensure!(
        cfg!(target_arch = "x86_64"),
        "capsule candidate probe architecture"
    );
    ensure!(
        base.is_absolute()
            && std::fs::canonicalize(base)? == base
            && std::fs::read_dir(base)?.next().is_none(),
        "capsule probe private empty scratch"
    );
    let metadata = std::fs::metadata(base)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
        "capsule probe ownership"
    );
    unsafe {
        libc::alarm(5);
    }
    let root = base.join("root");
    let grant = base.join("grant");
    for path in [&root, &grant] {
        std::fs::create_dir(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let open = |path: &Path| -> Result<File> {
        Ok(OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?)
    };
    let original = open(&root)?;
    let pin = DirectoryPin::of(&original)?;
    let grant_original = open(&grant)?;
    let grant_pin = DirectoryPin::of(&grant_original)?;
    setup::user_namespace()?;
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWNS) } == 0,
        "capsule probe namespace"
    );
    private()?;
    let source = anchor(&grant, &grant_pin, &grant_original)?;
    let under = grant.join("under");
    std::fs::create_dir(&under)?;
    std::fs::write(under.join("hidden-underlay"), b"must not silently surface")?;
    mount(
        Some(c"tmpfs"),
        &under,
        Some(c"tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
    )?;
    ensure!(
        no_submounts(&grant, &source).is_err(),
        "capsule submount negative did not refuse"
    );
    let under_path = cpath(&under)?;
    ensure!(
        unsafe { libc::umount2(under_path.as_ptr(), 0) } == 0,
        "capsule probe child mount cleanup"
    );
    no_submounts(&grant, &source)?;
    let target = anchor(&root, &pin, &original)?;
    let moved = base.join("moved-root");
    std::fs::rename(&root, &moved)?;
    std::fs::create_dir(&root)?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    ensure!(
        anchor(&root, &pin, &original).is_err(),
        "capsule substituted root route accepted"
    );
    let tree = fresh_root(&target)?;
    ensure!(
        unsafe { libc::fchdir(tree.as_raw_fd()) } == 0,
        "capsule probe pinned root cwd"
    );
    std::fs::create_dir(".old-root")?;
    std::fs::write("root-fd-canary", b"private tmpfs only")?;
    ensure!(
        std::fs::read_dir(&root)?.next().is_none(),
        "capsule root replacement was modified"
    );
    ensure!(
        std::fs::metadata(&moved)?.dev() == tree.metadata()?.dev()
            && std::fs::metadata(&root)?.dev() != tree.metadata()?.dev(),
        "capsule root mount redirected"
    );
    pivot()?;
    ensure!(
        std::fs::read("/root-fd-canary")? == b"private tmpfs only",
        "capsule pivot lost FD root"
    );
    Ok(())
}
