//! One internal task on a logged-in Claude Code: `claude -p` with every
//! built-in tool removed (`--tools ""`), in an empty directory, the prompt on
//! stdin, the answer from the `--output-format json` result object. The setup
//! token reaches the native only in its environment and is never written to
//! disk, so nothing is copied back.
use super::{
    DETAIL_BYTES, Exit, Failure, Harness, clip, execute, failure, in_run_dirs, unavailable,
};
use crate::{
    config::protected_read,
    subscription::{Identity, native_command},
};
use aq_intelligence_config::material::normalize_claude_token;
use aq_internal_work::{Claimed, Reason, Usage, validate};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
    process::ExitStatus,
    time::Duration,
};
use zeroize::Zeroizing;

/// Every run's environment besides `HOME`, `PATH`, `CLAUDE_CONFIG_DIR` and
/// the token.
const ENVIRONMENT: [(&str, &str); 5] = [
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
];

/// The efforts the worker's interactive Claude launch passes as `--effort`
/// (it also accepts `default`, passing nothing, and `off`); the same values
/// `claude --help` lists for `--effort`.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Runs `claimed` once on the Claude login of `h`, killing it after `budget`.
/// The per-run home under `homes` and working directory under `scratch` are
/// removed afterwards, whatever the outcome.
pub(crate) async fn claude(
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
    let token = protected_read(&h.secret_file, 65536)
        .map(Zeroizing::new)
        .and_then(normalize_claude_token)
        .map_err(|_| failure(Reason::ProviderUnavailable, "login file unusable"))?;
    // Printable ASCII, as `normalize_claude_token` checked.
    let secret = std::str::from_utf8(&token).map_err(|_| setup())?;
    let settings = home.join("settings.json");
    write_settings(&settings, home, identity).map_err(|_| setup())?;
    let candidate = &claimed.candidate;
    let effort = candidate.reasoning.as_deref();
    let args = arguments(&candidate.model, effort, &settings).ok_or_else(setup)?;
    let mut command = native_command(executable, args, home, cwd, identity);
    if effort == Some("off") {
        command.env("MAX_THINKING_TOKENS", "0");
    }
    // `native_command` sets the Codex home; Claude's is `CLAUDE_CONFIG_DIR`.
    command
        .env_remove("CODEX_HOME")
        .env("CLAUDE_CONFIG_DIR", home)
        .envs(ENVIRONMENT)
        .env(
            "CLAUDE_CODE_OAUTH_TOKEN",
            std::ffi::OsStr::from_bytes(&token),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let ran = execute(command, validate::prompt(&claimed.request), budget).await;
    let redact = |text: &str| text.replace(secret, "[credential redacted]");
    match ran {
        Err(_) => Err(failure(Reason::ProviderError, "native run could not start")),
        Ok(Exit::TimedOut) => Err(failure(
            Reason::DeadlineExceeded,
            "native did not finish within the candidate budget",
        )),
        Ok(Exit::Finished(status, stdout)) => outcome(status, &stdout, redact),
    }
}

/// The run's arguments: print mode, one JSON result, the prompt as text on
/// stdin, only the private settings, no MCP servers, no session saved, no
/// slash commands and no tools. The model choice's effort follows as
/// `--effort` when it is one of [`EFFORTS`], as the worker's interactive
/// Claude launch passes it; `off` is `MAX_THINKING_TOKENS=0` instead, and
/// `default`, no effort or any other value keeps the model's own.
fn arguments(model: &str, effort: Option<&str>, settings: &Path) -> Option<Vec<String>> {
    let settings = settings.to_str()?;
    let args = [
        "-p",
        "--output-format",
        "json",
        "--input-format",
        "text",
        "--model",
        model,
        "--setting-sources",
        "user",
        "--settings",
        settings,
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
        "--tools",
        "",
    ];
    let mut args = args.map(String::from).to_vec();
    if let Some(effort) = effort.filter(|e| EFFORTS.contains(e)) {
        args.extend(["--effort".into(), effort.into()]);
    }
    Some(args)
}

/// `<home>/settings.json`: no hooks and no project MCP servers. It and the
/// home belong to the native's user.
fn write_settings(path: &Path, home: &Path, identity: Identity) -> std::io::Result<()> {
    let body = json!({"disableAllHooks": true, "enableAllProjectMcpServers": false});
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(body.to_string().as_bytes())?;
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        std::os::unix::fs::chown(path, id, id)?;
        std::os::unix::fs::chown(home, id, id)?;
    }
    Ok(())
}

/// The answer and usage from a finished run's stdout, or its failure. The
/// stdout must be one result object; `is_error` other than `false` or a
/// non-zero exit fails the run. Text leaves only through `redact`.
fn outcome(
    status: ExitStatus,
    stdout: &[u8],
    redact: impl Fn(&str) -> String,
) -> Result<(String, Option<Usage>), Failure> {
    let object = serde_json::from_slice::<Value>(stdout)
        .ok()
        .filter(|object| object["type"] == "result");
    let Some(object) = object else {
        let text = String::from_utf8_lossy(stdout);
        let detail = if text.trim().is_empty() {
            format!("native printed no result and exited with {status}")
        } else {
            let detail = format!("native printed no JSON result: {}", redact(&text));
            clip(detail, DETAIL_BYTES)
        };
        return Err((Reason::ProviderError, detail));
    };
    let text = object["result"].as_str().filter(|t| !t.trim().is_empty());
    if !status.success() || object["is_error"] != false {
        let reason = if text.is_some_and(unavailable) {
            Reason::ProviderUnavailable
        } else {
            Reason::ProviderError
        };
        let detail = match text {
            Some(text) => clip(redact(text), DETAIL_BYTES),
            None => format!("native reported an error and exited with {status}"),
        };
        return Err((reason, detail));
    }
    let reply = text.ok_or_else(|| failure(Reason::ProviderError, "native final reply missing"))?;
    let usage = &object["usage"];
    let usage = match (
        usage["input_tokens"].as_u64(),
        usage["output_tokens"].as_u64(),
    ) {
        (Some(input_tokens), Some(output_tokens)) => Some(Usage {
            input_tokens,
            output_tokens,
        }),
        _ => None,
    };
    Ok((redact(reply), usage))
}
