//! Linux ownership primitives shared with the existing worker guardian.
//! Only a dedicated single-threaded subreaper may reap all children or claim
//! ECHILD as stopped-writer proof. Procfs discovery alone is never such proof.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    time::Duration,
};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessEvidence {
    pub pid: u32,
    pub start_ticks: u64,
}
pub fn process_evidence(pid: u32) -> Result<ProcessEvidence> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .context("malformed process stat")?
        .1
        .split_whitespace()
        .collect();
    Ok(ProcessEvidence {
        pid,
        start_ticks: fields.get(19).context("missing starttime")?.parse()?,
    })
}
pub fn raw_boot_ms() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) },
        0,
        "CLOCK_BOOTTIME unavailable"
    );
    (ts.tv_sec as u64)
        .saturating_mul(1000)
        .saturating_add(ts.tv_nsec as u64 / 1_000_000)
}
pub fn pidfd(pid: u32) -> Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    ensure!(
        fd >= 0,
        "pidfd_open unavailable: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
pub fn signal(fd: &OwnedFd, sig: i32) -> Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
        bail!("pidfd signal: {}", std::io::Error::last_os_error())
    }
    Ok(())
}
pub fn verified_pidfd(evidence: &ProcessEvidence) -> Result<OwnedFd> {
    let fd = pidfd(evidence.pid)?;
    ensure!(
        process_evidence(evidence.pid)? == *evidence,
        "process incarnation changed"
    );
    Ok(fd)
}
pub fn direct_children() -> Result<Vec<u32>> {
    std::fs::read_to_string(format!("/proc/self/task/{}/children", std::process::id()))?
        .split_whitespace()
        .map(|p| Ok(p.parse()?))
        .collect()
}
pub fn signal_children(sig: i32) -> Result<()> {
    for child in direct_children()? {
        match pidfd(child) {
            Ok(fd) => signal(&fd, sig)?,
            Err(e) => {
                if process_evidence(child).is_ok() {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}
/// Called only by the helper executable before it creates any child processes.
pub fn become_subreaper() -> Result<()> {
    let fd = pidfd(std::process::id())?;
    signal(&fd, 0)?;
    ensure!(
        direct_children()?.is_empty(),
        "guardian must begin without existing children"
    );
    ensure!(
        std::process::id() != 1,
        "guardian cannot be PID namespace init"
    );
    ensure!(
        std::fs::read_dir("/proc/self/task")?.count() == 1,
        "guardian must be single threaded"
    );
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0,
        "subreaper unavailable"
    );
    let mut enabled = 0;
    ensure!(
        unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled, 0, 0, 0) } == 0
            && enabled == 1,
        "subreaper verification failed"
    );
    Ok(())
}
#[derive(Default)]
pub struct Reaped {
    pub processes: Vec<(u32, i32)>,
    pub kernel_echild: bool,
}
pub const MAX_REAPS_PER_TICK: usize = 256;
pub fn reap_children() -> Result<Reaped> {
    let mut result = Reaped {
        processes: Vec::with_capacity(MAX_REAPS_PER_TICK),
        kernel_echild: false,
    };
    for _ in 0..MAX_REAPS_PER_TICK {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
        if pid > 0 {
            result.processes.push((pid as u32, status));
            continue;
        }
        if pid == 0 {
            return Ok(result);
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ECHILD) => {
                result.kernel_echild = true;
                return Ok(result);
            }
            Some(libc::EINTR) => continue,
            _ => return Err(e.into()),
        }
    }
    Ok(result)
}
/// Best effort cleanup on ordinary helper error. It publishes no stopped proof.
pub struct EmergencyReaper;
impl Drop for EmergencyReaper {
    fn drop(&mut self) {
        let end = raw_boot_ms().saturating_add(5000);
        loop {
            if reap_children().is_ok_and(|r| r.kernel_echild) {
                break;
            }
            let _ = signal_children_bounded(libc::SIGKILL);
            if raw_boot_ms() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
/// pidfd readability reports exit even while the parent has not reaped a zombie.
pub fn pidfd_is_live(fd: &OwnedFd) -> Result<bool> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut poll, 1, 0) };
    ensure!(
        rc >= 0,
        "pidfd liveness poll: {}",
        std::io::Error::last_os_error()
    );
    Ok(rc == 0)
}

/// One bounded signaling batch. ECHILD is still established only by waitpid;
/// truncated procfs discovery never establishes absence or permits release.
pub fn signal_children_bounded(sig: i32) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/self/task/{}/children", std::process::id()))?
        .take(4096)
        .read_to_end(&mut bytes)?;
    if bytes.len() == 4096 {
        let end = bytes
            .iter()
            .rposition(u8::is_ascii_whitespace)
            .context("child PID token exceeds bound")?;
        bytes.truncate(end);
    }
    for pid in std::str::from_utf8(&bytes)?
        .split_whitespace()
        .take(MAX_REAPS_PER_TICK)
    {
        let child = pid.parse()?;
        match pidfd(child) {
            Ok(fd) => signal(&fd, sig)?,
            Err(e) => {
                if process_evidence(child).is_ok() {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}
