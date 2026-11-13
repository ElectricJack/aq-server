//! One internal task on a logged-in Codex: `codex exec` in a read-only
//! sandbox and an empty directory, the prompt on stdin, the answer from
//! `--output-last-message`. The Codex CLI has no switch that removes its
//! shell tool, so this is what "tools off" means for Codex.
use super::Harness;
use crate::subscription::{
    Identity, copy_back, exec_arguments, exec_finish, exec_overrides, native_command, read_reply,
    redact_login, stage_login, verify_native,
};
use aq_internal_work::{Claimed, Reason, Usage, validate};
use serde_json::Value;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;

/// Longest failure `detail`, in bytes; it is stored in the task row.
const DETAIL_BYTES: usize = 500;
/// The JSON event stream read from stdout; the rest is drained unread.
const EVENTS_BYTES: u64 = 8 * 1024 * 1024;

type Failure = (Reason, String);

fn failure(reason: Reason, detail: &str) -> Failure {
    (reason, detail.into())
}

/// Runs `claimed` once on the Codex login of `h`, killing it after `budget`.
/// The per-run home under `homes` and working directory under `scratch` are
/// removed afterwards, whatever the outcome.
pub(crate) async fn codex(
    h: &Harness,
    homes: &Path,
    scratch: &Path,
    claimed: &Claimed,
    identity: Identity,
    budget: Duration,
) -> Result<(String, Option<Usage>), Failure> {
    for parent in [homes, scratch] {
        private_parent(parent, identity)
            .map_err(|_| failure(Reason::ProviderError, "internal work directories rejected"))?;
    }
    let executable = verify_native(&h.native, &h.native_digest, identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "native executable refused"))?;
    let name = uuid::Uuid::new_v4().to_string();
    let (home, cwd) = (homes.join(&name), scratch.join(&name));
    let result = attempt(h, &executable, &home, &cwd, claimed, identity, budget).await;
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&cwd);
    result
}

async fn attempt(
    h: &Harness,
    executable: &std::fs::File,
    home: &Path,
    cwd: &Path,
    claimed: &Claimed,
    identity: Identity,
    budget: Duration,
) -> Result<(String, Option<Usage>), Failure> {
    let setup = || failure(Reason::ProviderError, "native run could not be prepared");
    for dir in [home, cwd] {
        std::fs::create_dir(dir).map_err(|_| setup())?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| setup())?;
    }
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        std::os::unix::fs::chown(cwd, id, id).map_err(|_| setup())?;
    }
    let auth = stage_login(&h.secret_file, home, identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "login file unusable"))?;
    let auth_path = home.join("auth.json");
    let output = home.join("last-message.txt");
    let candidate = &claimed.candidate;
    let args = exec_arguments("read-only", &candidate.model, cwd, &output).map_err(|_| setup())?;
    let args = exec_finish(args, exec_overrides(&candidate.reasoning));
    let mut command = native_command(executable, args, home, cwd, identity);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let ran = execute(command, validate::prompt(&claimed.request), budget).await;
    let refreshed = match copy_back(&h.secret_file, &auth_path, &auth, identity) {
        Ok(refreshed) => refreshed,
        Err(_) => {
            eprintln!("internal work: login refresh was not copied back");
            None
        }
    };
    let _ = std::fs::remove_file(&auth_path);
    let current = crate::config::protected_read(&h.secret_file, 65536)
        .ok()
        .map(Zeroizing::new);
    let documents: Vec<&[u8]> = [Some(&auth), refreshed.as_ref(), current.as_ref()]
        .into_iter()
        .flatten()
        .map(|doc| doc.as_slice())
        .collect();
    // Without a redaction nothing of the native's text leaves this function.
    let redact = |text: String| redact_login(text, &documents).ok();
    match ran {
        Err(_) => Err(failure(Reason::ProviderError, "native run could not start")),
        Ok(Exit::TimedOut) => Err(failure(
            Reason::DeadlineExceeded,
            "native did not finish within the candidate budget",
        )),
        Ok(Exit::Finished(status, stdout)) => {
            let events = Events::parse(&stdout);
            if !status.success() {
                let reason = if events.unavailable {
                    Reason::ProviderUnavailable
                } else {
                    Reason::ProviderError
                };
                let detail = match events.last_error {
                    Some(message) => redact(message)
                        .map(|text| clip(text, DETAIL_BYTES))
                        .unwrap_or_else(|| "native failed; its message was withheld".into()),
                    None => format!("native exited with {status}"),
                };
                return Err((reason, detail));
            }
            let reply = read_reply(&output, identity.run_as)
                .map_err(|_| failure(Reason::ProviderError, "native final reply missing"))?;
            let reply = redact(reply).ok_or_else(|| {
                failure(Reason::ProviderError, "native reply could not be redacted")
            })?;
            Ok((reply, events.usage))
        }
    }
}

/// `dir` is a canonical directory owned by the executable owner and not
/// writable by group or others, so nobody else can plant a run directory.
fn private_parent(dir: &Path, identity: Identity) -> anyhow::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    anyhow::ensure!(
        meta.is_dir()
            && meta.uid() == identity.executable_owner
            && meta.mode() & 0o022 == 0
            && dir.canonicalize()? == dir,
        "internal work directory ownership"
    );
    Ok(())
}

enum Exit {
    Finished(std::process::ExitStatus, Vec<u8>),
    TimedOut,
}

/// Spawns `command`, writes `prompt` to its stdin, reads its stdout and waits
/// for it, all within `budget`. At the budget the child's whole session (it
/// leads one) is killed and reaped.
async fn execute(
    command: std::process::Command,
    prompt: String,
    budget: Duration,
) -> std::io::Result<Exit> {
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let run = async {
        let write = async move {
            if let Some(mut stdin) = stdin {
                // A child that exits without reading is judged by its status.
                let _ = stdin.write_all(prompt.as_bytes()).await;
            }
        };
        let read = async {
            let mut events = Vec::new();
            if let Some(out) = stdout.as_mut() {
                let _ = (&mut *out)
                    .take(EVENTS_BYTES)
                    .read_to_end(&mut events)
                    .await;
                let _ = tokio::io::copy(out, &mut tokio::io::sink()).await;
            }
            events
        };
        let ((), events, status) = tokio::join!(write, read, child.wait());
        status.map(|status| Exit::Finished(status, events))
    };
    match tokio::time::timeout(budget, run).await {
        Ok(finished) => finished,
        Err(_) => {
            if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
            let _ = child.wait().await;
            Ok(Exit::TimedOut)
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
            let Ok(event) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let message = match event["type"].as_str() {
                Some("turn.completed") => {
                    let usage = &event["usage"];
                    if let (Some(input), Some(output)) = (
                        usage["input_tokens"].as_u64(),
                        usage["output_tokens"].as_u64(),
                    ) {
                        events.usage = Some(Usage {
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
                events.unavailable |= unavailable(message);
                events.last_error = Some(message.to_owned());
            }
        }
        events
    }
}

/// An authentication or usage-limit failure: the login cannot serve now.
fn unavailable(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "usage limit",
        "usage_limit",
        "rate limit",
        "quota",
        "unauthorized",
        "401",
        "authenticat",
        "log in",
        "login",
        "sign in",
        "token expired",
        "token_expired",
        "refresh token",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// `text` cut to at most `limit` bytes at a character boundary.
fn clip(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
