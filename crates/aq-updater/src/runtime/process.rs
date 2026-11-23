//! Bounded Docker subprocess output, timeouts and command exit status.

use super::{DockerCli, MAX_CAPTURE, Ran, RuntimeResult, STDERR_KEEP, failed};
use std::ffi::OsString;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

pub(super) struct Invocation<'a> {
    name: &'a str,
    argv: Vec<OsString>,
    timeout: Duration,
    stdin: Stdio,
}

impl<'a> Invocation<'a> {
    pub(super) fn new(name: &'a str, argv: Vec<OsString>, timeout: Duration) -> Self {
        Self {
            name,
            argv,
            timeout,
            stdin: Stdio::null(),
        }
    }

    pub(super) fn with_stdin(mut self, stdin: Stdio) -> Self {
        self.stdin = stdin;
        self
    }
}

impl DockerCli {
    fn spawn(&self, name: &str, argv: &[OsString], stdin: Stdio) -> RuntimeResult<Child> {
        Command::new(&self.docker)
            .args(argv)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // Ctrl-C and hangup reach aq-updater, which finishes the step
            // and records its outcome. SIGKILL leaves recovery to the journal.
            .process_group(0)
            .spawn()
            .map_err(|e| failed(format!("{name}: cannot run {}: {e}", self.docker.display())))
    }

    /// Run the command with an optional stdout cap. The child is killed on
    /// timeout or if the future is dropped.
    pub(super) async fn exec_into<W>(
        &self,
        invocation: Invocation<'_>,
        sink: &mut W,
        cap: Option<u64>,
    ) -> RuntimeResult<(ExitStatus, Vec<u8>, u64)>
    where
        W: AsyncWrite + Unpin + Send,
    {
        let Invocation {
            name,
            argv,
            timeout,
            stdin,
        } = invocation;
        let mut child = self.spawn(name, &argv, stdin)?;
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let err_task = tokio::spawn(stderr_bytes(stderr));
        let run = async {
            let copied = copy_output(stdout, sink, cap, name).await;
            if copied.is_err() {
                let _ = child.start_kill();
            }
            let status = child
                .wait()
                .await
                .map_err(|e| failed(format!("{name}: cannot wait for docker: {e}")));
            (copied, status)
        };
        let (copied, status) = tokio::time::timeout(timeout, run)
            .await
            .map_err(|_| failed(format!("{name} timed out after {} s", timeout.as_secs())))?;
        let stderr = err_task.await.unwrap_or_default();
        let n = copied?;
        Ok((status?, stderr, n))
    }

    pub(super) async fn exec(&self, invocation: Invocation<'_>) -> RuntimeResult<Ran> {
        let compose = invocation.argv.first().is_some_and(|a| a == "compose");
        let mut stdout = Vec::new();
        let (status, stderr, _) = self
            .exec_into(invocation, &mut stdout, Some(MAX_CAPTURE))
            .await?;
        Ok(Ran {
            compose,
            success: status.success(),
            status,
            stdout,
            stderr,
        })
    }

    pub(super) async fn run(
        &self,
        name: &str,
        argv: Vec<OsString>,
        timeout: Duration,
    ) -> RuntimeResult<Vec<u8>> {
        self.exec(Invocation::new(name, argv, timeout))
            .await?
            .ok(name)
    }
}

async fn stderr_bytes(mut stderr: ChildStderr) -> Vec<u8> {
    let mut keep = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        keep.extend_from_slice(&buf[..n]);
        if keep.len() > STDERR_KEEP {
            keep.drain(..keep.len() - STDERR_KEEP);
        }
    }
    keep
}

async fn copy_output<W>(
    mut stdout: ChildStdout,
    sink: &mut W,
    cap: Option<u64>,
    name: &str,
) -> RuntimeResult<u64>
where
    W: AsyncWrite + Unpin + Send,
{
    let copied = match cap {
        Some(limit) => tokio::io::copy(&mut stdout.take(limit + 1), sink).await,
        None => tokio::io::copy(&mut stdout, sink).await,
    }
    .map_err(|e| failed(format!("{name}: cannot read its output: {e}")))?;
    if let Some(limit) = cap
        && copied > limit
    {
        return Err(failed(format!("{name}: output larger than {limit} bytes")));
    }
    Ok(copied)
}
