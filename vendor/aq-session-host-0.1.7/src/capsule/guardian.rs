use super::*;
use std::{mem::size_of, net::Ipv4Addr};

pub(super) fn listener(fd: OwnedFd, origin: &str) -> Result<TcpListener> {
    let mut kind = 0i32;
    let mut accepting = 0i32;
    let mut length = size_of::<i32>() as libc::socklen_t;
    ensure!(
        unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&mut kind as *mut i32).cast(),
                &mut length,
            )
        } == 0
            && kind == libc::SOCK_STREAM,
        "capsule listener type"
    );
    ensure!(
        unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                (&mut accepting as *mut i32).cast(),
                &mut length,
            )
        } == 0
            && accepting == 1,
        "capsule listener state"
    );
    let listener = unsafe { TcpListener::from_raw_fd(fd.into_raw_fd()) };
    let address = listener.local_addr()?;
    ensure!(
        address.ip() == std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
            && address.port() > 0
            && origin == format!("http://{address}"),
        "capsule listener exact origin"
    );
    listener.set_nonblocking(true)?;
    Ok(listener)
}
pub(super) fn verify_image(process: &ProcessEvidence, image: &seal::Pin) -> Result<()> {
    let pidfd = ownership::verified_pidfd(process)?;
    ensure!(
        ownership::pidfd_is_live(&pidfd)?,
        "capsule exec process exited"
    );
    let file = File::open(format!("/proc/{}/exe", process.pid))?;
    let metadata = file.metadata()?;
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    ensure!(
        metadata.dev() == image.device
            && metadata.ino() == image.inode
            && metadata.len() == image.bytes
            && seals >= 0
            && seals & seal::SEALS == seal::SEALS,
        "capsule exact actual sealed executable"
    );
    ensure!(
        ownership::process_evidence(process.pid)? == *process && ownership::pidfd_is_live(&pidfd)?,
        "capsule exec incarnation changed"
    );
    Ok(())
}
fn parent(pid: u32) -> Result<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(") ")
        .context("capsule process stat")?
        .1
        .split_whitespace()
        .nth(1)
        .context("capsule parent")?
        .parse()?)
}
fn namespaces(pid: u32, inner: u32, init: u32) -> Result<()> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let ids = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .context("capsule NSpid")?
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        ids.len() >= 2 && ids.len() <= 32 && ids[0] == pid && ids.last() == Some(&inner),
        "capsule translated actual PID"
    );
    for kind in ["user", "mnt", "pid", "net", "ipc", "uts"] {
        let actual = std::fs::metadata(format!("/proc/{pid}/ns/{kind}"))?;
        let outer = std::fs::metadata(format!("/proc/thread-self/ns/{kind}"))?;
        let initial = std::fs::metadata(format!("/proc/{init}/ns/{kind}"))?;
        ensure!(
            actual.ino() != outer.ino() && actual.ino() == initial.ino(),
            "capsule namespace identity mismatch"
        );
    }
    Ok(())
}
fn validate(plan: &PlanRecord, files: &mut [File]) -> Result<()> {
    let now = ownership::raw_boot_ms();
    ensure!(
        !plan.nonce.is_nil()
            && plan.deadline > now
            && plan.deadline - now <= MAX_LIFETIME_MS
            && files.len() == plan.grants.len() + 3
            && plan.grants.len() <= 10,
        "capsule plan bounds"
    );
    ensure!(
        plan.arguments.len() <= 16
            && plan
                .arguments
                .iter()
                .all(|a| a.len() <= 2048 && !a.contains('\0')),
        "capsule argument limits"
    );
    seal::verify(&mut files[0], &plan.program, MAX_PROGRAM)?;
    ensure!(plan.program.executable, "capsule program executable");
    seal::verify(&mut files[1], &plan.stdin, MAX_STDIN as u64)?;
    ensure!(!plan.stdin.executable, "capsule stdin nonexecutable");
    plan.root_pin.verify(&files[2])?;
    ensure!(
        std::fs::canonicalize(&plan.root)? == plan.root
            && std::fs::read_dir(&plan.root)?.next().is_none(),
        "capsule retained fresh root"
    );
    let named = std::fs::symlink_metadata(&plan.root)?;
    ensure!(
        named.is_dir()
            && named.dev() == plan.root_pin.device
            && named.ino() == plan.root_pin.inode
            && named.uid() == unsafe { libc::geteuid() }
            && named.mode() & 0o777 == 0o700,
        "capsule root binding/mode"
    );
    let mut targets = Vec::new();
    for (grant, file) in plan.grants.iter().zip(&files[3..]) {
        ensure!(
            grant.source.is_absolute()
                && std::fs::canonicalize(&grant.source)? == grant.source
                && !plan.root.starts_with(&grant.source)
                && !grant.source.starts_with(&plan.root),
            "capsule original source route"
        );
        grant.target.relative()?;
        grant.pin.verify(file)?;
        ensure!(
            !targets.contains(&grant.target)
                && (!grant.writable
                    || matches!(
                        grant.target,
                        MountTarget::Workspace | MountTarget::GitMetadata | MountTarget::Allowed(_)
                    )),
            "capsule mount target policy"
        );
        targets.push(grant.target);
    }
    ensure!(
        targets.contains(&MountTarget::Workspace),
        "capsule workspace absent"
    );
    prepared::validate_plan(plan, files)?;
    Ok(())
}
fn cleanup(init: Option<&OwnedFd>) -> Result<u64> {
    if let Some(init) = init {
        ownership::signal(init, libc::SIGKILL)?;
    }
    let end = ownership::raw_boot_ms() + CLEANUP_MS;
    let mut reaped = 0;
    loop {
        let batch = ownership::reap_children()?;
        reaped += batch.processes.len() as u64;
        if batch.kernel_echild {
            return Ok(reaped);
        }
        ensure!(
            ownership::raw_boot_ms() < end,
            "capsule physical cleanup uncertain"
        );
        ownership::signal_children_bounded(libc::SIGKILL)?;
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn caller_still_waiting(control: &wire::Socket) -> Result<()> {
    let mut poll = libc::pollfd {
        fd: control.raw(),
        events: libc::POLLIN,
        revents: 0,
    };
    ensure!(
        unsafe { libc::poll(&mut poll, 1, 0) } >= 0 && poll.revents == 0,
        "capsule caller/control lost before exec witness"
    );
    Ok(())
}
pub(super) fn run(control: wire::Socket) -> Result<()> {
    let worker = unsafe { libc::getppid() };
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } == 0
            && unsafe { libc::getppid() } == worker,
        "capsule guardian parent fence"
    );
    ownership::become_subreaper()?;
    let _emergency = ownership::EmergencyReaper;
    let packet = control.recv::<Message>(ownership::raw_boot_ms() + MAX_LIFETIME_MS)?;
    ensure!(
        packet.credentials.pid == worker && packet.fd.is_none(),
        "capsule original caller"
    );
    let Message::Plan(plan) = packet.value else {
        bail!("capsule first message must be plan")
    };
    let now = ownership::raw_boot_ms();
    ensure!(
        !plan.nonce.is_nil() && plan.deadline > now && plan.deadline - now <= MAX_LIFETIME_MS,
        "capsule initial original deadline"
    );
    let mut files = Vec::new();
    ensure!(plan.grants.len() <= 10, "capsule descriptor count");
    for index in 0..plan.grants.len() + 3 {
        let packet = control.recv::<Message>(plan.deadline)?;
        ensure!(
            packet.credentials.pid == worker
                && matches!(packet.value, Message::Descriptor { nonce, index: received } if nonce == plan.nonce && received == index),
            "capsule exact descriptor sequence"
        );
        files.push(File::from(
            packet.fd.context("capsule missing retained descriptor")?,
        ));
    }
    validate(&plan, &mut files)?;
    let (inner, setup_socket) = wire::Socket::pair()?;
    let guardian_pid = std::process::id();
    let bootstrap = unsafe { libc::fork() };
    ensure!(bootstrap >= 0, "capsule setup fork");
    if bootstrap == 0 {
        drop(control);
        drop(inner);
        let result = setup::run(plan, files, setup_socket, guardian_pid);
        if let Err(error) = result {
            eprintln!("capsule trusted setup refused: {error:#}");
        }
        unsafe { libc::_exit(111) }
    }
    drop(setup_socket);
    drop(files);
    let packet = inner.recv::<Message>(plan.deadline)?;
    let Message::Init { nonce, inner_pid } = packet.value else {
        bail!("capsule init witness missing")
    };
    ensure!(
        nonce == plan.nonce && inner_pid == 1 && packet.fd.is_none(),
        "capsule init stage"
    );
    let init = ownership::process_evidence(packet.credentials.pid as u32)?;
    ensure!(
        parent(init.pid)? == bootstrap as u32,
        "capsule init ancestry"
    );
    let initfd = ownership::verified_pidfd(&init)?;
    namespaces(init.pid, inner_pid, init.pid)?;
    let packet = inner.recv::<Message>(plan.deadline)?;
    let Message::Listener { nonce, port } = packet.value else {
        bail!("capsule listener stage absent")
    };
    ensure!(
        nonce == plan.nonce && packet.credentials.pid as u32 == init.pid && port > 0,
        "capsule listener sender/stage"
    );
    let origin = format!("http://127.0.0.1:{port}");
    let listener = listener(
        packet.fd.context("capsule child-netns listener missing")?,
        &origin,
    )?;
    let packet = inner.recv::<Message>(plan.deadline)?;
    let Message::Harness { nonce, inner_pid } = packet.value else {
        bail!("capsule harness stage absent")
    };
    ensure!(
        nonce == plan.nonce && inner_pid > 1 && packet.fd.is_none(),
        "capsule harness stage binding"
    );
    let harness = ownership::process_evidence(packet.credentials.pid as u32)?;
    ensure!(
        parent(harness.pid)? == init.pid && harness.pid != init.pid,
        "capsule actual harness ancestry"
    );
    let harnessfd = ownership::verified_pidfd(&harness)?;
    namespaces(harness.pid, inner_pid, init.pid)?;
    caller_still_waiting(&control)?;
    let was_prepared = plan.prepared.is_some();
    if was_prepared {
        let helper = File::open("/proc/self/exe")?;
        let metadata = helper.metadata()?;
        verify_image(
            &harness,
            &seal::Pin {
                digest: Digest::of(b"sealed helper identity only"),
                bytes: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
                executable: true,
            },
        )?;
        let info = prepared::info(
            &plan,
            ownership::process_evidence(std::process::id())?,
            init.clone(),
            harness.clone(),
            origin.clone(),
        )?;
        control.send(
            &Message::Prepared(info.clone()),
            Some(listener.as_raw_fd()),
            plan.deadline,
        )?;
        let commit = match control.recv::<Message>(plan.deadline) {
            Ok(packet) if packet.credentials.pid == worker && packet.fd.is_none() => match packet
                .value
            {
                Message::Commit(record) if prepared::validate_record(&record, &info).is_ok() => {
                    Some(record)
                }
                _ => None,
            },
            _ => None,
        };
        let Some(commit) = commit else {
            drop(inner);
            drop(listener);
            let reaped = cleanup(Some(&initfd))?;
            finish_report(
                &control,
                worker,
                plan.nonce,
                Message::PreparedStopped { info, reaped },
            )?;
            return Ok(());
        };
        caller_still_waiting(&control)?;
        inner.send(&Message::Commit(commit), None, plan.deadline)?;
    } else {
        inner.send(&Message::Release { nonce: plan.nonce }, None, plan.deadline)?;
    }
    drop(inner);
    while verify_image(&harness, &plan.program).is_err() {
        caller_still_waiting(&control)?;
        ensure!(
            ownership::raw_boot_ms() < plan.deadline && ownership::pidfd_is_live(&harnessfd)?,
            "capsule actual exec not established"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let witness = CapsuleWitness {
        guardian: ownership::process_evidence(std::process::id())?,
        init,
        harness,
        origin,
        nonce: plan.nonce,
    };
    control.send(
        &Message::Ready(witness),
        if was_prepared {
            None
        } else {
            Some(listener.as_raw_fd())
        },
        plan.deadline,
    )?;
    drop(listener);
    loop {
        if ownership::raw_boot_ms() >= plan.deadline
            || !ownership::pidfd_is_live(&initfd)?
            || !ownership::pidfd_is_live(&harnessfd)?
        {
            break;
        }
        let mut poll = libc::pollfd {
            fd: control.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 10) };
        ensure!(ready >= 0, "capsule control poll");
        if ready > 0 {
            // Control loss is a stop fence, never stopped proof.
            if let Ok(packet) = control.recv::<Message>(plan.deadline) {
                ensure!(
                    packet.credentials.pid == worker
                        && packet.fd.is_none()
                        && matches!(packet.value, Message::Stop { nonce } if nonce == plan.nonce),
                    "capsule stop sender/binding"
                );
            }
            break;
        }
    }
    let reaped = cleanup(Some(&initfd))?;
    ensure!(reaped > 0, "capsule owned descendant reap absent");
    // A dropped caller has no receiver; physical cleanup still completed.
    finish_report(
        &control,
        worker,
        plan.nonce,
        Message::Stopped {
            nonce: plan.nonce,
            reaped,
        },
    )?;
    Ok(())
}
fn finish_report(control: &wire::Socket, worker: i32, nonce: Uuid, report: Message) -> Result<()> {
    if control
        .send(&report, None, ownership::raw_boot_ms() + 100)
        .is_ok()
    {
        // Keep a late Stop from resetting a queued report when the endpoint
        // closes with unread data. ACK is delivery only: ECHILD is established.
        let until = ownership::raw_boot_ms() + 1000;
        while ownership::raw_boot_ms() < until {
            let Ok(packet) = control.recv::<Message>(until) else {
                break;
            };
            ensure!(
                packet.credentials.pid == worker && packet.fd.is_none(),
                "capsule stop ACK sender"
            );
            match packet.value {
                Message::StoppedAck { nonce: received } if received == nonce => break,
                Message::Stop { nonce: received } if received == nonce => {}
                _ => bail!("capsule stop ACK binding"),
            }
        }
    }
    Ok(())
}
