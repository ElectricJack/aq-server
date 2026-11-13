//! One internal task on a logged-in Codex: `codex exec` in a read-only
//! sandbox and an empty directory, the prompt on stdin, the answer from
//! `--output-last-message`. The Codex CLI has no switch that removes its
//! shell tool, so this is what "tools off" means for Codex.
use super::{
    DETAIL_BYTES, Exit, Failure, Harness, clip, execute, failure, in_run_dirs, unavailable,
};
use crate::subscription::{
    Identity, copy_back, exec_arguments, exec_finish, exec_overrides, native_command, read_reply,
    redact_login, service_tier, stage_login,
};
use aq_internal_work::{Claimed, Reason, Usage, validate};
use serde_json::Value;
use std::{path::Path, time::Duration};
use zeroize::Zeroizing;

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
    in_run_dirs(
        h,
        homes,
        scratch,
        identity,
        async |executable, home, cwd| {
            attempt(h, executable, home, cwd, claimed, identity, budget).await
        },
    )
    .await
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
    let auth = stage_login(&h.secret_file, home, identity)
        .map_err(|_| failure(Reason::ProviderUnavailable, "login file unusable"))?;
    let auth_path = home.join("auth.json");
    let output = home.join("last-message.txt");
    let candidate = &claimed.candidate;
    let args = exec_arguments("read-only", &candidate.model, cwd, &output).map_err(|_| setup())?;
    // The model choice's effort; `default` leaves the model's own.
    let effort = Some(candidate.reasoning.as_str()).filter(|e| *e != "default");
    let mut overrides = exec_overrides(effort);
    let tier = service_tier(&candidate.options)
        .map_err(|_| failure(Reason::ProviderError, "unsupported service tier"))?;
    overrides.extend(tier);
    let args = exec_finish(args, overrides);
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
