//! Guardian-owned closed gate. Only exact local owner and physical pane peers.
use super::*;
use crate::local_ipc::Socket;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
const NAME: &str = "observed-gate.sock";
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Request {
    Admit {
        guardian: GuardianIdentity,
    },
    Hello {
        guardian: GuardianIdentity,
        process: ProcessEvidence,
        columns: u16,
        rows: u16,
    },
    Status {
        baseline_id: Uuid,
    },
    Confirm {
        baseline_id: Uuid,
    },
    Release {
        baseline_digest: Digest,
    },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClosedReady {
    pub guardian: GuardianIdentity,
    pub pane: PaneIdentity,
    pub baseline_id: Uuid,
    pub columns: u16,
    pub rows: u16,
    pub helper_ready_boot_ms: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Response {
    Admitted,
    Waiting,
    Closed {
        ready: ClosedReady,
    },
    Baseline {
        baseline: BlankBaselineRecord,
    },
    Released,
    Execute {
        guardian: GuardianIdentity,
        baseline_id: Uuid,
    },
}
pub(crate) struct Owner {
    listener: Socket,
    path: PathBuf,
    inode: (u64, u64),
    pending: Option<Socket>,
    control: Option<Socket>,
    helper: Option<Socket>,
    image: Option<File>,
    ready: Option<ClosedReady>,
    baseline: Option<BlankBaselineRecord>,
    released: bool,
}
impl Owner {
    pub(crate) fn new(directory: &Path) -> Result<Self> {
        let path = directory.join(NAME);
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
            "observed gate socket collision"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        ensure!(
            unsafe { libc::listen(listener.raw(), 2) } == 0,
            "observed gate listen"
        );
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            listener,
            path,
            inode: (metadata.dev(), metadata.ino()),
            pending: None,
            control: None,
            helper: None,
            image: None,
            ready: None,
            baseline: None,
            released: false,
        })
    }
    pub(crate) fn closed_ready(&self) -> Option<&ClosedReady> {
        self.ready.as_ref()
    }
    pub(crate) fn tick(
        &mut self,
        config: &SealedLaunch,
        host: &HostObservation,
        directory: &Path,
        readers: &reader_lease::Owner,
        guard: &terminal_io::DirectoryGuard,
    ) -> Result<bool> {
        guard.verify()?;
        let spec = config
            .observed
            .as_ref()
            .context("missing observed specification")?;
        let metadata = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.mode() & 0o777 == 0o600
                && metadata.uid() == unsafe { libc::geteuid() }
                && (metadata.dev(), metadata.ino()) == self.inode,
            "observed gate socket replaced"
        );
        ensure!(
            ownership::raw_boot_ms() < host.permit_deadline_boot_ms
                && ownership::raw_boot_ms() < config.wall_deadline()?,
            "observed gate authority expired"
        );
        if self.released {
            let helper = self.helper.as_ref().context("released helper missing")?;
            if helper.hung_up()? {
                exec::live_pane(&self.ready.as_ref().context("released pane missing")?.pane)?;
                exec::observed_image(
                    &self
                        .ready
                        .as_ref()
                        .context("released identity missing")?
                        .pane
                        .pane,
                    &spec.program,
                )?;
                return Ok(true);
            }
            ensure!(
                helper.receive()?.is_none(),
                "unexpected released helper packet"
            );
            return Ok(false);
        }
        if self.pending.is_none() {
            self.pending = self.listener.accept()?;
        }
        if let Some(socket) = self.pending.as_ref()
            && let Some(packet) = socket.receive()?
        {
            let peer = socket.credentials()?;
            match serde_json::from_slice::<Request>(&packet.bytes)? {
                Request::Admit { guardian } => {
                    ensure!(
                        guardian == host.guardian
                            && peer.pid as u32 == spec.owner.pid
                            && ownership::process_evidence(spec.owner.pid)? == spec.owner
                            && self.control.is_none()
                            && self.image.is_none()
                            && host.phase == HostPhase::Waiting,
                        "observed program owner/admission mismatch"
                    );
                    let file =
                        File::from(packet.fd.context("observed program descriptor missing")?);
                    executable::verify_file(&file, &spec.program)?;
                    socket.send(&Response::Admitted, None)?;
                    self.image = Some(file);
                    self.control = self.pending.take();
                }
                Request::Hello {
                    guardian,
                    process,
                    columns,
                    rows,
                } => {
                    let pane = host
                        .pane
                        .as_ref()
                        .context("observed pane not established")?;
                    ensure!(
                        packet.fd.is_none()
                            && guardian == host.guardian
                            && host.phase == HostPhase::Starting
                            && self.image.is_some()
                            && self.helper.is_none()
                            && process == pane.pane
                            && peer.pid as u32 == process.pid
                            && ownership::process_evidence(process.pid)? == process
                            && columns == spec.columns
                            && rows == spec.rows,
                        "observed helper identity/geometry mismatch"
                    );
                    self.ready = Some(ClosedReady {
                        guardian,
                        pane: pane.clone(),
                        baseline_id: spec.baseline_id,
                        columns,
                        rows,
                        helper_ready_boot_ms: ownership::raw_boot_ms(),
                    });
                    self.helper = self.pending.take();
                }
                _ => bail!("observed gate first packet refused"),
            }
        }
        if let Some(socket) = self.control.as_ref()
            && let Some(packet) = socket.receive()?
        {
            ensure!(
                packet.fd.is_none(),
                "unexpected observed control descriptor"
            );
            match serde_json::from_slice::<Request>(&packet.bytes)? {
                Request::Status { baseline_id } => {
                    ensure!(
                        baseline_id == spec.baseline_id,
                        "observed baseline mismatch"
                    );
                    socket.send(
                        &self
                            .ready
                            .as_ref()
                            .map_or(Response::Waiting, |ready| Response::Closed {
                                ready: ready.clone(),
                            }),
                        None,
                    )?;
                }
                Request::Confirm { baseline_id } => {
                    ensure!(
                        baseline_id == spec.baseline_id,
                        "observed baseline mismatch"
                    );
                    let ready = self.ready.as_ref().context("closed helper missing")?;
                    verify_closed(config, host, ready, directory)?;
                    if !readers.drained_after(ready.helper_ready_boot_ms) {
                        socket.send(&Response::Waiting, None)?;
                    } else {
                        let baseline = self.baseline.get_or_insert_with(|| BlankBaselineRecord {
                            ready: ready.clone(),
                            terminal: spec.terminal.clone(),
                            renderer: spec.renderer.clone(),
                            configuration_digest: host.guardian.intent.configuration_digest.clone(),
                        });
                        socket.send(
                            &Response::Baseline {
                                baseline: baseline.clone(),
                            },
                            None,
                        )?;
                    }
                }
                Request::Release { baseline_digest } => {
                    let ready = self.ready.as_ref().context("closed helper missing")?;
                    verify_closed(config, host, ready, directory)?;
                    let baseline = self.baseline.as_ref().context("baseline not confirmed")?;
                    ensure!(
                        Digest::of(&serde_json::to_vec(baseline)?) == baseline_digest
                            && readers.drained_after(ready.helper_ready_boot_ms),
                        "baseline release digest/read lease mismatch"
                    );
                    guard.verify()?;
                    durable_new(
                        &directory.join("pane-exec.claim"),
                        &serde_json::to_vec(baseline)?,
                    )?;
                    guard.verify()?;
                    verify_closed(config, host, ready, directory)?;
                    ensure!(
                        readers.drained_after(ready.helper_ready_boot_ms),
                        "reader lost during release persistence"
                    );
                    self.released = true; // No second release even if delivery/ACK is uncertain.
                    self.helper
                        .as_ref()
                        .context("closed helper missing")?
                        .send(
                            &Response::Execute {
                                guardian: host.guardian.clone(),
                                baseline_id: spec.baseline_id,
                            },
                            Some(
                                self.image
                                    .as_ref()
                                    .context("sealed image missing")?
                                    .as_raw_fd(),
                            ),
                        )?;
                    socket.send(&Response::Released, None)?;
                    #[cfg(feature = "test-support")]
                    supervisor::fault_point(directory, "observed-exec-permit")?;
                }
                _ => bail!("observed control operation refused"),
            }
        }
        // The helper stays blocked here. EOF, client loss and every malformed
        // packet fail closed; the guardian's independent stop path reaps it.
        if !self.released
            && let Some(helper) = self.helper.as_ref()
        {
            ensure!(
                helper.receive()?.is_none(),
                "unexpected closed helper packet"
            );
        }
        Ok(false)
    }
}
fn verify_closed(
    config: &SealedLaunch,
    host: &HostObservation,
    ready: &ClosedReady,
    directory: &Path,
) -> Result<()> {
    ensure!(
        host.phase == HostPhase::Starting
            && host.guardian == ready.guardian
            && host.pane.as_ref() == Some(&ready.pane)
            && ownership::raw_boot_ms() < host.permit_deadline_boot_ms
            && ownership::raw_boot_ms() < config.wall_deadline()?
            && !directory.join("stop.request").exists(),
        "closed baseline authority/identity changed"
    );
    exec::live_pane(&ready.pane)?;
    ensure!(
        ownership::process_evidence(ready.pane.pane.pid)? == ready.pane.pane,
        "closed helper incarnation changed"
    );
    let current = std::fs::metadata(format!("/proc/{}/exe", ready.pane.pane.pid))?;
    let expected = std::fs::metadata(&config.helper)?;
    ensure!(
        (current.dev(), current.ino()) == (expected.dev(), expected.ino()),
        "closed helper executable changed"
    );
    let stdin = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(format!("/proc/{}/fd/0", ready.pane.pane.pid))?;
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::ioctl(stdin.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col == ready.columns
            && size.ws_row == ready.rows,
        "closed pane geometry changed"
    );
    Ok(())
}
pub(crate) struct Client {
    socket: Socket,
}
impl Client {
    pub(crate) fn admit(
        directory: &Path,
        guardian: &GuardianIdentity,
        program: &ObservedProgram,
        deadline: u64,
    ) -> Result<Self> {
        let socket = Socket::connect(&directory.join(NAME))?;
        ensure!(
            socket.credentials()?.pid as u32 == guardian.process.pid
                && ownership::process_evidence(guardian.process.pid)? == guardian.process,
            "observed guardian peer mismatch"
        );
        socket.send(
            &Request::Admit {
                guardian: guardian.clone(),
            },
            Some(program.file().as_raw_fd()),
        )?;
        loop {
            if let Some(packet) = socket.receive()? {
                ensure!(
                    packet.fd.is_none()
                        && matches!(
                            serde_json::from_slice::<Response>(&packet.bytes)?,
                            Response::Admitted
                        ),
                    "observed program acknowledgement mismatch"
                );
                return Ok(Self { socket });
            }
            ensure!(
                ownership::raw_boot_ms() < deadline,
                "observed admission deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn exchange(&self, request: &Request, deadline: u64) -> Result<Response> {
        ensure!(
            ownership::raw_boot_ms() < deadline,
            "observed command deadline"
        );
        self.socket.send(request, None)?;
        loop {
            if let Some(packet) = self.socket.receive()? {
                ensure!(
                    packet.fd.is_none(),
                    "unexpected observed control reply descriptor"
                );
                return Ok(serde_json::from_slice(&packet.bytes)?);
            }
            ensure!(
                ownership::raw_boot_ms() < deadline,
                "observed reply deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    pub(crate) fn confirm(
        &self,
        baseline_id: Uuid,
        deadline: u64,
    ) -> Result<Option<VerifiedBlankBaseline>> {
        match self.exchange(&Request::Confirm { baseline_id }, deadline)? {
            Response::Waiting => Ok(None),
            Response::Baseline { baseline } => Ok(Some(VerifiedBlankBaseline(baseline))),
            _ => bail!("unexpected baseline response"),
        }
    }
    pub(crate) fn release(&self, baseline_digest: Digest, deadline: u64) -> Result<()> {
        ensure!(
            matches!(
                self.exchange(&Request::Release { baseline_digest }, deadline)?,
                Response::Released
            ),
            "unexpected release response"
        );
        Ok(())
    }
    pub(crate) fn closed(&self, baseline_id: Uuid, deadline: u64) -> Result<Option<ClosedReady>> {
        self.socket.send(&Request::Status { baseline_id }, None)?;
        loop {
            if let Some(packet) = self.socket.receive()? {
                ensure!(packet.fd.is_none(), "unexpected observed reply descriptor");
                return match serde_json::from_slice(&packet.bytes)? {
                    Response::Waiting => Ok(None),
                    Response::Closed { ready } => Ok(Some(ready)),
                    _ => bail!("unexpected observed gate response"),
                };
            }
            ensure!(
                ownership::raw_boot_ms() < deadline,
                "observed status deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
/// Dedicated pane helper; no terminal bytes before release. The first component
/// releases only on the guardian's exact baseline/reader-checked FD permit.
pub(crate) fn wait_closed(
    directory: &Path,
    config: &SealedLaunch,
    guardian: &GuardianIdentity,
    server: &ProcessEvidence,
) -> Result<()> {
    let spec = config
        .observed
        .as_ref()
        .context("observed helper configuration")?;
    let socket = Socket::connect(&directory.join(NAME))?;
    ensure!(
        socket.credentials()?.pid as u32 == guardian.process.pid
            && ownership::process_evidence(guardian.process.pid)? == guardian.process,
        "observed helper guardian mismatch"
    );
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } == 0,
        "observed helper terminal geometry"
    );
    socket.send(
        &Request::Hello {
            guardian: guardian.clone(),
            process: ownership::process_evidence(std::process::id())?,
            columns: size.ws_col,
            rows: size.ws_row,
        },
        None,
    )?;
    loop {
        ensure!(
            ownership::raw_boot_ms() < config.permit.expires_boot_ms
                && ownership::raw_boot_ms() < config.wall_deadline()?
                && !directory.join("stop.request").exists(),
            "observed helper original authority expired"
        );
        if let Some(packet) = socket.receive()? {
            let Response::Execute {
                guardian: released,
                baseline_id,
            } = serde_json::from_slice(&packet.bytes)?
            else {
                bail!("observed helper release operation refused")
            };
            ensure!(
                released == *guardian && baseline_id == spec.baseline_id,
                "observed helper release identity mismatch"
            );
            let file = File::from(
                packet
                    .fd
                    .context("observed helper sealed descriptor missing")?,
            );
            executable::verify_file(&file, &spec.program)?;
            let rendered = config
                .rendered_startup
                .as_ref()
                .context("observed rendered payload missing")?;
            rendered.verify(
                config.manifest.task()?,
                &read_bytes(&directory.join("context-0.bin"), 1_048_576)?,
            )?;
            ensure!(
                ownership::raw_boot_ms() < config.permit.expires_boot_ms
                    && ownership::raw_boot_ms() < config.wall_deadline()?
                    && !directory.join("stop.request").exists()
                    && supervisor::guardian_live(guardian),
                "authority expired immediately before sealed exec"
            );
            ensure!(
                ownership::pidfd_is_live(&ownership::verified_pidfd(server)?)?,
                "owned server lost immediately before sealed exec"
            );
            let startup_stdin = exec::startup_stdin(config, directory)?;
            return exec::enter(
                &file,
                &spec.program,
                directory,
                config,
                guardian,
                server,
                startup_stdin.as_ref(),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
