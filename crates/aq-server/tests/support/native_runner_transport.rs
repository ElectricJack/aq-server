//! Fixture-only private-net listener/state handoff. No AQ execution authority.
#![allow(dead_code)] // Shared fixture API has separate hostless and composed callers.
use anyhow::{Context, Result, ensure};
use nix::libc;
use serde::Deserialize;
use std::{
    ffi::CString,
    fs::{self, File},
    io::Read,
    net::{Ipv4Addr, TcpListener},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};
const MAX_PACKET: usize = 131072;
const PRIVATE_STATE: &str = "/var/lib/aq-qualification/state";
const MINIMUM_FREE: u64 = 11005853696;
#[path = "native_runner_transport/git.rs"]
mod git;
fn timeout(fd: i32) -> Result<()> {
    let value = libc::timeval {
        tv_sec: 10,
        tv_usec: 0,
    };
    ensure!(
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&value as *const libc::timeval).cast(),
                std::mem::size_of_val(&value) as _,
            )
        } == 0,
        "fixture channel timeout"
    );
    Ok(())
}
fn ancestor(mut pid: u32, expected: u32) -> Result<bool> {
    for _ in 0..16 {
        if pid == expected {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        let mut body = String::new();
        File::open(format!("/proc/{pid}/stat"))?
            .take(4097)
            .read_to_string(&mut body)?;
        ensure!(body.len() <= 4096, "bounded fixture ancestry");
        let rest = body.rsplit_once(')').context("fixture process stat")?.1;
        pid = rest
            .split_whitespace()
            .nth(1)
            .context("fixture parent absent")?
            .parse()?;
    }
    Ok(false)
}
fn integer(fd: i32, key: i32) -> Result<i32> {
    let mut result = 0i32;
    let mut bytes = std::mem::size_of_val(&result) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                key,
                (&mut result as *mut i32).cast(),
                &mut bytes,
            )
        } == 0
            && bytes as usize == std::mem::size_of_val(&result),
        "fixture socket observation"
    );
    Ok(result)
}
fn listener(fd: OwnedFd, expected: &str) -> Result<TcpListener> {
    ensure!(
        integer(fd.as_raw_fd(), libc::SO_TYPE)? == libc::SOCK_STREAM
            && integer(fd.as_raw_fd(), libc::SO_DOMAIN)? == libc::AF_INET
            && integer(fd.as_raw_fd(), libc::SO_ACCEPTCONN)? == 1,
        "actual fixture TCP listener required"
    );
    let listener = TcpListener::from(fd);
    let address = listener.local_addr()?;
    ensure!(
        address.ip() == Ipv4Addr::LOCALHOST
            && address.port() != 0
            && expected == format!("http://{address}/"),
        "exact private listener origin"
    );
    listener.set_nonblocking(true)?;
    Ok(listener)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerInfo {
    schema_version: u32,
    phase: String,
    pub control_origin: String,
    pub provider_origin: String,
    pub state_dir: PathBuf,
    pub state_free_bytes: u64,
    private_pid_namespace: PathBuf,
}
/// Trusted outer fixture audit access, not a read-only directory capability.
/// O_RDONLY on a directory still permits openat writes; this API uses only
/// bounded reads and explicit trusted-operator Git provisioning; the harness
/// never inherits its descriptor. Neither operation creates AQ launch authority.
pub struct PrivateState {
    root: File,
}
impl PrivateState {
    pub fn free_bytes(&self) -> Result<u64> {
        let mut value: libc::statvfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::fstatvfs(self.root.as_raw_fd(), &mut value) } == 0,
            "actual private free space observation"
        );
        Ok(value.f_bavail.saturating_mul(value.f_frsize))
    }
    /// Observe ENOENT through the same held root. Other refusals do not count
    /// as cleanup, and this observation is never a process stopped proof.
    pub fn missing(&self, relative: &Path) -> Result<bool> {
        ensure!(
            !relative.is_absolute()
                && relative.as_os_str().len() <= 2048
                && relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "fixture relative cleanup member"
        );
        let name = CString::new(relative.as_os_str().as_encoded_bytes())?;
        #[repr(C)]
        struct How {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = How {
            flags: (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: 0,
            resolve: 0x01 | 0x04 | 0x08,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of_val(&how),
            )
        };
        if fd < 0 {
            ensure!(
                std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT),
                "cleanup observation refused without ENOENT"
            );
            return Ok(true);
        }
        drop(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        Ok(false)
    }
    /// Read-side assertion only. Contents cannot reconstruct a live input/host.
    pub fn read(&self, relative: &Path, maximum: usize) -> Result<Vec<u8>> {
        ensure!(
            maximum > 0 && maximum <= 4 * 1024 * 1024,
            "bounded fixture state read"
        );
        ensure!(
            !relative.is_absolute()
                && relative
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "fixture state-relative path required"
        );
        let name = CString::new(relative.as_os_str().as_encoded_bytes())?;
        #[repr(C)]
        struct How {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = How {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK) as u64,
            mode: 0,
            resolve: 0x01 | 0x04 | 0x08,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of_val(&how),
            )
        };
        ensure!(fd >= 0, "fixture state member unavailable");
        let file = unsafe { File::from_raw_fd(fd as i32) };
        let before = file.metadata()?;
        ensure!(
            before.is_file()
                && before.uid() == unsafe { libc::geteuid() }
                && before.len() <= maximum as u64,
            "bounded owned fixture member"
        );
        let mut bytes = Vec::new();
        (&file).take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(
            bytes.len() <= maximum
                && before.len() == bytes.len() as u64
                && before.len() == after.len()
                && before.mtime() == after.mtime()
                && before.mtime_nsec() == after.mtime_nsec()
                && before.ctime() == after.ctime()
                && before.ctime_nsec() == after.ctime_nsec(),
            "fixture state member changed"
        );
        Ok(bytes)
    }
}
pub struct Transferred {
    pub info: ListenerInfo,
    pub control: TcpListener,
    pub provider: TcpListener,
    pub state: PrivateState,
    channel: Option<OwnedFd>,
    sender: OwnedFd,
}
impl Transferred {
    /// Exit observation of the exact provisioning process; this is not an AQ
    /// Stopped proof or authority to release a workspace/result resource.
    pub fn sender_exited(&self) -> Result<bool> {
        let mut event = libc::pollfd {
            fd: self.sender.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut event, 1, 0) };
        ensure!(ready >= 0, "fixture sender exit observation");
        Ok(ready == 1 && event.revents & libc::POLLIN != 0)
    }

    pub fn configure(mut self, packet: &serde_json::Value) -> Result<Self> {
        let bytes = serde_json::to_vec(packet)?;
        ensure!(bytes.len() <= MAX_PACKET, "bounded fixture configure");
        let channel = self.channel.take().context("fixture already configured")?;
        let sent = unsafe {
            libc::send(
                channel.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        ensure!(
            sent >= 0 && sent as usize == bytes.len(),
            "exact fixture configure send"
        );
        drop(channel);
        Ok(self)
    }
}
/// Authenticate the sender before receiving any transferred descriptors.
fn sender_witness(channel: &OwnedFd, wrapper_pid: u32) -> Result<(u32, OwnedFd)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                channel.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut size,
            )
        } == 0
            && size as usize == std::mem::size_of_val(&credentials)
            && credentials.uid == unsafe { libc::geteuid() }
            && credentials.pid > 0
            && ancestor(credentials.pid as u32, wrapper_pid)?,
        "actual owned fixture sender required"
    );
    let pid = credentials.pid as u32;
    let raw_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    ensure!(raw_pidfd >= 0, "live fixture sender witness");
    let witness = unsafe { OwnedFd::from_raw_fd(raw_pidfd as i32) };
    Ok((pid, witness))
}

