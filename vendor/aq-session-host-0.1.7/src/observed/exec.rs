//! Positive kernel exec evidence. EOF is only a trigger to inspect, never proof.
use super::*;
use std::{ffi::CString, os::fd::AsRawFd, os::unix::ffi::OsStrExt};

/// Incarnation/liveness only; guardian ancestry was established at pane creation.
/// Both the original server and pane must still exist for terminal exec evidence.
pub(crate) fn live_pane(pane: &PaneIdentity) -> Result<()> {
    for process in [&pane.server, &pane.pane] {
        ensure!(
            ownership::pidfd_is_live(&ownership::verified_pidfd(process)?)?,
            "owned terminal process exited"
        );
    }
    Ok(())
}
pub(crate) fn observed_image(process: &ProcessEvidence, image: &ElfIdentity) -> Result<()> {
    let fd = ownership::verified_pidfd(process)?;
    ensure!(ownership::pidfd_is_live(&fd)?, "exec process not live");
    let metadata = std::fs::metadata(format!("/proc/{}/exe", process.pid))?;
    ensure!(
        metadata.dev() == image.device && metadata.ino() == image.inode,
        "live executable does not match sealed image"
    );
    ensure!(
        ownership::process_evidence(process.pid)? == *process && ownership::pidfd_is_live(&fd)?,
        "exec process changed while observed"
    );
    if matches!(
        image.profile,
        Some(executable::ImageProfile::NamedReadonlyExecStdinV1(_))
    ) {
        let named = File::open(format!("/proc/{}/exe", process.pid))?;
        executable::verify_file(&named, image)?;
    }
    Ok(())
}
/// Called only in the dedicated pane helper, after the guardian release packet.
/// No path lookup, shell fallback or inherited ambient environment.
pub(crate) fn enter(
    file: &File,
    image: &ElfIdentity,
    directory: &Path,
    config: &SealedLaunch,
    guardian: &GuardianIdentity,
    server: &ProcessEvidence,
    startup_stdin: Option<&File>,
) -> Result<()> {
    executable::verify_file(file, image)?;
    let command = &config.command;
    let workspace = &config.workspace;
    command.validate()?;
    let mut args = vec![CString::new(command.executable.as_os_str().as_bytes())?];
    for arg in &command.arguments {
        args.push(CString::new(arg.as_bytes())?);
    }
    if config.observed.as_ref().is_some_and(|s| s.interactive()) {
        // Append from the existing verified context at the actual exec boundary;
        // ordinary command arguments keep their original 4096-byte bounds.
        ensure!(
            command.arguments.last().is_some_and(|a| a == "--"),
            "interactive prompt delimiter absent"
        );
        let bytes = read_bytes(&directory.join("context-0.bin"), 1_048_576)?;
        config
            .rendered_startup
            .as_ref()
            .context("interactive startup descriptor absent")?
            .verify(config.manifest.task()?, &bytes)?;
        validate_interactive_prompt(&bytes)?;
        args.push(CString::new(bytes)?);
    }
    let environment = command_environment(
        command,
        directory,
        config
            .private_input
            .as_ref()
            .is_some_and(private_inputs::PrivateInputRecord::scoped),
    )?;
    let environment: Vec<_> = environment
        .into_iter()
        .map(|(key, value)| CString::new(format!("{key}={value}")))
        .collect::<std::result::Result<_, _>>()?;
    let mut argv: Vec<_> = args.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut envp: Vec<_> = environment.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    std::env::set_current_dir(workspace)?;
    if let Some(stdin) = startup_stdin {
        ensure!(
            unsafe { libc::dup2(stdin.as_raw_fd(), libc::STDIN_FILENO) } >= 0,
            "native exact startup stdin unavailable"
        );
    }
    private_inputs::verify_at_exec(config, directory)?;
    config.validate_permit_at(
        &config.permit,
        &guardian.intent.session,
        &guardian.intent.manifest_digest,
        ownership::raw_boot_ms(),
    )?;
    ensure!(
        ownership::raw_boot_ms() < config.wall_deadline()?
            && !directory.join("stop.request").exists()
            && supervisor::guardian_live(guardian)
            && ownership::pidfd_is_live(&ownership::verified_pidfd(server)?)?,
        "authority lost at native exec boundary"
    );
    let empty = c"";
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            file.as_raw_fd(),
            empty.as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        );
    }
    Err(std::io::Error::last_os_error()).context("sealed FD native exec failed; no fallback")
}
fn command_environment(
    command: &LocalCommand,
    directory: &Path,
    scoped: bool,
) -> Result<std::collections::BTreeMap<String, String>> {
    command.validate()?;
    let mut environment = std::collections::BTreeMap::from([
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
    ]);
    if !scoped {
        environment.insert(
            "AQ_SESSION_DIRECTORY".to_owned(),
            directory
                .to_str()
                .context("session directory UTF-8")?
                .to_owned(),
        );
    }
    environment.extend(command.environment.clone());
    Ok(environment)
}

