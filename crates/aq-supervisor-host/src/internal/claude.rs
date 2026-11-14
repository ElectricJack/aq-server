//! One internal task on a logged-in Claude Code: `claude -p` with every
//! built-in tool removed (`--tools ""`), in an empty directory, the prompt on
//! stdin, the answer from the `--output-format json` result object. Agent
//! work instead gets the file tools and a shell limited to
//! [`agent::SHELL_EXECUTABLES`], confined to its workspace, with its event
//! stream kept as the transcript and its answer read from the outcome file.
//! The setup token reaches the native only in its environment and is never
//! written to disk, so nothing is copied back.
use super::{
    DETAIL_BYTES, Failure, Harness, Place, Run, agent, clip, execute, execute_into, failure,
    in_run_dirs, scrub, unavailable,
};
use crate::{
    config::protected_read,
    subscription::{Identity, native_command},
};
use aq_intelligence_config::material::normalize_claude_token;
use aq_internal_work::{Reason, Usage, validate};
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
    process::ExitStatus,
};
use zeroize::Zeroizing;

/// Every run's environment besides `HOME`, `PATH`, `CLAUDE_CONFIG_DIR` and
/// the token.
/// How much of what an agent harness printed outside its event stream is
/// kept for a failure's detail.
const STRAY_BYTES: usize = 600;

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

