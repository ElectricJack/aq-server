use super::*;
use serde::de::DeserializeOwned;
use std::mem::{size_of, size_of_val};

const MAX_PACKET: usize = 32768;
pub(super) struct Socket(pub OwnedFd);
pub(super) struct Packet<T> {
    pub value: T,
    pub fd: Option<OwnedFd>,
    pub credentials: libc::ucred,
}
impl Socket {
    pub fn pair() -> Result<(Self, Self)> {
        let mut fds = [-1; 2];
        ensure!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    fds.as_mut_ptr(),
                )
            } == 0,
            "capsule socketpair"
        );
        let a = Self(unsafe { OwnedFd::from_raw_fd(fds[0]) });
        let b = Self(unsafe { OwnedFd::from_raw_fd(fds[1]) });
        for socket in [&a, &b] {
            let on = 1i32;
            ensure!(
                unsafe {
                    libc::setsockopt(
                        socket.raw(),
                        libc::SOL_SOCKET,
                        libc::SO_PASSCRED,
                        (&on as *const i32).cast(),
                        size_of::<i32>() as _,
                    )
                } == 0,
                "capsule passcred"
            );
        }
        Ok((a, b))
    }
    pub fn raw(&self) -> RawFd {
        self.0.as_raw_fd()
    }
    pub fn poll(&self, events: i16, until: u64) -> Result<()> {
        let now = ownership::raw_boot_ms();
        ensure!(now < until, "capsule original deadline elapsed");
        let mut poll = libc::pollfd {
            fd: self.raw(),
            events,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, (until - now).min(10000) as i32) };
        ensure!(result > 0, "capsule packet timeout or poll error");
        Ok(())
    }
    pub fn send<T: Serialize>(&self, value: &T, fd: Option<RawFd>, until: u64) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_PACKET,
            "capsule packet size"
        );
        self.poll(libc::POLLOUT, until)?;
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 16];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        if let Some(fd) = fd {
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = unsafe { libc::CMSG_SPACE(size_of_val(&fd) as _) } as _;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&message);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(size_of_val(&fd) as _) as _;
                std::ptr::copy_nonoverlapping(
                    (&fd as *const RawFd).cast::<u8>(),
                    libc::CMSG_DATA(c),
                    size_of_val(&fd),
                );
            }
        }
        ensure!(
            unsafe {
                libc::sendmsg(
                    self.raw(),
                    &message,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            } == bytes.len() as isize,
            "capsule packet send"
        );
        Ok(())
    }
    pub fn recv<T: DeserializeOwned>(&self, until: u64) -> Result<Packet<T>> {
        self.poll(libc::POLLIN, until)?;
        let mut bytes = vec![0u8; MAX_PACKET];
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        let mut control = [0usize; 32];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = size_of_val(&control);
        let count = unsafe {
            libc::recvmsg(
                self.raw(),
                &mut message,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            )
        };
        // Own every received descriptor before rejecting malformed ancillary data.
        let mut fds = Vec::new();
        let mut credentials = None;
        let mut valid = true;
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&message);
            while !c.is_null() {
                let length = (*c).cmsg_len;
                if (*c).cmsg_level == libc::SOL_SOCKET
                    && (*c).cmsg_type == libc::SCM_RIGHTS
                    && length >= libc::CMSG_LEN(0) as usize
                {
                    let body = length - libc::CMSG_LEN(0) as usize;
                    valid &= body == size_of::<RawFd>();
                    for index in 0..body / size_of::<RawFd>() {
                        let fd = std::ptr::read_unaligned(
                            libc::CMSG_DATA(c)
                                .add(index * size_of::<RawFd>())
                                .cast::<RawFd>(),
                        );
                        if fd >= 0 {
                            fds.push(OwnedFd::from_raw_fd(fd));
                        } else {
                            valid = false;
                        }
                    }
                } else if (*c).cmsg_level == libc::SOL_SOCKET
                    && (*c).cmsg_type == libc::SCM_CREDENTIALS
                    && length == libc::CMSG_LEN(size_of::<libc::ucred>() as _) as usize
                    && credentials.is_none()
                {
                    credentials = Some(std::ptr::read_unaligned(
                        libc::CMSG_DATA(c).cast::<libc::ucred>(),
                    ));
                } else {
                    valid = false;
                }
                c = libc::CMSG_NXTHDR(&message, c);
            }
        }
        ensure!(
            count > 0
                && message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) == 0
                && valid
                && fds.len() <= 1,
            "capsule closed/malformed packet"
        );
        let credentials = credentials.context("capsule packet lacks post-send credentials")?;
        // An ancestor sender is deliberately invisible as PID0 inside a private
        // PID namespace. Only the trusted pre-exec Release stage permits that.
        ensure!(
            credentials.pid >= 0
                && credentials.uid == unsafe { libc::geteuid() }
                && credentials.gid == unsafe { libc::getegid() },
            "capsule sender credentials"
        );
        bytes.truncate(count as usize);
        Ok(Packet {
            value: serde_json::from_slice(&bytes)?,
            fd: fds.pop(),
            credentials,
        })
    }
}