/// Retain received rights through packet validation and metadata decoding.
fn receive_packet(channel: &OwnedFd) -> Result<(ListenerInfo, Vec<OwnedFd>)> {
    let mut data = vec![0u8; MAX_PACKET + 1];
    let mut ancillary = [0usize; 64];
    let mut vector = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: data.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&ancillary);
    let got = unsafe { libc::recvmsg(channel.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    let mut descriptors = Vec::new();
    let mut valid_ancillary = true;
    if got >= 0 {
        unsafe {
            let mut item = libc::CMSG_FIRSTHDR(&message);
            while !item.is_null() {
                if (*item).cmsg_level == libc::SOL_SOCKET && (*item).cmsg_type == libc::SCM_RIGHTS {
                    let base = libc::CMSG_LEN(0) as usize;
                    let length = (*item).cmsg_len as usize;
                    ensure!(
                        length >= base
                            && (length - base).is_multiple_of(std::mem::size_of::<i32>()),
                        "fixture rights framing"
                    );
                    for index in 0..(length - base) / std::mem::size_of::<i32>() {
                        descriptors.push(OwnedFd::from_raw_fd(
                            *libc::CMSG_DATA(item).cast::<i32>().add(index),
                        ));
                    }
                } else {
                    valid_ancillary = false;
                }
                item = libc::CMSG_NXTHDR(&message, item);
            }
        }
    }
    ensure!(
        got > 0
            && got as usize <= MAX_PACKET
            && message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
            && valid_ancillary
            && descriptors.len() == 3,
        "bounded exact fixture handoff"
    );
    data.truncate(got as usize);
    let info: ListenerInfo = serde_json::from_slice(&data)?;
    ensure!(
        info.schema_version == 1
            && info.phase == "listeners"
            && info.state_dir == Path::new(PRIVATE_STATE)
            && info.state_free_bytes > MINIMUM_FREE,
        "exact fixture handoff metadata"
    );
    Ok((info, descriptors))
}

pub struct ControlListener {
    fd: OwnedFd,
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl ControlListener {
    pub fn bind(path: &Path) -> Result<Self> {
        let parent = path.parent().context("fixture socket parent absent")?;
        ensure!(
            parent.canonicalize()? == parent && !path.exists(),
            "fresh canonical fixture channel"
        );
        let m = fs::symlink_metadata(parent)?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
            "private fixture channel parent"
        );
        let bytes = path.as_os_str().as_encoded_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        ensure!(
            bytes.len() < address.sun_path.len(),
            "bounded fixture socket path"
        );
        address.sun_family = libc::AF_UNIX as _;
        for (target, source) in address.sun_path.iter_mut().zip(bytes) {
            *target = *source as _;
        }
        let raw =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        ensure!(raw >= 0, "fixture channel unavailable");
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        ensure!(
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            } == 0,
            "fixture channel bind"
        );
        ensure!(
            unsafe { libc::listen(fd.as_raw_fd(), 1) } == 0,
            "fixture channel listen"
        );
        timeout(fd.as_raw_fd())?;
        let m = fs::symlink_metadata(path)?;
        ensure!(m.file_type().is_socket(), "actual fixture socket");
        Ok(Self {
            fd,
            path: path.to_owned(),
            device: m.dev(),
            inode: m.ino(),
        })
    }
    /// Call in a blocking job. Kernel socket timeouts bound accept/receive;
    /// cancellation creates no worker/admission permission or side effect.
    pub fn receive(self, wrapper_pid: u32) -> Result<Transferred> {
        let raw = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            )
        };
        ensure!(raw >= 0, "bounded fixture accept");
        let channel = unsafe { OwnedFd::from_raw_fd(raw) };
        timeout(channel.as_raw_fd())?;
        let (pid, witness) = sender_witness(&channel, wrapper_pid)?;
        let (info, mut descriptors) = receive_packet(&channel)?;
        let pid_namespace = fs::read_link(format!("/proc/{pid}/ns/pid"))?;
        let net_namespace = fs::read_link(format!("/proc/{pid}/ns/net"))?;
        ensure!(
            pid_namespace == info.private_pid_namespace
                && pid_namespace != fs::read_link("/proc/thread-self/ns/pid")?
                && net_namespace != fs::read_link("/proc/thread-self/ns/net")?,
            "actual private fixture process/network view required"
        );
        let state = File::from(descriptors.pop().unwrap());
        let m = state.metadata()?;
        ensure!(
            m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o7777 == 0o700,
            "owned fixture state descriptor"
        );
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::fstatvfs(state.as_raw_fd(), &mut stat) } == 0
                && stat.f_bavail.saturating_mul(stat.f_frsize) > MINIMUM_FREE,
            "actual private state headroom"
        );
        let provider = listener(descriptors.pop().unwrap(), &info.provider_origin)?;
        let control = listener(descriptors.pop().unwrap(), &info.control_origin)?;
        let mut poll = libc::pollfd {
            fd: witness.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut poll, 1, 0) } == 0,
            "fixture sender exited during handoff"
        );
        Ok(Transferred {
            info,
            control,
            provider,
            state: PrivateState { root: state },
            channel: Some(channel),
            sender: witness,
        })
    }
}
impl Drop for ControlListener {
    fn drop(&mut self) {
        if let Ok(m) = fs::symlink_metadata(&self.path)
            && m.file_type().is_socket()
            && m.dev() == self.device
            && m.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet_channel(data: &[u8], descriptors: &[i32]) -> OwnedFd {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let sender = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let receiver = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        timeout(receiver.as_raw_fd()).unwrap();
        let mut vector = libc::iovec {
            iov_base: data.as_ptr().cast_mut().cast(),
            iov_len: data.len(),
        };
        let mut ancillary = [0usize; 64];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            let length = std::mem::size_of_val(descriptors) as u32;
            message.msg_control = ancillary.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE(length) } as usize;
            assert!(message.msg_controllen <= std::mem::size_of_val(&ancillary));
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(length) as usize;
                std::ptr::copy_nonoverlapping(
                    descriptors.as_ptr(),
                    libc::CMSG_DATA(header).cast::<i32>(),
                    descriptors.len(),
                );
            }
        }
        assert_eq!(
            unsafe { libc::sendmsg(sender.as_raw_fd(), &message, libc::MSG_NOSIGNAL) },
            data.len() as isize
        );
        receiver
    }

    #[test]
    fn packet_requires_exact_rights_and_bounded_closed_metadata() {
        let file = tempfile::tempfile().unwrap();
        let rights = [file.as_raw_fd(); 4];
        let info = serde_json::json!({
            "schema_version": 1, "phase": "listeners",
            "control_origin": "http://127.0.0.1:1/",
            "provider_origin": "http://127.0.0.1:2/",
            "state_dir": PRIVATE_STATE, "state_free_bytes": MINIMUM_FREE + 1,
            "private_pid_namespace": "/proc/fixture/ns/pid"
        });
        let bytes = serde_json::to_vec(&info).unwrap();
        for count in [0, 2, 3, 4] {
            let channel = packet_channel(&bytes, &rights[..count]);
            let result = receive_packet(&channel);
            if count == 3 {
                let (received, descriptors) = result.unwrap();
                assert_eq!(received.control_origin, info["control_origin"]);
                assert_eq!(descriptors.len(), 3);
                for descriptor in descriptors {
                    assert_eq!(
                        unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) }
                            & libc::FD_CLOEXEC,
                        libc::FD_CLOEXEC
                    );
                }
            } else {
                assert!(result.is_err());
            }
        }
        for (field, value) in [
            ("schema_version", serde_json::json!(2)),
            ("phase", serde_json::json!("other")),
            ("state_dir", serde_json::json!("/other")),
            ("state_free_bytes", serde_json::json!(MINIMUM_FREE)),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut changed = info.clone();
            changed[field] = value;
            let channel = packet_channel(&serde_json::to_vec(&changed).unwrap(), &rights[..3]);
            assert!(receive_packet(&channel).is_err(), "{field}");
        }
        for bytes in [b"invalid JSON".to_vec(), vec![b' '; MAX_PACKET + 1]] {
            let channel = packet_channel(&bytes, &rights[..3]);
            assert!(receive_packet(&channel).is_err());
        }
    }

    #[test]
    fn sender_witness_requires_owned_ancestry_and_retains_live_pidfd() {
        let channel = packet_channel(b"fixture", &[]);
        let pid = std::process::id();
        let (actual, witness) = sender_witness(&channel, pid).unwrap();
        assert_eq!(actual, pid);
        let mut event = libc::pollfd {
            fd: witness.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut event, 1, 0) }, 0);
        assert!(sender_witness(&channel, u32::MAX).is_err());
    }

    #[test]
    fn listener_requires_an_actual_bound_tcp_origin_and_refuses_wrong_kind_or_origin() {
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = socket.local_addr().unwrap();
        let transferred = listener(socket.into(), &format!("http://{address}/")).unwrap();
        assert_eq!(transferred.local_addr().unwrap(), address);
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(listener(socket.into(), "http://127.0.0.1:1/").is_err());
        let datagram = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert!(listener(datagram.into(), "http://127.0.0.1:1/").is_err());
    }
    #[test]
    fn state_audit_reads_beneath_held_directory_and_refuses_escape_symlink_and_growth_bound() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut file = File::create(dir.path().join("proof")).unwrap();
        file.write_all(b"gate04-proof").unwrap();
        drop(file);
        let state = PrivateState {
            root: File::open(dir.path()).unwrap(),
        };
        assert_eq!(state.read(Path::new("proof"), 64).unwrap(), b"gate04-proof");
        assert!(!state.missing(Path::new("proof")).unwrap());
        assert!(state.missing(Path::new("absent")).unwrap());
        assert!(state.missing(Path::new("../absent")).is_err());
        assert!(state.read(Path::new("../proof"), 64).is_err());
        assert!(state.read(Path::new("/etc/passwd"), 64).is_err());
        assert!(state.read(Path::new("proof"), 1).is_err());
        std::os::unix::fs::symlink("proof", dir.path().join("alias")).unwrap();
        assert!(state.read(Path::new("alias"), 64).is_err());
    }
}
