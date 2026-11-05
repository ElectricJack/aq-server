//! Private local read-client ownership; not an external relay or launch authority.
use super::*;
#[cfg(test)]
use crate::local_ipc::MAX_PACKET;
use crate::local_ipc::Socket;
#[cfg(test)]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd};
const SOCKET: &str = "terminal-reader.sock";
const VIEWER_SOCKET: &str = "terminal-viewer.sock";
pub(crate) const LEASE_MS: u64 = 1000;
const DRAIN_CADENCE_MS: u64 = 250;
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Request {
    Register {
        binding: Box<terminal_io::TerminalBinding>,
        claim: Uuid,
    },
    Acknowledge {
        claim: Uuid,
    },
    Drained {
        claim: Uuid,
        sequence: u64,
        boot_ms: u64,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    binding: terminal_io::TerminalBinding,
    claim: Uuid,
    process: ProcessEvidence,
    deadline: u64,
}
struct Reader {
    socket: Socket,
    _stdin: Option<std::process::ChildStdin>,
    viewer: Option<crate::terminal_viewer::ViewerProcess>,
    process: ProcessEvidence,
    pidfd: OwnedFd,
    claim: Uuid,
    acknowledged: bool,
    deadline: u64,
    last_sequence: u64,
    last_drain: u64,
}
impl Drop for Reader {
    fn drop(&mut self) {
        let _ = ownership::signal(&self.pidfd, libc::SIGKILL);
    }
}
pub(crate) struct Owner {
    listener: Socket,
    path: PathBuf,
    inode: (u64, u64),
    pending: Option<(Socket, u64)>,
    reader: Option<Reader>,
    claimed: bool,
    failed: bool,
    viewer: bool,
}
pub(crate) struct Gate<'a> {
    pub highwater: &'a mut u64,
    pub pending_observation: bool,
}
impl Owner {
    pub(crate) fn new(dir: &Path) -> Result<Self> {
        Self::create(dir, false)
    }
    pub(crate) fn new_viewer(dir: &Path) -> Result<Self> {
        Self::create(dir, true)
    }
    fn create(dir: &Path, viewer: bool) -> Result<Self> {
        let path = dir.join(if viewer { VIEWER_SOCKET } else { SOCKET });
        let listener = Socket::new()?;
        let (address, length) = Socket::address(&path)?;
        ensure!(
            unsafe {
                libc::bind(
                    listener.raw(),
                    (&address as *const libc::sockaddr_un).cast(),
                    length,
                )
            } == 0,
            "reader socket collision"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        ensure!(
            unsafe { libc::listen(listener.raw(), 1) } == 0,
            "reader local listen"
        );
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            inode: (metadata.dev(), metadata.ino()),
            pending: None,
            reader: None,
            claimed: false,
            failed: false,
            viewer,
        })
    }
    pub(crate) fn drained_after(&self, after: u64) -> bool {
        self.reader.as_ref().is_some_and(|r| {
            r.acknowledged
                && r.last_sequence > 0
                && r.last_drain >= after
                && ownership::raw_boot_ms() < r.deadline
                && !r.socket.hung_up().unwrap_or(true)
                && ownership::pidfd_is_live(&r.pidfd).unwrap_or(false)
                && ownership::process_evidence(r.process.pid).is_ok_and(|p| p == r.process)
                && std::fs::read_to_string(format!("/proc/{}/stat", r.process.pid)).is_ok_and(
                    |stat| {
                        stat.rsplit_once(") ").is_some_and(|(_, fields)| {
                            fields
                                .as_bytes()
                                .first()
                                .is_some_and(|v| *v != b'T' && *v != b't')
                        })
                    },
                )
        })
    }
    pub(crate) fn tick(
        &mut self,
        dir: &Path,
        config: &SealedLaunch,
        host: &HostObservation,
        wall_deadline: u64,
        guard: &terminal_io::DirectoryGuard,
        gate: Gate<'_>,
    ) {
        if self.failed {
            return;
        }
        if self
            .try_tick(dir, config, host, wall_deadline, guard, gate)
            .is_err()
        {
            self.reader.take();
            self.pending.take();
            // Only the disposable viewer can attach again. The original
            // control reader retains its one-shot continuity/baseline rules.
            self.claimed = !self.viewer;
            self.failed = !self.viewer;
        }
    }
    fn try_tick(
        &mut self,
        dir: &Path,
        config: &SealedLaunch,
        host: &HostObservation,
        wall_deadline: u64,
        guard: &terminal_io::DirectoryGuard,
        gate: Gate<'_>,
    ) -> Result<()> {
        let Gate {
            highwater,
            pending_observation,
        } = gate;
        guard.verify()?;
        let metadata = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && (metadata.dev(), metadata.ino()) == self.inode,
            "reader socket replaced"
        );
        let now = ownership::raw_boot_ms();
        if let Some(reader) = &mut self.reader {
            ensure!(
                (host.phase == HostPhase::Running
                    || pending_observation && host.phase == HostPhase::Starting)
                    && now < reader.deadline
                    && now < host.permit_deadline_boot_ms
                    && now < wall_deadline
                    && ownership::pidfd_is_live(&reader.pidfd)?,
                "reader lease ended"
            );
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", reader.process.pid))?;
            let state = stat
                .rsplit_once(") ")
                .context("reader process state")?
                .1
                .as_bytes()[0];
            ensure!(state != b'T' && state != b't', "reader process stopped");
            if let Some(viewer) = &mut reader.viewer {
                viewer.tick(dir, config, host, wall_deadline, reader.process.pid)?;
            }
            if let Some(packet) = reader.socket.receive()? {
                ensure!(packet.fd.is_none(), "unexpected reader descriptor");
                match serde_json::from_slice::<Request>(&packet.bytes)? {
                    Request::Acknowledge { claim } => {
                        ensure!(
                            claim == reader.claim && !reader.acknowledged,
                            "reader acknowledgement conflict"
                        );
                        reader.acknowledged = true;
                    }
                    Request::Drained {
                        claim,
                        sequence,
                        boot_ms,
                    } => {
                        ensure!(
                            reader.acknowledged && claim == reader.claim,
                            "reader acknowledgement/claim"
                        );
                        reader.deadline = drain_deadline(
                            now,
                            reader.deadline,
                            (reader.last_sequence, reader.last_drain),
                            (sequence, boot_ms),
                            host.permit_deadline_boot_ms.min(wall_deadline),
                        )?;
                        reader.last_sequence = sequence;
                        reader.last_drain = boot_ms;
                    }
                    _ => bail!("unexpected reader command"),
                }
            }
            return Ok(());
        }
        if self.claimed
            || !(host.phase == HostPhase::Running
                || pending_observation && host.phase == HostPhase::Starting)
        {
            return Ok(());
        }
        if self.pending.is_none() {
            self.pending = self.listener.accept()?.map(|s| (s, now + LEASE_MS));
        }
        if let Some((socket, deadline)) = &self.pending {
            ensure!(now < *deadline, "reader registration deadline");
            if let Some(packet) = socket.receive()? {
                ensure!(packet.fd.is_none(), "unexpected registration descriptor");
                let Request::Register { binding, claim } =
                    serde_json::from_slice::<Request>(&packet.bytes)?
                else {
                    bail!("reader registration required")
                };
                if pending_observation {
                    ensure!(
                        config
                            .observed
                            .as_ref()
                            .is_some_and(|spec| spec.terminal == binding.terminal),
                        "preexec reader terminal mismatch"
                    );
                }
                if self.viewer {
                    ensure!(
                        !pending_observation
                            && host.phase == HostPhase::Running
                            && config.observed.as_ref().is_some_and(|s| s.interactive())
                            && config.manifest.terminal().input_enabled,
                        "viewer requires original interactive Running"
                    );
                }
                terminal_io::verify_output_claim(
                    dir,
                    &binding,
                    claim,
                    host,
                    guard,
                    highwater,
                    pending_observation,
                )?;
                self.claimed = true;
                let now = ownership::raw_boot_ms();
                ensure!(
                    now < host.permit_deadline_boot_ms && now < wall_deadline,
                    "execution expired before reader spawn"
                );
                let socket = self.pending.take().unwrap().0;
                let (mut child, viewer, viewer_stdout) = if self.viewer {
                    let (child, viewer, stdout) =
                        crate::terminal_viewer::spawn(dir, config, host, wall_deadline)?;
                    (child, Some(viewer), Some(stdout))
                } else {
                    (
                        Command::new(&config.tmux)
                            .args(["-N", "-C", "-S"])
                            .arg(dir.join("tmux.sock"))
                            .args([
                                "attach-session",
                                "-f",
                                "read-only,ignore-size,pause-after=1",
                                "-t",
                                "aq",
                            ])
                            .env_clear()
                            .env("PATH", "/usr/bin:/bin")
                            .env("TERM", "xterm-256color")
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::null())
                            .spawn()?,
                        None,
                        None,
                    )
                };
                // This child belongs to the guardian before any capability is sent.
                // On setup failure it is killed directly and reaped by the guardian.
                let setup = (|| -> Result<_> {
                    let process = ownership::process_evidence(child.id())?;
                    let pidfd = ownership::verified_pidfd(&process)?;
                    let stdin = if self.viewer {
                        None
                    } else {
                        Some(child.stdin.take().context("reader stdin")?)
                    };
                    let stdout = if let Some(stdout) = viewer_stdout {
                        stdout
                    } else {
                        File::from(OwnedFd::from(child.stdout.take().context("reader stdout")?))
                    };
                    Ok((process, pidfd, stdin, stdout))
                })();
                let (process, pidfd, stdin, stdout) = match setup {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = child.kill();
                        return Err(e);
                    }
                };
                let deadline = now
                    .saturating_add(LEASE_MS)
                    .min(host.permit_deadline_boot_ms)
                    .min(wall_deadline);
                self.reader = Some(Reader {
                    socket,
                    _stdin: stdin,
                    viewer,
                    process: process.clone(),
                    pidfd,
                    claim,
                    acknowledged: false,
                    deadline,
                    last_sequence: 0,
                    last_drain: now,
                });
                #[cfg(feature = "test-support")]
                durable_new(
                    &dir.join("reader.process.json"),
                    &serde_json::to_vec(&process)?,
                )?;
                self.reader.as_ref().unwrap().socket.send(
                    &Ready {
                        binding: *binding,
                        claim,
                        process,
                        deadline,
                    },
                    Some(stdout.as_raw_fd()),
                )?;
            }
        }
        Ok(())
    }
}
pub(crate) struct Client {
    socket: Socket,
    claim: Uuid,
    sequence: u64,
    last_sent: u64,
}
impl Client {
    pub(crate) fn open(
        terminal: &terminal_io::TerminalHandle,
        claim: Uuid,
    ) -> Result<(Self, File, ProcessEvidence)> {
        Self::open_at(terminal, claim, SOCKET)
    }
    pub(crate) fn open_viewer(
        terminal: &terminal_io::TerminalHandle,
        claim: Uuid,
    ) -> Result<(Self, File, ProcessEvidence)> {
        Self::open_at(terminal, claim, VIEWER_SOCKET)
    }
    fn open_at(
        terminal: &terminal_io::TerminalHandle,
        claim: Uuid,
        name: &str,
    ) -> Result<(Self, File, ProcessEvidence)> {
        let (dir, _, _) = terminal.output_settings()?;
        let socket = Socket::connect(&dir.join(name))?;
        let peer = socket.credentials()?;
        ensure!(
            peer.pid as u32 == terminal.binding().guardian.process.pid
                && ownership::process_evidence(peer.pid as u32)?
                    == terminal.binding().guardian.process,
            "reader guardian identity"
        );
        socket.send(
            &Request::Register {
                binding: Box::new(terminal.binding().clone()),
                claim,
            },
            None,
        )?;
        test_pause(&dir, "before-ready")?;
        let deadline = Instant::now() + Duration::from_millis(500);
        let packet = loop {
            if let Some(packet) = socket.receive()? {
                break packet;
            }
            ensure!(Instant::now() < deadline, "reader ready timeout");
            std::thread::sleep(Duration::from_millis(2));
        };
        let ready: Ready = serde_json::from_slice(&packet.bytes)?;
        ensure!(
            ready.binding == *terminal.binding()
                && ready.claim == claim
                && ownership::raw_boot_ms() < ready.deadline,
            "reader ready identity/deadline"
        );
        let fd = packet.fd.context("reader output descriptor missing")?;
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        ensure!(
            flags >= 0
                && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                    == 0,
            "reader nonblocking descriptor"
        );
        test_pause(&dir, "before-ack")?;
        socket.send(&Request::Acknowledge { claim }, None)?;
        Ok((
            Self {
                socket,
                claim,
                sequence: 0,
                last_sent: 0,
            },
            File::from(fd),
            ready.process,
        ))
    }
    pub(crate) fn check(&self) -> Result<()> {
        ensure!(
            self.socket.receive()?.is_none(),
            "unexpected reader response"
        );
        Ok(())
    }
    pub(crate) fn drained(&mut self) -> Result<()> {
        let now = ownership::raw_boot_ms();
        if now.saturating_sub(self.last_sent) < DRAIN_CADENCE_MS {
            return Ok(());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("reader drain sequence overflow")?;
        self.socket.send(
            &Request::Drained {
                claim: self.claim,
                sequence: self.sequence,
                boot_ms: now,
            },
            None,
        )?;
        self.last_sent = now;
        Ok(())
    }
}
use std::os::unix::fs::FileTypeExt;

fn test_pause(_dir: &Path, _stage: &str) -> Result<()> {
    #[cfg(feature = "test-support")]
    if std::env::var("AQ_HOST_READER_PAUSE").ok().as_deref() == Some(_stage) {
        durable_new(&_dir.join("reader-owner-paused"), _stage.as_bytes())?;
        unsafe {
            libc::raise(libc::SIGSTOP);
        }
    }
    Ok(())
}

fn drain_deadline(
    now: u64,
    deadline: u64,
    last: (u64, u64),
    next: (u64, u64),
    ceiling: u64,
) -> Result<u64> {
    ensure!(
        now < deadline
            && next.0 > last.0
            && next.1 >= last.1
            && next.1 <= now
            && now < next.1.saturating_add(LEASE_MS),
        "reader drain receipt stale"
    );
    Ok(next.1.saturating_add(LEASE_MS).min(ceiling))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_old_drain_cannot_extend_from_dequeue_or_revive_expiry() {
        assert_eq!(
            drain_deadline(1100, 1500, (0, 500), (1, 500), 9000).unwrap(),
            1500
        );
        assert!(drain_deadline(1500, 1500, (0, 500), (1, 500), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (1, 500), (1, 1000), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (0, 500), (1, 1101), 9000).is_err());
        assert!(drain_deadline(1100, 1500, (0, 500), (1, 499), 9000).is_err());
        assert_eq!(
            drain_deadline(1100, 1500, (0, 500), (1, 1000), 1200).unwrap(),
            1200
        );
    }
    #[test]
    fn packet_fd_is_bounded_and_close_on_exec_and_extra_mutation_fields_refuse() {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let a = Socket(unsafe { OwnedFd::from_raw_fd(pair[0]) });
        let b = Socket(unsafe { OwnedFd::from_raw_fd(pair[1]) });
        let file = tempfile::tempfile().unwrap();
        a.send(&"bounded", Some(file.as_raw_fd())).unwrap();
        let packet = b.receive().unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&packet.bytes).unwrap(),
            "bounded"
        );
        let fd = packet.fd.unwrap();
        assert!(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC != 0);
        assert_eq!(
            File::from(fd).metadata().unwrap().ino(),
            file.metadata().unwrap().ino()
        );
        assert!(
            serde_json::from_str::<Request>(
                r#"{"kind":"Acknowledge","claim":"00000000-0000-0000-0000-000000000001","pid":1}"#
            )
            .is_err()
        );
        assert!(a.send(&"x".repeat(MAX_PACKET), None).is_err());
    }
}