/// Runs `run`'s task once on the Claude login of `h` in its session.
/// The per-run home under `homes` and working directory under `scratch` are
/// removed afterwards, whatever the outcome.
pub(crate) async fn claude(
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
    let token = protected_read(&h.secret_file, 65536)
        .map(Zeroizing::new)
        .and_then(normalize_claude_token)
        .map_err(|_| failure(Reason::ProviderUnavailable, "login file unusable"))?;
    // Printable ASCII, as `normalize_claude_token` checked.
    let secret = std::str::from_utf8(&token).map_err(|_| setup())?;
    let work = claimed.request.agent.as_ref();
    // An agent's settings are the host's, beside its home, not in it.
    let settings = place.root.join("settings.json");
    write_settings(&settings, home, identity, work.is_some()).map_err(|_| setup())?;
    let candidate = &claimed.candidate;
    let effort = candidate.reasoning.as_deref();
    let args = match work {
        Some(work) => agent_arguments(&candidate.model, effort, &settings, work.max_turns),
        None => arguments(&candidate.model, effort, &settings),
    };
    let args = args.ok_or_else(setup)?;
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
    if work.is_some() {
        command
            .env("PATH", place.path(true))
            .env("TMPDIR", place.tmp());
        // What the harness says when it cannot start goes to its standard
        // error: send that to the event stream, where lines that are not
        // events are kept (redacted, capped) for the failure's detail.
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| match libc::dup2(1, 2) {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
    }
    // The redaction takes every login's secrets, read once the native has
    // ended, and this run's token.
    if work.is_some() {
        let prompt = validate::agent_prompt(&claimed.request);
        let (mut transcript, mut result) = (agent::Transcript::default(), None);
        let mut stray = String::new();
        let mut each = |line: &[u8]| match serde_json::from_slice::<Value>(line) {
            Ok(event) if event["type"] == "result" => result = Some(event),
            Ok(event) => transcript.claude(&event),
            Err(_) if stray.len() < STRAY_BYTES => {
                stray.extend(String::from_utf8_lossy(line).chars().take(STRAY_BYTES));
                stray.push(' ');
            }
            Err(_) => {}
        };
        let mut lines = agent::Lines::default();
        let ran = execute_into(command, prompt, session, &mut |chunk| {
            lines.feed(chunk, &mut each)
        })
        .await;
        lines.end(&mut each);
        let secrets = run.secrets();
        let redact = |text: &str| scrub(text.replace(secret, agent::REDACTED), &secrets);
        run.keep(transcript.finish(|text| Some(redact(text))));
        let status =
            ran.map_err(|_| failure(Reason::ProviderError, "native run could not start"))?;
        // The harness's own failure counts only when there is no outcome.
        let failed = match &result {
            Some(result) => outcome(status, result.to_string().as_bytes(), redact).err(),
            None if status.success() => None,
            None => Some((
                Reason::ProviderError,
                format!("native exited with {status}: {}", redact(stray.trim())),
            )),
        };
        let limit = claimed.request.max_output_bytes;
        let reply = agent::conclude(cwd, limit, failed, |text| Some(redact(&text)))?;
        return Ok((reply, result.as_ref().and_then(usage)));
    }
    let ran = execute(command, validate::prompt(&claimed.request), session).await;
    let secrets = run.secrets();
    let redact = |text: &str| scrub(text.replace(secret, agent::REDACTED), &secrets);
    match ran {
        Err(_) => Err(failure(Reason::ProviderError, "native run could not start")),
        Ok((status, stdout)) => outcome(status, &stdout, redact),
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

/// The arguments of agent work: those of [`arguments`] with the event stream
/// as output, and in place of no tools the file tools and a shell that may
/// run only [`agent::SHELL_EXECUTABLES`]. `--restricted` confines the file
/// tools to the workspace, `dontAsk` refuses whatever is not allowed (nobody
/// is there to ask), the web tools are refused outright, and the agent stops
/// after `max_turns` turns.
fn agent_arguments(
    model: &str,
    effort: Option<&str>,
    settings: &Path,
    max_turns: u32,
) -> Option<Vec<String>> {
    let mut args = arguments(model, effort, settings)?;
    let format = args.iter().position(|a| a == "--output-format")?;
    args[format + 1] = "stream-json".into();
    args.insert(format + 2, "--verbose".into());
    let tools = args.iter().position(|a| a == "--tools")?;
    let file_tools = ["Read", "Write", "Edit", "Glob", "Grep"];
    let shell = agent::SHELL_EXECUTABLES.map(|name| format!("Bash({name}:*)"));
    let agent = [
        "--restricted".to_owned(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--tools".into(),
        format!("Bash,{}", file_tools.join(",")),
        "--allowedTools".into(),
    ]
    .into_iter()
    .chain(shell)
    .chain(file_tools.map(String::from))
    .chain(["--disallowedTools", "WebFetch", "WebSearch", "--max-turns"].map(String::from))
    .chain([max_turns.to_string()]);
    // In place of `--tools ""`.
    args.splice(tools..tools + 2, agent);
    Some(args)
}

/// The settings file at `path`: no hooks and no project MCP servers. The
/// home belongs to the native's user. An answer run's file is its own; an
/// agent's stays the host's, readable (0644) and not writable by the agent,
/// in a directory the agent cannot write either.
fn write_settings(
    path: &Path,
    home: &Path,
    identity: Identity,
    agent: bool,
) -> std::io::Result<()> {
    let body = json!({"disableAllHooks": true, "enableAllProjectMcpServers": false});
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if agent { 0o644 } else { 0o600 })
        .open(path)?
        .write_all(body.to_string().as_bytes())?;
    // `mode` is narrowed by the host's umask; an agent must read its file.
    let mode = if agent { 0o644 } else { 0o600 };
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if identity.drop_privileges {
        let id = Some(identity.run_as);
        if !agent {
            std::os::unix::fs::chown(path, id, id)?;
        }
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
    Ok((redact(reply), usage(&object)))
}

/// The usage a result object reports, if it reports both counts.
fn usage(result: &Value) -> Option<Usage> {
    let usage = &result["usage"];
    match (
        usage["input_tokens"].as_u64(),
        usage["output_tokens"].as_u64(),
    ) {
        (Some(input_tokens), Some(output_tokens)) => Some(Usage {
            input_tokens,
            output_tokens,
        }),
        _ => None,
    }
}
