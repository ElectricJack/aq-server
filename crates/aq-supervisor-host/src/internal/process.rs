use super::*;

/// How long a native's stdout may stay open after the native has exited and
/// its session was killed: a grandchild that left the session can hold the
/// pipe no longer than this.
const DRAIN_MS: u64 = 500;

/// The process session a native leads, so that the runner can stop the run
/// from outside it. No native starts once the session was stopped or its
/// stop time has passed.
pub(super) struct Session {
    state: std::sync::Mutex<SessionState>,
    stop_at: Instant,
    /// The agent's uid, whose every process [`Session::kill`] kills too, so
    /// a process that left the session cannot outlive the run.
    sweep: Option<u32>,
}

enum SessionState {
    NotStarted,
    Running(i32),
    Stopped,
}

impl Session {
    pub(super) fn new(stop_at: Instant, sweep: Option<u32>) -> Self {
        Session {
            state: std::sync::Mutex::new(SessionState::NotStarted),
            stop_at,
            sweep,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spawn(
        &self,
        command: &mut tokio::process::Command,
    ) -> std::io::Result<tokio::process::Child> {
        let mut state = self.state();
        if matches!(*state, SessionState::Stopped) || Instant::now() >= self.stop_at {
            *state = SessionState::Stopped;
            return Err(std::io::Error::other("the run was stopped"));
        }
        let child = command.spawn()?;
        let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
        *state = pid.map_or(SessionState::Stopped, SessionState::Running);
        Ok(child)
    }

    /// Asks every process in the session to end (SIGTERM), and keeps a
    /// native that has not started from starting.
    pub(super) fn terminate(&self) {
        let mut state = self.state();
        match *state {
            SessionState::Running(pid) => unsafe {
                libc::kill(-pid, libc::SIGTERM);
            },
            _ => *state = SessionState::Stopped,
        }
    }

    /// Kills every process left in the session (SIGKILL), and for agent work
    /// every process of the agent's uid, and keeps a native that has not
    /// started from starting.
    pub(super) fn kill(&self) {
        let mut state = self.state();
        if let SessionState::Running(pid) = *state {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        *state = SessionState::Stopped;
        if let Some(uid) = self.sweep {
            sweep_uid(uid);
        }
    }
}

/// Kills every process of `uid` (SIGKILL), from a child that has taken that
/// uid, so nothing else can be reached; then removes what `uid` left at the
/// top of `/tmp`. Only agent work's own uid is swept: no other run uses it,
/// and at most one agent runs on a host.
pub(super) fn sweep_uid(uid: u32) {
    unsafe {
        let pid = libc::fork();
        if pid == 0 {
            sweep_child(uid);
        }
        if pid > 0 {
            let mut status = 0;
            while libc::waitpid(pid, &mut status, 0) < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
            }
        }
    }
    clean_agent_temp(uid);
}

fn clean_agent_temp(uid: u32) {
    for entry in std::fs::read_dir("/tmp").into_iter().flatten().flatten() {
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.uid() == uid)
            && std::fs::remove_dir_all(&path).is_err()
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Called only in the child after the fork. No allocator, locks or destructors.
fn sweep_child(uid: u32) -> ! {
    unsafe {
        // Only async-signal-safe calls in the child of a threaded process.
        let switched = libc::setgroups(0, std::ptr::null()) == 0
            && libc::setgid(uid) == 0
            && libc::setuid(uid) == 0;
        if switched {
            // Again while something was signalled, so a process forked
            // while the first pass ran dies too; zombies count as
            // signalled, hence the bound.
            let pause = libc::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            for _ in 0..20 {
                if libc::kill(-1, libc::SIGKILL) != 0 {
                    break;
                }
                libc::nanosleep(&pause, std::ptr::null_mut());
            }
        }
        libc::_exit(0);
    }
}

/// A run dropped before its end (an aborted task, a panic) kills the whole
/// session, not only the leader `kill_on_drop` reaches.
impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Spawns `command` in `session`, writes `prompt` to its stdin, reads its
/// stdout and waits for it to exit. Once it has exited, whatever is left of
/// its session is killed and stdout read for at most [`DRAIN_MS`] more, so a
/// grandchild holding the pipe cannot hold up the result. [`Session::terminate`]
/// and [`Session::kill`] end the run the same way.
pub(super) async fn execute(
    command: std::process::Command,
    prompt: String,
    session: &Session,
) -> std::io::Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut bytes = Vec::new();
    let status = execute_into(command, prompt, session, &mut |chunk| {
        // Past the limit the rest is drained unread.
        let room = STDOUT_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
    })
    .await?;
    Ok((status, bytes))
}

/// [`execute`], handing each piece of stdout to `out` as it arrives instead
/// of collecting it, so an agent's long event stream is read whole.
pub(super) async fn execute_into(
    command: std::process::Command,
    prompt: String,
    session: &Session,
    out: &mut (dyn FnMut(&[u8]) + Send),
) -> std::io::Result<std::process::ExitStatus> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = session.spawn(&mut command)?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let write = write_prompt(stdin, prompt);
    tokio::pin!(write);
    let mut chunk = vec![0u8; 64 * 1024];
    let (mut written, mut status, mut drain_until) = (false, None, None);
    while status.is_none() || stdout.is_some() {
        let drained =
            tokio::time::sleep_until(drain_until.unwrap_or_else(tokio::time::Instant::now));
        tokio::select! {
            () = &mut write, if !written => written = true,
            read = read_some(&mut stdout, &mut chunk), if stdout.is_some() => match read {
                Ok(0) | Err(_) => stdout = None,
                Ok(n) => out(&chunk[..n]),
            },
            exited = child.wait(), if status.is_none() => {
                session.kill();
                status = Some(exited?);
                let drain = Duration::from_millis(DRAIN_MS);
                drain_until = Some(tokio::time::Instant::now() + drain);
            },
            () = drained, if drain_until.is_some() => break,
        }
    }
    Ok(status.expect("the loop ends only after the native has exited"))
}

async fn write_prompt(stdin: Option<tokio::process::ChildStdin>, prompt: String) {
    if let Some(mut stdin) = stdin {
        // A child that exits without reading is judged by its status.
        let _ = stdin.write_all(prompt.as_bytes()).await;
    }
}

/// The next bytes of `out`; none once it is closed.
async fn read_some(
    out: &mut Option<tokio::process::ChildStdout>,
    chunk: &mut [u8],
) -> std::io::Result<usize> {
    match out {
        Some(out) => out.read(chunk).await,
        None => Ok(0),
    }
}
