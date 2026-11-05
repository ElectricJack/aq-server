//! Bounded owner-only local packets and one descriptor. No network listener.
use super::*;
use std::mem::{size_of, size_of_val};
use std::os::{
    fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    unix::ffi::OsStrExt,
};
pub(crate) const MAX_PACKET: usize = 4096;
pub(crate) struct Packet {
    pub(crate) bytes: Vec<u8>,
    pub(crate) fd: Option<OwnedFd>,
}
pub(crate) struct Socket(pub(crate) OwnedFd);
impl Socket {
    pub(crate) fn new() -> Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        ensure!(fd >= 0, "reader local socket creation");
        let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        let capacity: libc::c_int = 8192;
        for option in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
            ensure!(
                unsafe {
                    libc::setsockopt(
                        socket.raw(),
                        libc::SOL_SOCKET,
                        option,
                        (&capacity as *const libc::c_int).cast(),
                        size_of::<libc::c_int>() as _,
                    )
                } == 0,
                "reader socket capacity"
            );
        }
        Ok(socket)
    }
    pub(crate) fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
    pub(crate) fn address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
        let bytes = path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        ensure!(
            !bytes.contains(&0) && bytes.len() < address.sun_path.len(),
            "reader socket path bound"
        );
        address.sun_family = libc::AF_UNIX as _;
        for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
            *out = *byte as _;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as _;
        Ok((address, length))
    }
    pub(crate) fn connect(path: &Path) -> Result<Self> {
        let socket = Self::new()?;
        let (address, length) = Self::address(path)?;
        ensure!(
            unsafe {
                libc::connect(
                    socket.raw(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            } == 0,
            "reader local connect refused"
        );
        Ok(socket)
    }
    pub(crate) fn credentials(&self) -> Result<libc::ucred> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = size_of::<libc::ucred>() as libc::socklen_t;
        ensure!(
            unsafe {
                libc::getsockopt(
                    self.raw(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut credentials as *mut libc::ucred).cast(),
                    &mut length,
                )
            } == 0
                && length as usize == size_of::<libc::ucred>()
                && credentials.uid == unsafe { libc::geteuid() },
            "reader local peer identity"
        );
        Ok(credentials)
    }
    pub(crate) fn accept(&self) -> Result<Option<Self>> {
        let fd = unsafe {
            libc::accept4(
                self.raw(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e.into());
        }
        let socket = Self(unsafe { OwnedFd::from_raw_fd(fd) });
        socket.credentials()?;
        Ok(Some(socket))
    }
    pub(crate) fn send<T: Serialize>(&self, value: &T, fd: Option<RawFd>) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(bytes.len() <= MAX_PACKET, "reader packet bound");
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(fd) = fd {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) } as _;
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&msg);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as _) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
            }
        }
        ensure!(
            unsafe { libc::sendmsg(self.raw(), &msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) }
                == bytes.len() as isize,
            "reader packet send refused"
        );
        Ok(())
    }
    pub(crate) fn hung_up(&self) -> Result<bool> {
        let mut poll = libc::pollfd {
            fd: self.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut poll, 1, 0) } >= 0,
            "local peer poll"
        );
        Ok(poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0)
    }
    pub(crate) fn receive(&self) -> Result<Option<Packet>> {
        let mut bytes = vec![0u8; MAX_PACKET];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control);
        let count = unsafe {
            libc::recvmsg(
                self.raw(),
                &mut msg,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        if count < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(e.into());
        }
        let mut fds = Vec::new();
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&msg);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let length = (*header)
                        .cmsg_len
                        .saturating_sub(libc::CMSG_LEN(0) as usize)
                        / size_of::<RawFd>();
                    for n in 0..length {
                        fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                            libc::CMSG_DATA(header).cast::<RawFd>().add(n),
                        )));
                    }
                }
                header = libc::CMSG_NXTHDR(&msg, header);
            }
        }
        ensure!(
            count > 0
                && msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
                && fds.len() <= 1,
            "reader closed or malformed packet"
        );
        bytes.truncate(count as usize);
        Ok(Some(Packet {
            bytes,
            fd: fds.pop(),
        }))
    }
}
