//! Human and agent command translation. Domain authority and mutation recovery
//! stay on the server.
//!
//! One binary, two credential modes (spec 0006 §4.1):
//! - **owner mode**: `--origin` and `--cf-access-token-file`, on the admin listener;
//! - **channel mode**: `AQ_CHANNEL=unix:/aq/channel/aq.sock` inside a sandbox.
//!   The CLI sends no credential; the channel proxy behind the socket holds it.
//!
//! Every registered command is reachable through `run` (the generic command
//! surface, `POST /api/v1/commands/{name}`); the typed subcommands are thin
//! wrappers that build the same request.
mod actions;
pub mod catalog;
mod chat;
pub mod cli;
mod client;
pub mod flags;
mod input;
mod json;
pub mod render;
pub mod result;
mod surface;
mod typed;
pub mod wire;
use aq_human_http::Client;
use clap::Parser;
use serde_json::{Value, json};
use std::{ffi::OsString, path::PathBuf, sync::OnceLock};
use uuid::Uuid;

pub use surface::Invocation;
pub use typed::{Comment, Knowledge, Message, Operation, Question, Task, Wait};

pub type Result<T> = std::result::Result<T, Value>;
pub fn error(code: &str, message: &str) -> Value {
    json!({"code":code,"message":message})
}
pub fn unsupported(operation: &str) -> Value {
    error(
        "unsupported",
        &format!("{operation} is unsupported by the published server contract"),
    )
}

/// How this process reaches AQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Cloudflare Access owner credential on the admin listener.
    Owner,
    /// The sandbox channel: a Unix socket whose proxy injects the attempt credential.
    Channel,
}
impl Mode {
    /// The registry principal whose route serves this mode (commands.json `principals`).
    pub fn principal(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Channel => "agent",
        }
    }
}
/// A connected client and its mode.
pub struct Ctx {
    pub http: Client,
    pub mode: Mode,
}

/// The name this binary was invoked as: `aq` inside sandboxes (owner
/// decision 5), `aqv2` on hosts, where v1 still owns `aq`.
pub fn program() -> &'static str {
    static NAME: OnceLock<&'static str> = OnceLock::new();
    NAME.get_or_init(|| {
        let invoked = std::env::args_os().next().map(PathBuf::from);
        if invoked.as_deref().and_then(|p| p.file_name()) == Some("aq".as_ref()) {
            "aq"
        } else {
            "aqv2"
        }
    })
}
/// The read-only command that inspects an operation after an uncertain outcome.
pub fn recovery(mode: Mode, id: Uuid) -> String {
    match mode {
        Mode::Owner => format!(
            "{} --origin ORIGIN --cf-access-token-file TOKEN_FILE operations show {id}",
            program()
        ),
        Mode::Channel => format!("{} operation show {id}", program()),
    }
}
pub fn with_operation(error: Value, id: Uuid) -> Value {
    with_operation_in(Mode::Owner, error, id)
}
pub fn with_operation_in(mode: Mode, mut error: Value, id: Uuid) -> Value {
    error["operation_id"] = json!(id);
    error["recovery"] = json!(recovery(mode, id));
    error
}

/// Refusal codes (spec 0006 §6.8, commands-1 `RefusalCode`) that mean the
/// caller's authority does not allow the call. They exit 14.
pub const REFUSED: &[&str] = &[
    "grant_missing",
    "scope_denied",
    "owner_only",
    "quota_exceeded",
    "session_revoked",
    "session_expired",
    "channel_refused",
    "not_available",
    "unauthorized",
];
/// The process exit status for an error value. Documented in the README and
/// `--help`:
///
/// | Exit | Meaning |
/// | --- | --- |
/// | 0 | success |
/// | 1 | any other error |
/// | 2 | invalid arguments |
/// | 10 | mutation pending or unknown: inspect its operation ID |
/// | 11 | mutation conflict |
/// | 12 | mutation rejected |
/// | 13 | transport failure or closed channel (a mutation's outcome may be unknown) |
/// | 14 | refused: grant, scope, owner-only, quota, session or channel (§6.8) |
/// | 15 | invalid server response |
/// | 16 | a bounded wait ended before its condition (`question wait`) |
pub fn exit_code(error: &Value) -> u8 {
    let code = error["code"].as_str().unwrap_or_default();
    if REFUSED.contains(&code) {
        return 14;
    }
    match code {
        "invalid_arguments" => 2,
        "operation_unconfirmed" => {
            let op = &error["operation"];
            let disposition = op["data"]["disposition"]
                .as_str()
                .or(op["state"].as_str())
                .unwrap_or_default();
            match disposition {
                "conflict" => 11,
                "rejected" => 12,
                _ => 10,
            }
        }
        "transport_error" | "channel_closed" => 13,
        "invalid_response" => 15,
        "wait_timeout" => 16,
        _ => 1,
    }
}

