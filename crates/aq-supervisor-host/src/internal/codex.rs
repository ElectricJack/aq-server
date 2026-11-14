//! One internal task on a logged-in Codex: `codex exec` in a read-only
//! sandbox and an empty directory, the prompt on stdin, the answer from
//! `--output-last-message`. The Codex CLI has no switch that removes its
//! shell tool, so this is what "tools off" means for Codex. Agent work runs
//! in a workspace-write sandbox without network instead, in the workspace
//! holding its files, with its event stream kept as the transcript and its
//! answer read from the outcome file.
use super::{
    DETAIL_BYTES, Failure, Harness, Place, Run, agent, clip, execute, execute_into, failure,
    in_run_dirs, scrub, unavailable,
};
use crate::subscription::{
    Identity, copy_back, exec_arguments, exec_finish, exec_overrides, native_command, read_reply,
    redact_login, service_tier, stage_login,
};
use aq_internal_work::{Reason, Usage, validate};
use serde_json::Value;
use std::{path::Path, process::ExitStatus};
use zeroize::Zeroizing;

/// The failure of a Codex agent candidate on a host whose sandbox fails its probe.
pub(crate) const SANDBOX_CANNOT_START: &str = "codex sandbox cannot start on this host";

/// Runs `run`'s task once on the Codex login of `h` in its session.
/// The per-run home under `homes` and working directory under `scratch` are
/// removed afterwards, whatever the outcome.
pub(crate) async fn codex(
    h: &Harness,
    homes: &Path,
    scratch: &Path,
    run: Run<'_>,
) -> Result<(String, Option<Usage>), Failure> {
    in_run_dirs(h, homes, scratch, run, async |executable, place| {
        attempt(h, executable, place, run).await
    })
    .await
}

/// The `--config` overrides of agent work's sandbox: no network, and `/tmp`
/// not writable (`exclude_slash_tmp`; accepted by codex-cli 0.167.1 under
/// `--strict-config`, which refuses an unknown key of the same table). Its
/// shell's `PATH` starts with the shims and its `TMPDIR` is the workspace's.
fn agent_overrides(place: &Place) -> [String; 4] {
    let toml = |text: String| Value::String(text).to_string();
    [
        "sandbox_workspace_write.network_access=false".into(),
        "sandbox_workspace_write.exclude_slash_tmp=true".into(),
        format!(
            "shell_environment_policy.set.PATH={}",
            toml(place.path(true))
        ),
        format!(
            "shell_environment_policy.set.TMPDIR={}",
            toml(place.tmp().display().to_string())
        ),
    ]
}

async fn attempt(
    h: &Harness,
    executable: &std::fs::File,
    place: &Place,
    run: Run<'_>,
) -> Result<(String, Option<Usage>), Failure> {
    let (home, cwd) = (place.home.as_path(), place.cwd.as_path());
    let Run {
        claimed,
        identity,
        session,
        ..
    } = run;
    let setup = || failure(Reason::ProviderError, "native run could not be prepared");
    if claimed.request.agent.is_some() && !h.sandbox_starts(executable, place, identity).await {
        // The next candidate takes over at once.
        return Err(failure(Reason::ProviderUnavailable, SANDBOX_CANNOT_START));
    }
    let auth = stage_login(&h.secret_file, home, identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "login file unusable"))?;
    let auth_path = home.join("auth.json");
    // From here on the login is copied back however this ends, a panic
    // included, and before the run's directories go.
    let copy_back = CopyBack {
        secret_file: &h.secret_file,
        auth_path: &auth_path,
        original: &auth,
        identity,
        done: false,
    };
    let output = home.join("last-message.txt");
    let candidate = &claimed.candidate;
    let agent = claimed.request.agent.is_some();
    let sandbox = if agent {
        "workspace-write"
    } else {
        "read-only"
    };
    let args = exec_arguments(sandbox, &candidate.model, cwd, &output).map_err(|_| setup())?;
    // The model choice's effort; without one Codex keeps its default.
    let mut overrides = exec_overrides(candidate.reasoning.as_deref());
    let tier = service_tier(&candidate.options)
        .map_err(|_| failure(Reason::ProviderError, "unsupported service tier"))?;
    overrides.extend(tier);
    if agent {
        // In place of the fixed shell `PATH`.
        overrides.retain(|o| !o.starts_with("shell_environment_policy.set.PATH="));
        overrides.extend(agent_overrides(place));
    }
    let args = exec_finish(args, overrides);
    let mut command = native_command(executable, args, home, cwd, identity);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if agent {
        command
            .env("PATH", place.path(true))
            .env("TMPDIR", place.tmp());
    }
    let (mut events, mut transcript) = (Events::default(), agent::Transcript::default());
    let ran = if agent {
        let prompt = validate::agent_prompt(&claimed.request);
        let mut each = |line: &[u8]| {
            if let Ok(event) = serde_json::from_slice::<Value>(line) {
                events.line(&event);
                transcript.codex(&event);
            }
        };
        let mut lines = agent::Lines::default();
        let ran = execute_into(command, prompt, session, &mut |chunk| {
            lines.feed(chunk, &mut each)
        })
        .await;
        lines.end(&mut each);
        ran.map(|status| (status, Vec::new()))
    } else {
        execute(command, validate::prompt(&claimed.request), session).await
    };
    #[cfg(feature = "test-support")]
    {
        use std::sync::atomic::Ordering::SeqCst;
        if h.panics
            .fetch_update(SeqCst, SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            panic!("internal work test panic after the native ran");
        }
    }
    let refreshed = copy_back.copy();
    let current = crate::config::protected_read(&h.secret_file, 65536)
        .ok()
        .map(Zeroizing::new);
    let documents: Vec<&[u8]> = [Some(&auth), refreshed.as_ref(), current.as_ref()]
        .into_iter()
        .flatten()
        .map(|doc| doc.as_slice())
        .collect();
    // Without a redaction nothing of the native's text leaves this function;
    // it also takes the secrets of every other login on this host.
    let secrets = run.secrets();
    let redact = |text: String| {
        redact_login(text, &documents)
            .ok()
            .map(|t| scrub(t, &secrets))
    };
    if agent {
        run.keep(transcript.finish(|text| redact(text.to_owned())));
    }
    let (status, stdout) =
        ran.map_err(|_| failure(Reason::ProviderError, "native run could not start"))?;
    if !agent {
        events = Events::parse(&stdout);
    }
    let failed = (!status.success()).then(|| events.failure(status, redact));
    if agent {
        // The harness's own failure counts only when there is no outcome.
        let limit = claimed.request.max_output_bytes;
        let reply = agent::conclude(cwd, limit, failed, redact)?;
        return Ok((reply, events.usage));
    }
    if let Some(failed) = failed {
        return Err(failed);
    }
    let reply = read_reply(&output, identity.run_as)
        .map_err(|_| failure(Reason::ProviderError, "native final reply missing"))?;
    let reply = redact(reply)
        .ok_or_else(|| failure(Reason::ProviderError, "native reply could not be redacted"))?;
    Ok((reply, events.usage))
}

