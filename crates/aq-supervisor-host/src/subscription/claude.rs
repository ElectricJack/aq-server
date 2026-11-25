//! A headless Claude Code turn. The verified native gets the provider login
//! in its environment and can use AQ only through the installed MCP broker.
use super::{CommandHome, Identity, native_command};
use crate::{
    config::{Config, Subscription, protected_read},
    internal::{
        claude::{EFFORTS, ENVIRONMENT},
        process::{Session, execute_into},
    },
};
use anyhow::{Context, Result, ensure};
use aq_intelligence_config::material::normalize_claude_token;
use serde::Deserialize;
use serde_json::json;
use std::{
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAX_STDOUT: usize = 1048576;
const PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub(super) async fn turn(
    c: &Config,
    home: &Path,
    credential: &Path,
    startup: &[u8],
    executable: &std::fs::File,
    actions: &Path,
) -> Result<String> {
    let settings = c
        .subscription
        .as_ref()
        .context("subscription mode missing")?;
    let identity = Identity::NATIVE;
    let uid = Some(identity.run_as);
    std::os::unix::fs::chown(home, uid, uid)?;
    let token = normalize_claude_token(Zeroizing::new(protected_read(&c.secret_file, 65536)?))?;
    let token = std::str::from_utf8(&token)?;
    let reasoning = c.model.reasoning.as_str();
    let mut command = native_command(
        executable,
        arguments(
            c.model.model.as_str(),
            reasoning,
            settings,
            home,
            credential,
            actions,
        )?,
        CommandHome {
            home,
            cwd: &c.workspace,
        },
        identity,
    );
    set_environment(&mut command, home, token, reasoning);

    let now = crate::clock()?.boot_ms;
    let remaining = c.host.deadline_boot_ms.saturating_sub(now);
    ensure!(remaining > 0, "native deadline passed");
    let limit = Duration::from_millis(remaining);
    let session = Session::new(Instant::now() + limit, None);
    let mut buffer = Zeroizing::new(Vec::new());
    let mut overflow = false;
    let mut capture = |chunk: &[u8]| {
        let keep = chunk.len().min(MAX_STDOUT - buffer.len());
        overflow |= keep < chunk.len();
        buffer.extend_from_slice(&chunk[..keep]);
    };
    let status = tokio::time::timeout(
        limit,
        execute_into(
            command,
            std::str::from_utf8(startup)?.to_owned(),
            &session,
            &mut capture,
        ),
    )
    .await??;
    ensure!(!overflow, "native supervisor result byte bound");
    reply(status, &buffer, token)
}

fn set_environment(command: &mut Command, home: &Path, token: &str, reasoning: &str) {
    command
        .env_remove("CODEX_HOME")
        .env("CLAUDE_CONFIG_DIR", home)
        .envs(ENVIRONMENT)
        .env("CLAUDE_CODE_OAUTH_TOKEN", token)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if reasoning == "off" {
        command.env("MAX_THINKING_TOKENS", "0");
    }
}

fn arguments(
    model: &str,
    reasoning: &str,
    settings: &Subscription,
    home: &Path,
    credential: &Path,
    actions: &Path,
) -> Result<Vec<String>> {
    let broker = broker_config(home, credential, &settings.server_origin, actions)?;
    let private_settings = json!({"disableAllHooks": true, "enableAllProjectMcpServers": false});
    let mut args = [
        "-p",
        "--output-format",
        "json",
        "--input-format",
        "text",
        "--model",
        model,
        "--setting-sources",
        "",
        "--settings",
        &private_settings.to_string(),
        "--restricted",
        "--strict-mcp-config",
        "--mcp-config",
        &broker,
        "--no-session-persistence",
        "--disable-slash-commands",
        "--tools",
        "",
        "--permission-mode",
        "dontAsk",
        "--append-system-prompt",
        &settings.instructions,
        "--allowedTools",
        "mcp__aq__aq_discover",
        "mcp__aq__aq_describe",
        "mcp__aq__aq_command",
        "mcp__aq__aq_attachment",
    ]
    .map(String::from)
    .to_vec();
    if cfg!(all(
        feature = "supervisor_full_access",
        feature = "delivery_playbook"
    )) {
        args.extend(["mcp__aq__repo_sync".into(), "mcp__aq__repo_read".into()]);
    }
    if EFFORTS.contains(&reasoning) {
        args.extend(["--effort".into(), reasoning.into()]);
    } else {
        ensure!(
            matches!(reasoning, "default" | "off"),
            "unsupported Claude effort"
        );
    }
    Ok(args)
}

fn broker_config(home: &Path, credential: &Path, origin: &str, actions: &Path) -> Result<String> {
    // Claude supplies its own environment to stdio MCP subprocesses. Start
    // the AQ client with an explicit environment so its broker and its git
    // operations never inherit the provider login.
    let binding = |key: &str, path: &Path| -> Result<String> {
        Ok(format!(
            "{key}={}",
            path.to_str().context("broker path UTF8")?
        ))
    };
    let args = [
        "-i".into(),
        binding("HOME", home)?,
        format!("PATH={PATH}"),
        binding("AQ_SUPERVISOR_CREDENTIAL_FILE", credential)?,
        format!("AQ_SUPERVISOR_ORIGIN={origin}"),
        binding("AQ_SUPERVISOR_ACTIONS_FILE", actions)?,
        "/usr/local/bin/aq-supervisor-host".into(),
        "mcp".into(),
    ];
    Ok(json!({"mcpServers": {"aq": {"command": "/usr/bin/env", "args": args}}}).to_string())
}

#[derive(Deserialize)]
struct NativeResult {
    #[serde(rename = "type")]
    kind: String,
    is_error: bool,
    result: String,
}

fn reply(status: ExitStatus, buffer: &[u8], token: &str) -> Result<String> {
    ensure!(status.success(), "native supervisor turn failed");
    let result: NativeResult = serde_json::from_slice(buffer).context("native result format")?;
    ensure!(
        result.kind == "result" && !result.is_error,
        "native supervisor turn failed"
    );
    ensure!(
        !result.result.trim().is_empty() && !result.result.contains('\0'),
        "native final reply missing"
    );
    Ok(result.result.replace(token, "[credential redacted]"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, path::PathBuf};

    #[test]
    fn command_uses_only_the_scoped_broker_and_keeps_login_env_only() {
        let home = PathBuf::from("/temporary home");
        let auth = home.join("credential");
        let actions = home.join("receipts");
        let settings = Subscription {
            server_origin: "https://aq.example".into(),
            instructions: "Act only within this project.\nReply in text.".into(),
            control_revision: 3,
            options: Default::default(),
        };
        let args = arguments("claude-sonnet", "off", &settings, &home, &auth, &actions).unwrap();
        let option = |name: &str| {
            let start = args.iter().position(|arg| arg == name).unwrap();
            &args[start + 1]
        };
        assert_eq!(option("--append-system-prompt"), &settings.instructions);
        assert_eq!(option("--model"), "claude-sonnet");
        assert_eq!(option("--tools"), "");
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(!args.iter().any(|arg| arg == "--effort"));
        let broker: serde_json::Value = serde_json::from_str(option("--mcp-config")).unwrap();
        let broker = &broker["mcpServers"]["aq"];
        assert_eq!(broker["command"], "/usr/bin/env");
        assert_eq!(broker["args"][0], "-i");
        assert_eq!(broker["args"][1], "HOME=/temporary home");
        assert!(broker["args"].as_array().unwrap().iter().any(|arg| {
            arg.as_str() == Some("AQ_SUPERVISOR_CREDENTIAL_FILE=/temporary home/credential")
        }));
        assert!(args.iter().any(|arg| arg == "mcp__aq__aq_command"));
        assert!(!args.iter().any(|arg| arg.contains("Bash")));
        let mut command = Command::new("claude");
        command.env_clear().args(args);
        set_environment(&mut command, &home, "sample-login", "off");
        let env = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        let lookup = |key: &str| env[std::ffi::OsStr::new(key)].unwrap();
        assert_eq!(lookup("CLAUDE_CODE_OAUTH_TOKEN"), "sample-login");
        assert_eq!(lookup("MAX_THINKING_TOKENS"), "0");
        assert!(!command.get_args().any(|arg| arg == "sample-login"));

        let args = arguments("claude-sonnet", "high", &settings, &home, &auth, &actions).unwrap();
        assert_eq!(&args[args.len() - 2..], &["--effort", "high"]);
    }

    #[test]
    fn accepts_only_a_successful_final_result_and_redacts_the_provider_login() {
        let success = ExitStatus::from_raw(0);
        let output =
            r#"{"type":"result","is_error":false,"result":"hello sample-login ☺"}"#.as_bytes();
        assert_eq!(
            reply(success, output, "sample-login").unwrap(),
            "hello [credential redacted] ☺"
        );
        assert!(reply(ExitStatus::from_raw(256), output, "sample-login").is_err());
        for invalid in [
            r#"{"type":"system","is_error":false,"result":"hello"}"#,
            r#"{"type":"result","is_error":true,"result":"hello"}"#,
            r#"{"type":"result","is_error":false,"result":" "}"#,
            r#"{"type":"result","is_error":false,"result":"hello\u0000"}"#,
            r#"{"type":"result","result":"hello"}"#,
            "diagnostic output",
        ] {
            assert!(reply(success, invalid.as_bytes(), "sample-login").is_err());
        }
    }
}