/// Owner mode, for tests and embedding: parse `args` and execute.
pub async fn execute(http: Client, args: &[&str]) -> Result<Value> {
    execute_in(
        &Ctx {
            http,
            mode: Mode::Owner,
        },
        args,
    )
    .await
}
/// Channel mode against the socket at `socket`, for tests and embedding.
pub async fn execute_channel(socket: &std::path::Path, args: &[&str]) -> Result<Value> {
    execute_in(&channel_ctx(socket)?, args).await
}
pub async fn execute_in(ctx: &Ctx, args: &[&str]) -> Result<Value> {
    let commands =
        cli::Commands::try_parse_from(std::iter::once(program()).chain(args.iter().copied()))
            .map_err(|_| {
                error(
                    "invalid_arguments",
                    &format!("Invalid command arguments; use {} --help", program()),
                )
            })?;
    dispatch(ctx, &commands.command).await
}
async fn dispatch(ctx: &Ctx, command: &cli::Command) -> Result<Value> {
    use cli::Command as C;
    match command {
        C::Run(args) => surface::run(ctx, &args.name, &args.flags).await,
        C::Discover { all } => surface::discover(ctx, *all).await,
        C::Whoami => surface::whoami(ctx).await,
        C::Task(c) => typed::task(ctx, c).await,
        C::Knowledge(c) => typed::knowledge(ctx, c).await,
        C::Comment(c) => typed::comment(ctx, c).await,
        C::Question(c) => typed::question(ctx, c).await,
        C::Wait(c) => typed::wait(ctx, c).await,
        C::Message(c) => typed::message(ctx, c).await,
        C::Operation(c) => typed::operation(ctx, c).await,
        C::External(tokens) => surface::external(ctx, tokens).await,
        C::Result(cli::ResultCommand::Command(rest)) => {
            let tokens: Vec<String> = std::iter::once("result".into())
                .chain(rest.iter().cloned())
                .collect();
            surface::external(ctx, &tokens).await
        }
        C::Result(_) => Err(error(
            "unsupported",
            &format!(
                "`{} result submit|status` runs as its own process",
                program()
            ),
        )),
        C::Operations(cli::Operations::Show { id }) if ctx.mode == Mode::Channel => {
            typed::operation(ctx, &Operation::Show { id: *id }).await
        }
        legacy if ctx.mode == Mode::Channel => Err(error(
            "unsupported",
            &format!(
                "`{} {}` is an owner command on the admin listener; through the channel use the typed commands, `{0} run` or `{0} commands`",
                program(),
                legacy.group()
            ),
        )),
        legacy => client::execute(&ctx.http, legacy).await,
    }
}