pub(crate) fn validate_interactive_prompt(bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= 64 * 1024
            && !bytes.contains(&0)
            && std::str::from_utf8(bytes).is_ok(),
        "interactive startup UTF8/64KiB bound"
    );
    Ok(())
}
/// Prepare exact bytes from the host's retained descriptor and seal them before
/// changing FD0. Old small-ELF sessions retain their original PTY stdin behavior.
pub(crate) fn startup_stdin(config: &SealedLaunch, directory: &Path) -> Result<Option<File>> {
    if !config
        .observed
        .as_ref()
        .is_some_and(|s| s.native_exec() && !s.interactive())
    {
        return Ok(None);
    }
    let rendered = config
        .rendered_startup
        .as_ref()
        .context("native startup descriptor missing")?;
    let bytes = read_bytes(&directory.join("context-0.bin"), 1_048_576)?;
    rendered.verify(config.manifest.task()?, &bytes)?;
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::memfd_create(
            c"aq-native-startup".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | libc::MFD_NOEXEC_SEAL,
        )
    };
    ensure!(fd >= 0, "native startup sealed fd unavailable");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(&bytes)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } == 0,
        "native startup immutable seals unavailable"
    );
    use std::io::Seek;
    file.rewind()?;
    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};
    #[test]
    fn interactive_prompt_uses_existing_rendered_utf8_and_bounded_argv() {
        validate_interactive_prompt("Context ›\n".as_bytes()).unwrap();
        validate_interactive_prompt(&vec![b'a'; 64 * 1024]).unwrap();
        for invalid in [
            vec![],
            vec![b'a'; 64 * 1024 + 1],
            vec![0xff],
            vec![b'a', 0, b'b'],
        ] {
            assert!(validate_interactive_prompt(&invalid).is_err());
        }
    }
    #[test]
    fn scoped_child_environment_omits_private_host_record_route_and_keeps_legacy_contract() {
        let command = LocalCommand {
            executable: "/usr/bin/true".into(),
            arguments: vec![],
            environment: std::collections::BTreeMap::from([(
                "CODEX_HOME".into(),
                "/state/private-home".into(),
            )]),
        };
        let scoped =
            command_environment(&command, Path::new("/state/private-launch"), true).unwrap();
        assert_eq!(
            scoped.get("CODEX_HOME").map(String::as_str),
            Some("/state/private-home")
        );
        assert_eq!(scoped.len(), 3);
        assert!(!scoped.contains_key("AQ_SESSION_DIRECTORY"));
        assert!(
            command_environment(&command, Path::new("/state/private-launch"), false)
                .unwrap()
                .contains_key("AQ_SESSION_DIRECTORY")
        );
    }
    #[test]
    fn dead_original_server_refuses_even_while_the_pane_identity_is_live() {
        let mut server = std::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let pane = PaneIdentity {
            server: ownership::process_evidence(server.id()).unwrap(),
            pane: ownership::process_evidence(std::process::id()).unwrap(),
            pane_id: "%0".into(),
            session_name: "aq".into(),
        };
        live_pane(&pane).unwrap();
        server.kill().unwrap();
        server.wait().unwrap();
        assert!(ownership::process_evidence(pane.pane.pid).is_ok());
        assert!(live_pane(&pane).is_err());
    }
    #[test]
    fn eof_without_matching_live_image_is_not_exec_proof() {
        let process = ownership::process_evidence(std::process::id()).unwrap();
        let mut image = ElfIdentity {
            artifact: ArtifactRef {
                id: aq_session_protocol::Text::new("synthetic").unwrap(),
                version: aq_session_protocol::Text::new("1").unwrap(),
                digest: Digest::of(b"not current image"),
            },
            bytes: 128,
            device: 0,
            inode: 1,
            profile: None,
        };
        assert!(observed_image(&process, &image).is_err());
        let metadata = std::fs::metadata("/proc/self/exe").unwrap();
        image.device = metadata.dev();
        image.inode = metadata.ino();
        assert!(observed_image(&process, &image).is_ok());
        let stale = ProcessEvidence {
            pid: process.pid,
            start_ticks: process.start_ticks + 1,
        };
        assert!(observed_image(&stale, &image).is_err());
        // A closed CLOEXEC-shaped channel alone provides no executable evidence.
        let mut pair = [0; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let a = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let b = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        drop(b);
        let mut byte = 0u8;
        assert_eq!(
            unsafe { libc::read(a.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) },
            0
        );
        image.inode += 1;
        assert!(observed_image(&process, &image).is_err());
    }
}