/// Copies the run's login back to the login file ([`copy_back`]) and removes
/// the run's copy: once through [`CopyBack::copy`], or on drop if that never
/// ran (an early return or a panic).
struct CopyBack<'a> {
    secret_file: &'a Path,
    auth_path: &'a Path,
    original: &'a [u8],
    identity: Identity,
    done: bool,
}

impl CopyBack<'_> {
    /// The refreshed login the native left, if any.
    fn copy(mut self) -> Option<Zeroizing<Vec<u8>>> {
        self.run()
    }

    fn run(&mut self) -> Option<Zeroizing<Vec<u8>>> {
        self.done = true;
        let copied = copy_back(
            self.secret_file,
            self.auth_path,
            self.original,
            self.identity,
        );
        let _ = std::fs::remove_file(self.auth_path);
        copied.unwrap_or_else(|_| {
            eprintln!("internal work: login refresh was not copied back");
            None
        })
    }
}

impl Drop for CopyBack<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.run();
        }
    }
}

/// What the `--json` event stream says: the last turn's usage, the last
/// error message, and whether any error names a login or usage-limit problem.
#[derive(Default)]
struct Events {
    usage: Option<Usage>,
    last_error: Option<String>,
    unavailable: bool,
}

impl Events {
    fn parse(stdout: &[u8]) -> Events {
        let mut events = Events::default();
        for line in stdout.split(|b| *b == b'\n') {
            if let Ok(event) = serde_json::from_slice::<Value>(line) {
                events.line(&event);
            }
        }
        events
    }

    /// The failure of a run that exited with `status`: a login or usage-limit
    /// problem is unavailable, anything else an error. The last error message
    /// leaves only through `redact`.
    fn failure(&self, status: ExitStatus, redact: impl Fn(String) -> Option<String>) -> Failure {
        let reason = match self.unavailable {
            true => Reason::ProviderUnavailable,
            false => Reason::ProviderError,
        };
        let detail = match &self.last_error {
            Some(message) => redact(message.clone())
                .map(|text| clip(text, DETAIL_BYTES))
                .unwrap_or_else(|| "native failed; its message was withheld".into()),
            None => format!("native exited with {status}"),
        };
        (reason, detail)
    }

    fn line(&mut self, event: &Value) {
        let message = match event["type"].as_str() {
            Some("turn.completed") => {
                let usage = &event["usage"];
                if let (Some(input), Some(output)) = (
                    usage["input_tokens"].as_u64(),
                    usage["output_tokens"].as_u64(),
                ) {
                    self.usage = Some(Usage {
                        input_tokens: input,
                        output_tokens: output,
                    });
                }
                None
            }
            Some("error") => event["message"].as_str(),
            Some("turn.failed") => event["error"]["message"].as_str(),
            Some("item.completed") if event["item"]["type"] == "error" => {
                event["item"]["message"].as_str()
            }
            _ => None,
        };
        if let Some(message) = message {
            self.unavailable |= unavailable(message);
            self.last_error = Some(message.to_owned());
        }
    }
}