fn limits() -> aq_human_http::Limits {
    aq_human_http::Limits {
        max_request_bytes: 65536,
        ..Default::default()
    }
}
fn channel_ctx(socket: &std::path::Path) -> Result<Ctx> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::metadata(socket) {
        Ok(m) if m.file_type().is_socket() => {}
        Ok(_) => {
            return Err(error(
                "configuration_error",
                "AQ_CHANNEL does not name a Unix socket",
            ));
        }
        // Nothing was sent, so nothing can have taken effect.
        Err(_) => {
            return Err(error(
                "channel_closed",
                "The AQ channel socket is not there: the attempt's channel is closed or was never opened",
            ));
        }
    }
    let http = Client::channel(socket, limits()).map_err(client::http_error)?;
    Ok(Ctx {
        http,
        mode: Mode::Channel,
    })
}
/// `AQ_CHANNEL=unix:/absolute/path`.
pub fn channel_path(value: &OsString) -> Result<PathBuf> {
    let bad = || {
        error(
            "configuration_error",
            "AQ_CHANNEL must be unix:/absolute/path/to/aq.sock",
        )
    };
    let text = value.to_str().ok_or_else(bad)?;
    let path = PathBuf::from(text.strip_prefix("unix:").ok_or_else(bad)?);
    if !path.is_absolute() {
        return Err(bad());
    }
    Ok(path)
}
fn connect(cli: &cli::Cli, channel: Option<OsString>) -> Result<Ctx> {
    if let Some(channel) = channel {
        if cli.cf_access_token_file.is_some() {
            return Err(error(
                "invalid_arguments",
                "AQ_CHANNEL is set: channel mode sends no credential, so --cf-access-token-file is refused",
            ));
        }
        if cli.origin.is_some() {
            return Err(error(
                "invalid_arguments",
                "AQ_CHANNEL is set: channel mode always uses the channel, so --origin is refused",
            ));
        }
        return channel_ctx(&channel_path(&channel)?);
    }
    let origin = cli.origin.as_ref().ok_or_else(|| {
        error(
            "invalid_arguments",
            "--origin is required (or AQ_CHANNEL inside a sandbox)",
        )
    })?;
    let token = cli
        .cf_access_token_file
        .clone()
        .ok_or_else(|| error("invalid_arguments", "--cf-access-token-file is required"))?;
    let http = Client::new(
        origin,
        aq_human_http::Authentication::CfAccessTokenFile(token),
        limits(),
    )
    .map_err(client::http_error)?;
    Ok(Ctx {
        http,
        mode: Mode::Owner,
    })
}
/// The CLI tokens of a command that is not typed in this binary.
fn external_tokens(command: &cli::Command) -> Option<Vec<String>> {
    use cli::Command as C;
    use typed::{Comment, Knowledge, Message, Operation, Question, Task, Wait};
    let (group, rest) = match command {
        C::External(tokens) => return Some(tokens.clone()),
        C::Task(Task::Other(rest)) => ("task", rest),
        C::Knowledge(Knowledge::Other(rest)) => ("knowledge", rest),
        C::Comment(Comment::Other(rest)) => ("comment", rest),
        C::Question(Question::Other(rest)) => ("question", rest),
        C::Wait(Wait::Other(rest)) => ("wait", rest),
        C::Message(Message::Other(rest)) => ("message", rest),
        C::Operation(Operation::Other(rest)) => ("operation", rest),
        C::Result(cli::ResultCommand::Command(rest)) => ("result", rest),
        _ => return None,
    };
    Some(
        std::iter::once(group.to_owned())
            .chain(rest.iter().cloned())
            .collect(),
    )
}
/// Add what the registry knows to clap's help: the command groups at the top
/// level, and under each typed group the registry commands that share it.
pub fn with_registry_help(mut command: clap::Command) -> clap::Command {
    let program = program();
    let mut groups = String::new();
    let mut line = String::from(" ");
    for g in flags::groups() {
        if line.len() + g.len() > 76 {
            groups.push_str(&line);
            groups.push('\n');
            line = String::from(" ");
        }
        line.push(' ');
        line.push_str(&g);
    }
    groups.push_str(&line);
    command = command.after_help(format!(
        "{}\n\nRegistry command groups (`{program} GROUP` lists a group, `{program} COMMAND --help` a command's flags):\n{groups}",
        cli::AFTER_HELP
    ));
    for group in [
        "task",
        "knowledge",
        "comment",
        "question",
        "wait",
        "message",
        "operation",
        "result",
    ] {
        let lines = flags::group(&[group.to_owned()]);
        if lines.is_empty() {
            continue;
        }
        let width = lines.iter().map(|(t, _)| t.len()).max().unwrap_or(0);
        let mut text =
            format!("Registry commands (`{program} COMMAND --help` shows a command's flags):\n");
        for (tokens, summary) in lines {
            text.push_str(&format!("  {tokens:<width$}  {summary}\n"));
        }
        command = command.mut_subcommand(group, |c| c.after_help(text.trim_end().to_owned()));
    }
    command
}
pub async fn run(cli: cli::Cli) -> Result<Value> {
    if let Some(operation) = cli.commands.command.unsupported() {
        return Err(unsupported(operation));
    }
    // Describing a built-in command needs no connection.
    if let cli::Command::Run(args) = &cli.commands.command
        && args.flags.describe
    {
        return surface::describe(&args.name);
    }
    let channel = std::env::var_os("AQ_CHANNEL").filter(|v| !v.is_empty());
    // Generated help needs no connection either.
    let mode = if channel.is_some() {
        Mode::Channel
    } else {
        Mode::Owner
    };
    if let Some(tokens) = external_tokens(&cli.commands.command)
        && let Some(help) = surface::help(mode, &tokens)
    {
        return Ok(help);
    }
    let ctx = connect(&cli, channel)?;
    dispatch(&ctx, &cli.commands.command).await
}
