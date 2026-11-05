use super::*;
use std::ffi::CString;

fn write(path: &str, body: &str) -> Result<()> {
    use std::io::Write;
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(body.as_bytes())?;
    Ok(())
}
fn cap_drop() -> Result<()> {
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } == 0,
        "capsule NNP"
    );
    let last: u32 = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")?
        .trim()
        .parse()?;
    ensure!(last <= 63, "capsule capability ABI unsupported");
    for cap in 0..=last {
        ensure!(
            unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) } == 0,
            "capsule bounding cap drop"
        );
    }
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x20080522,
        pid: 0,
    };
    let data = [
        Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        Data {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];
    ensure!(
        unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } == 0,
        "capsule capset"
    );
    let status = std::fs::read_to_string("/proc/thread-self/status")?;
    let mut caps = 0;
    let mut nnp = false;
    for line in status.lines() {
        if line.starts_with("Cap") {
            let (_, value) = line.split_once(':').context("capsule cap status")?;
            ensure!(
                u64::from_str_radix(value.trim(), 16)? == 0,
                "capsule capability remains"
            );
            caps += 1;
        }
        if line.starts_with("NoNewPrivs:") {
            nnp = line.split_once(':').is_some_and(|(_, v)| v.trim() == "1");
        }
    }
    ensure!(caps == 5 && nnp, "capsule effective privilege fence");
    Ok(())
}
fn filter() -> Result<()> {
    fn statement(code: u32, value: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k: value,
        }
    }
    fn jump(code: u32, value: u32, jt: u8, jf: u8) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt,
            jf,
            k: value,
        }
    }
    let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
    let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    let ret = libc::BPF_RET | libc::BPF_K;
    let deny = 0x00050000 | libc::EPERM as u32;
    let allow = 0x7fff0000;
    let argument = |i: u32| 16 + i * 8;
    let mut code = vec![
        statement(load, 4),
        jump(equal, 0xc000003e, 1, 0),
        statement(ret, 0x80000000),
        statement(load, 0),
        jump(
            libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K,
            0x40000000,
            0,
            1,
        ),
        statement(ret, 0x80000000),
    ];
    for syscall in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        code.extend([jump(equal, syscall as u32, 0, 1), statement(ret, deny)]);
    }
    code.extend([
        jump(equal, libc::SYS_socket as u32, 0, 19),
        statement(load, argument(0)),
        jump(equal, libc::AF_INET as u32, 2, 0),
        jump(equal, libc::AF_INET6 as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(1)),
        statement(
            libc::BPF_ALU | libc::BPF_AND | libc::BPF_K,
            !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32,
        ),
        jump(equal, libc::SOCK_STREAM as u32, 0, 5),
        statement(load, argument(2)),
        jump(equal, 0, 2, 0),
        jump(equal, libc::IPPROTO_TCP as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        jump(equal, libc::SOCK_DGRAM as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(2)),
        jump(equal, 0, 2, 0),
        jump(equal, libc::IPPROTO_UDP as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        jump(equal, libc::SYS_socketpair as u32, 0, 12),
        statement(load, argument(0)),
        jump(equal, libc::AF_UNIX as u32, 1, 0),
        statement(ret, deny),
        statement(load, argument(2)),
        jump(equal, 0, 1, 0),
        statement(ret, deny),
        statement(load, argument(1)),
        statement(
            libc::BPF_ALU | libc::BPF_AND | libc::BPF_K,
            !(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u32,
        ),
        jump(equal, libc::SOCK_STREAM as u32, 2, 0),
        jump(equal, libc::SOCK_SEQPACKET as u32, 1, 0),
        statement(ret, deny),
        statement(ret, allow),
        statement(ret, allow),
    ]);
    let program = libc::sock_fprog {
        len: code.len() as u16,
        filter: code.as_mut_ptr(),
    };
    ensure!(
        unsafe { libc::syscall(libc::SYS_seccomp, 1, 0, &program) } == 0,
        "capsule IPC filter"
    );
    Ok(())
}
fn loopback() -> Result<()> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    ensure!(fd >= 0, "capsule lo control");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    request.ifr_name[0] = b'l' as _;
    request.ifr_name[1] = b'o' as _;
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } == 0,
        "capsule lo flags"
    );
    unsafe {
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as i16;
    }
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), libc::SIOCSIFFLAGS, &request) } == 0,
        "capsule lo up"
    );
    Ok(())
}
fn remap(
    files: Vec<File>,
    control: wire::Socket,
    listener: TcpListener,
) -> Result<(wire::Socket, TcpListener)> {
    let image = unsafe { libc::fcntl(files[0].as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    let stdin = unsafe { libc::fcntl(files[1].as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    let channel = unsafe { libc::fcntl(control.raw(), libc::F_DUPFD_CLOEXEC, 64) };
    let server = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) };
    ensure!(
        image >= 0 && stdin >= 0 && channel >= 0 && server >= 0,
        "capsule remap retain"
    );
    // These process-local owners must not later close a newly reused descriptor.
    for file in files {
        let _ = file.into_raw_fd();
    }
    let _ = control.0.into_raw_fd();
    let _ = listener.into_raw_fd();
    for (from, to) in [(channel, 3), (image, 4), (stdin, 5), (server, 6)] {
        ensure!(
            unsafe { libc::dup2(from, to) } == to,
            "capsule descriptor remap"
        );
    }
    for fd in [4, 5, 6] {
        ensure!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "capsule exec descriptor CLOEXEC"
        );
    }
    ensure!(
        unsafe { libc::syscall(libc::SYS_close_range, 7u32, u32::MAX, 0) } == 0,
        "capsule exact descriptor closure"
    );
    Ok((wire::Socket(unsafe { OwnedFd::from_raw_fd(3) }), unsafe {
        TcpListener::from_raw_fd(6)
    }))
}
fn exec(plan: PlanRecord, control: wire::Socket, port: u16) -> Result<()> {
    control.send(
        &Message::Harness {
            nonce: plan.nonce,
            inner_pid: std::process::id(),
        },
        None,
        plan.deadline,
    )?;
    let packet = control.recv::<Message>(plan.deadline)?;
    ensure!(
        packet.credentials.pid == 0 && packet.fd.is_none(),
        "capsule trusted ancestor sender"
    );
    let (selected_arguments, selected_environment) = if plan.prepared.is_some() {
        let Message::Commit(record) = packet.value else {
            bail!("prepared commit missing")
        };
        super::prepared::verify_exec(&plan, port, &record)?.into_parts()
    } else {
        ensure!(
            matches!(packet.value,Message::Release{nonce} if nonce==plan.nonce),
            "capsule exact ancestor release"
        );
        (
            plan.arguments.clone(),
            vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                (
                    "AQ_CAPSULE_ENDPOINT".into(),
                    format!("http://127.0.0.1:{port}"),
                ),
            ],
        )
    };
    let mut arguments = vec![CString::new("aq-static-capsule-peer")?];
    for argument in selected_arguments {
        arguments.push(CString::new(argument)?);
    }
    let environment = selected_environment
        .into_iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut argv = arguments.iter().map(|a| a.as_ptr()).collect::<Vec<_>>();
    argv.push(std::ptr::null());
    let mut env = environment.iter().map(|a| a.as_ptr()).collect::<Vec<_>>();
    env.push(std::ptr::null());
    ensure!(unsafe { libc::dup2(5, 0) } == 0, "capsule exact stdin");
    drop(control);
    ensure!(
        unsafe { libc::close(5) } == 0,
        "capsule stdin source closure"
    );
    std::env::set_current_dir("/workspace")?;
    ensure!(
        ownership::raw_boot_ms() < plan.deadline,
        "capsule authority time lost before exec"
    );
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            4,
            c"".as_ptr(),
            argv.as_ptr(),
            env.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    Err(std::io::Error::last_os_error()).context("capsule static exec failed; no fallback")
}
fn init(plan: PlanRecord, files: Vec<File>, control: wire::Socket) -> Result<()> {
    ensure!(std::process::id() == 1, "capsule init identity");
    super::mounts::enter(&plan, &files)?;
    loopback()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let (control, listener) = remap(files, control, listener)?;
    cap_drop()?;
    filter()?;
    control.send(
        &Message::Init {
            nonce: plan.nonce,
            inner_pid: 1,
        },
        None,
        plan.deadline,
    )?;
    control.send(
        &Message::Listener {
            nonce: plan.nonce,
            port,
        },
        Some(listener.as_raw_fd()),
        plan.deadline,
    )?;
    drop(listener);
    let child = unsafe { libc::fork() };
    ensure!(child >= 0, "capsule harness fork");
    if child == 0 {
        let result = exec(plan, control, port);
        if let Err(error) = result {
            eprintln!("capsule static exec refused: {error:#}");
        }
        unsafe { libc::_exit(112) }
    }
    drop(control);
    unsafe {
        libc::close(4);
        libc::close(5);
    }
    loop {
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(-1, &mut status, 0) };
        if reaped == child {
            unsafe { libc::_exit(0) }
        }
        ensure!(
            reaped > 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR),
            "capsule init wait"
        );
    }
}
pub(super) fn run(
    plan: PlanRecord,
    files: Vec<File>,
    control: wire::Socket,
    guardian: u32,
) -> Result<()> {
    ensure!(
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } == 0
            && unsafe { libc::getppid() } as u32 == guardian,
        "capsule setup parent fence"
    );
    user_namespace()?;
    ensure!(
        unsafe {
            libc::unshare(
                libc::CLONE_NEWNS
                    | libc::CLONE_NEWPID
                    | libc::CLONE_NEWNET
                    | libc::CLONE_NEWIPC
                    | libc::CLONE_NEWUTS,
            )
        } == 0,
        "capsule private namespaces"
    );
    let bootstrap = ownership::pidfd(std::process::id())?;
    let child = unsafe { libc::fork() };
    ensure!(child >= 0, "capsule init fork");
    if child == 0 {
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } == 0
                && ownership::pidfd_is_live(&bootstrap)?,
            "capsule init parent fence"
        );
        drop(bootstrap);
        return init(plan, files, control);
    }
    drop(bootstrap);
    drop(files);
    drop(control);
    let mut status = 0;
    ensure!(
        unsafe { libc::waitpid(child, &mut status, 0) } == child,
        "capsule bootstrap wait"
    );
    unsafe { libc::_exit(0) }
}

pub(super) fn user_namespace() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "capsule rootless helper required"
    );
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    ensure!(
        unsafe { libc::unshare(libc::CLONE_NEWUSER) } == 0,
        "capsule user namespace"
    );
    write("/proc/thread-self/setgroups", "deny\n")?;
    write("/proc/thread-self/uid_map", &format!("{uid} {uid} 1\n"))?;
    write("/proc/thread-self/gid_map", &format!("{gid} {gid} 1\n"))?;
    Ok(())
}
