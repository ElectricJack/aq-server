//! Disposable, read-only tmux PTY attachment. Bytes are tmux's real redraw,
//! never a capture-text reconstruction, Snapshot or complete-history claim.
use super::*;
use std::os::{
    fd::{AsRawFd, FromRawFd},
    unix::process::CommandExt,
};
use terminal_output::ControlEvent;

const MAX_READ: usize = 16 * 1024;
// tmux sanitizes control characters in metadata formats without a terminal
// environment. These validated IDs and the fixed session name use a printable
// delimiter so the guardian's cleared environment preserves exact boundaries.
const PANE_METADATA: &str =
    "#{session_id}|#{session_name}|#{pane_id}|#{pane_pid}|#{pane_width}|#{pane_height}";
const CLIENT_METADATA: &str = "#{client_pid}|#{session_id}";

/// Actual running terminal owner is required; no serialized constructor or
/// input writer is exposed. Dropping the handle detaches only this viewer.
pub struct TerminalViewer {
    terminal: terminal_io::TerminalHandle,
    lease: reader_lease::Client,
    output: File,
    failed: bool,
}
impl TerminalViewer {
    pub fn open(terminal: &terminal_io::TerminalHandle) -> Result<Self> {
        let claim = terminal.viewer_claim()?;
        let (lease, output, _) = reader_lease::Client::open_viewer(terminal, claim)?;
        Ok(Self {
            terminal: terminal.clone(),
            lease,
            output,
            failed: false,
        })
    }
    pub fn poll(&mut self) -> Result<Vec<ControlEvent>> {
        ensure!(!self.failed, "PTY viewer disconnected");
        let result = self.read_once();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn read_once(&mut self) -> Result<Vec<ControlEvent>> {
        self.terminal.viewer_claim()?;
        self.lease.check()?;
        let mut bytes = [0u8; MAX_READ];
        match self.output.read(&mut bytes) {
            Ok(0) => bail!("PTY viewer detached"),
            Ok(n) => Ok(vec![ControlEvent::Bytes(bytes[..n].to_vec())]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                self.lease.drained()?;
                Ok(vec![])
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(vec![]),
            Err(_) => bail!("PTY viewer read unavailable"),
        }
    }
}

pub(crate) struct ViewerProcess {
    master: File,
    session: String,
    size: (u16, u16),
    checked: Instant,
}
impl ViewerProcess {
    pub(crate) fn tick(
        &mut self,
        dir: &Path,
        config: &SealedLaunch,
        host: &HostObservation,
        deadline: u64,
        pid: u32,
    ) -> Result<()> {
        if self.checked.elapsed() < Duration::from_millis(500) {
            return Ok(());
        }
        let (session, columns, rows) = pane(dir, config, host, deadline)?;
        ensure!(session == self.session, "PTY viewer session changed");
        verify_client(dir, config, host, deadline, pid, &session)?;
        // Follow an already accepted pane Resize; this client has ignore-size
        // and cannot independently resize the harness or acquire control.
        if self.size != (columns, rows) {
            set_size(&self.master, columns, rows)?;
            let evidence = ownership::process_evidence(pid)?;
            let fd = ownership::verified_pidfd(&evidence)?;
            ownership::signal(&fd, libc::SIGWINCH)?;
            self.size = (columns, rows);
        }
        self.checked = Instant::now();
        Ok(())
    }
}
fn pane(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    deadline: u64,
) -> Result<(String, u16, u16)> {
    let p = host.pane.as_ref().context("PTY viewer pane absent")?;
    let value = terminal_io::guarded_tmux(
        dir,
        config,
        host,
        deadline,
        host.permit_deadline_boot_ms,
        &["display-message", "-p", "-t", &p.pane_id, PANE_METADATA],
    )?;
    let text = std::str::from_utf8(&value).context("PTY viewer identity encoding")?;
    let fields: Vec<_> = text.trim_end_matches('\n').split('|').collect();
    ensure!(
        fields.len() == 6
            && numeric_session(fields[0])
            && fields[1] == p.session_name
            && fields[2] == p.pane_id
            && fields[3].parse::<u32>().ok() == Some(p.pane.pid),
        "PTY viewer exact pane/session mismatch"
    );
    let columns = fields[4].parse::<u16>()?;
    let rows = fields[5].parse::<u16>()?;
    ensure!(
        // Match ResizeCommand's accepted geometry (session protocol v1).
        // The smaller observed-startup bounds constrain initial layout only;
        // TerminalPolicy has no separate post-start dimension ceiling.
        columns > 0 && rows > 0 && columns <= 1000 && rows <= 1000,
        "PTY viewer dimensions unavailable"
    );
    Ok((fields[0].to_owned(), columns, rows))
}
fn numeric_session(s: &str) -> bool {
    s.strip_prefix('$')
        .is_some_and(|v| !v.is_empty() && v.len() <= 10 && v.bytes().all(|b| b.is_ascii_digit()))
}
fn verify_client(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    deadline: u64,
    pid: u32,
    session: &str,
) -> Result<()> {
    let policy = terminal_io::guarded_tmux(
        dir,
        config,
        host,
        deadline,
        host.permit_deadline_boot_ms,
        &["show-options", "-Av", "-t", session, "detach-on-destroy"],
    )?;
    ensure!(
        policy == b"on\n",
        "PTY viewer requires detach-on-destroy on"
    );
    let clients = terminal_io::guarded_tmux(
        dir,
        config,
        host,
        deadline,
        host.permit_deadline_boot_ms,
        &["list-clients", "-F", CLIENT_METADATA],
    )?;
    let expected = format!("{pid}|{session}");
    ensure!(
        std::str::from_utf8(&clients)?
            .lines()
            .any(|v| v == expected),
        "PTY viewer client target changed"
    );
    Ok(())
}
pub(crate) fn spawn(
    dir: &Path,
    config: &SealedLaunch,
    host: &HostObservation,
    deadline: u64,
) -> Result<(Child, ViewerProcess, File)> {
    let (session, columns, rows) = pane(dir, config, host, deadline)?;
    let (mut child, viewer, output) = spawn_attach(
        &config.tmux,
        &dir.join("tmux.sock"),
        &session,
        columns,
        rows,
    )?;
    let ready: Result<()> = (|| {
        let end = Instant::now() + Duration::from_millis(250);
        loop {
            if verify_client(dir, config, host, deadline, child.id(), &session).is_ok() {
                // Revalidate the actual pane after the attach became visible.
                ensure!(
                    pane(dir, config, host, deadline)? == (session.clone(), columns, rows),
                    "PTY viewer pane changed during attach"
                );
                return Ok(());
            }
            ensure!(
                child.try_wait()?.is_none() && Instant::now() < end,
                "PTY viewer attachment unavailable"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if let Err(error) = ready {
        let _ = child.kill();
        return Err(error);
    }
    Ok((child, viewer, output))
}
fn set_size(file: &File, columns: u16, rows: u16) -> Result<()> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    ensure!(
        unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &size) } == 0,
        "PTY viewer sizing unavailable"
    );
    Ok(())
}
fn spawn_attach(
    tmux: &Path,
    socket: &Path,
    session: &str,
    columns: u16,
    rows: u16,
) -> Result<(Child, ViewerProcess, File)> {
    ensure!(
        numeric_session(session),
        "PTY viewer numeric session required"
    );
    let mut master = -1;
    let mut slave = -1;
    ensure!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        } == 0,
        "PTY viewer allocation unavailable"
    );
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    // openpty does not set CLOEXEC. Neither end may leak into later launches.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "PTY viewer descriptor protection unavailable"
        );
    }
    set_size(&slave, columns, rows)?;
    let mut command = Command::new(tmux);
    command
        .args(["-N", "-u", "-S"])
        .arg(socket)
        .args([
            "-T",
            "RGB",
            "attach-session",
            "-E",
            "-f",
            "read-only,ignore-size",
            "-t",
            session,
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = master.try_clone()?;
    let child = command.spawn()?;
    Ok((
        child,
        ViewerProcess {
            master,
            session: session.to_owned(),
            size: (columns, rows),
            checked: Instant::now(),
        },
        output,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real tmux/PTY regression for the V1 port. It exercises the read-only
    // attach primitive, not a grant, baseline, provider or full-history claim.
    #[test]
    fn actual_readonly_pty_attach_redraws_idle_pane_and_detach_preserves_process() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("tmux.sock");
        let tmux = Path::new("/usr/bin/tmux");
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("/usr/bin/tmux")
                    .args(["-N", "-S"])
                    .arg(&self.0)
                    .arg("kill-server")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        let _cleanup = Cleanup(socket.clone());
        let start = Command::new(tmux)
            .args(["-S"])
            .arg(&socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "24",
                "-s",
                "viewer-proof",
                r"printf '\033[2J\033[Hactual-pty-first-draw'; exec /bin/sleep 30",
            ])
            .output()
            .unwrap();
        assert!(start.status.success());
        let query = |args: &[&str]| {
            let value = Command::new(tmux)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .args(["-N", "-S"])
                .arg(&socket)
                .args(args)
                .output()
                .unwrap();
            assert!(value.status.success());
            String::from_utf8(value.stdout).unwrap()
        };
        let session = query(&[
            "display-message",
            "-p",
            "-t",
            "viewer-proof:",
            "#{session_id}",
        ])
        .trim()
        .to_owned();
        let pid: u32 = query(&["display-message", "-p", "-t", &session, "#{pane_pid}"])
            .trim()
            .parse()
            .unwrap();
        let pane = ownership::process_evidence(pid).unwrap();
        let metadata = query(&["display-message", "-p", "-t", &session, PANE_METADATA]);
        let fields: Vec<_> = metadata.trim_end_matches('\n').split('|').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], session);
        assert_eq!(fields[1], "viewer-proof");
        assert_eq!(fields[3].parse::<u32>().unwrap(), pid);
        assert_eq!(&fields[4..], ["80", "24"]);
        let ready = Instant::now() + Duration::from_secs(2);
        while !query(&["capture-pane", "-p", "-t", &session]).contains("actual-pty-first-draw") {
            assert!(Instant::now() < ready);
            std::thread::sleep(Duration::from_millis(10));
        }
        // The pane is now idle: each attach must paint already existing text,
        // rather than relying on a new future %output control event.
        for _ in 0..2 {
            let (mut child, viewer, mut output) =
                spawn_attach(tmux, &socket, &session, 80, 24).unwrap();
            let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
            assert_eq!(
                unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            let end = Instant::now() + Duration::from_secs(2);
            let mut received = Vec::new();
            loop {
                let mut bytes = [0u8; MAX_READ];
                match output.read(&mut bytes) {
                    Ok(n) if n > 0 => received.extend_from_slice(&bytes[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    _ => panic!("PTY viewer ended before redraw"),
                }
                if received
                    .windows(b"actual-pty-first-draw".len())
                    .any(|v| v == b"actual-pty-first-draw")
                {
                    break;
                }
                assert!(received.len() <= 65_536 && Instant::now() < end);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(
                query(&["list-clients", "-F", CLIENT_METADATA])
                    .lines()
                    .any(|line| line == format!("{}|{}", child.id(), session))
            );
            assert!(
                query(&[
                    "list-clients",
                    "-F",
                    "#{client_pid}|#{session_id}|#{client_flags}"
                ])
                .lines()
                .any(|line| line.starts_with(&format!(
                    "{}|{}|",
                    child.id(),
                    session
                )) && line.contains("read-only")
                    && line.contains("ignore-size"))
            );
            drop(output);
            drop(viewer);
            let end = Instant::now() + Duration::from_secs(1);
            while child.try_wait().unwrap().is_none() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            if child.try_wait().unwrap().is_none() {
                child.kill().unwrap();
                child.wait().unwrap();
            }
            assert_eq!(ownership::process_evidence(pid).unwrap(), pane);
            assert_eq!(
                query(&["display-message", "-p", "-t", &session, "#{session_id}"]).trim(),
                session
            );
        }
    }
}
