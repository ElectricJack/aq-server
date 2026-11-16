use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use uuid::Uuid;

pub const AFTER_HELP: &str = "\
Modes:
  owner    --origin https://aq.example --cf-access-token-file /private/access.jwt
  channel  AQ_CHANNEL=unix:/aq/channel/aq.sock (inside a sandbox; no credential is sent)

Every registered command runs through `run NAME`; `commands` lists what you may run.

Exit status:
  0 success, 1 other error, 2 invalid arguments,
  10 mutation pending or unknown (inspect the printed operation ID), 11 conflict, 12 rejected,
  13 transport failure or closed channel, 14 refused (grant, scope, owner-only, quota, session),
  15 invalid server response, 16 wait ended before its condition
  `result submit|status` (worker-local) keeps aq-agent's: 0 ready or recorded, 10 pending,
  11 conflict, 13 needs reconciliation or unknown, 15 unusable channel or request";

#[derive(Parser)]
#[command(
    name = "aqv2",
    version,
    about = "AQ v2 command line: the owner's terminal and the agent's channel",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Owner mode: the AQ admin origin (not with AQ_CHANNEL).
    #[arg(long, global = true)]
    pub origin: Option<String>,
    /// Absolute path to an owner-only Cloudflare Access JWT file.
    #[arg(long, global = true)]
    pub cf_access_token_file: Option<PathBuf>,
    /// Print one JSON document: {"schema_version":1,"data"|"error":…}.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(flatten)]
    pub commands: Commands,
}

#[derive(Parser)]
pub struct Commands {
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Subcommand)]
pub enum Command {
    /// Invoke any registered command: `run task.show --param task_id=ID`.
    Run(RunArgs),
    /// List the commands you may run, with the scope of each grant.
    #[command(name = "commands")]
    Discover {
        /// Also list the commands you cannot run, each with the reason.
        #[arg(long)]
        all: bool,
    },
    /// Who you are to AQ: attempt, task, project, grants and credential expiry.
    Whoami,
    /// Tasks: show, create (`--parent self` for a subtask).
    #[command(subcommand)]
    Task(crate::typed::Task),
    /// Knowledge: search, show, author.
    #[command(subcommand)]
    Knowledge(crate::typed::Knowledge),
    /// A task's comments: add, list, edit, delete.
    #[command(subcommand)]
    Comment(crate::typed::Comment),
    /// Questions to the owner: ask, show, wait (bounded).
    #[command(subcommand)]
    Question(crate::typed::Question),
    /// Your attempt's H4 wait: register, inspect, ack; list, show.
    #[command(subcommand)]
    Wait(crate::typed::Wait),
    /// Mailboxes: send, inbox, body, ack.
    #[command(subcommand)]
    Message(crate::typed::Message),
    /// Inspect an operation by ID after an uncertain outcome.
    #[command(subcommand)]
    Operation(crate::typed::Operation),
    /// Your attempt's local result claim: submit, status (AQ_AGENT_RESULT_DIR;
    /// not a server command). Other `result …` tokens are registry commands.
    #[command(subcommand)]
    Result(ResultCommand),
    /// Any other command by its CLI tokens (commands.json `cli`), e.g. `task graph show`.
    #[command(external_subcommand)]
    External(Vec<String>),
    /// Owner: task overview and lists (admin listener).
    #[command(subcommand)]
    Tasks(Tasks),
    /// Owner: review decisions.
    #[command(subcommand)]
    Decisions(Decisions),
    /// Owner: the supervisor conversation's messages.
    #[command(subcommand)]
    Messages(Messages),
    /// Read the supervisor conversation, or send and wait for a reply.
    Chat(Chat),
    /// Owner: workers.
    #[command(subcommand)]
    Workers(Workers),
    /// Owner: projects.
    #[command(subcommand)]
    Projects(Projects),
    /// Owner: inspect an action-union operation.
    #[command(subcommand)]
    Operations(Operations),
}
#[derive(Args, Default)]
pub struct Page {
    #[arg(long)]
    pub limit: Option<u16>,
    #[arg(long)]
    pub cursor: Option<String>,
}
#[derive(Subcommand)]
pub enum Tasks {
    List {
        #[arg(long)]
        project: Option<Uuid>,
        #[command(flatten)]
        page: Page,
    },
    Show {
        id: Uuid,
        #[arg(long)]
        project: Uuid,
    },
    Create(CreateTask),
    /// Cancel a task and stop any attempt holding it (`task.cancel`).
    Cancel(TaskChange),
    /// Release a task whose attempt has settled for a replacement attempt (`task.retry`).
    Retry(TaskChange),
}
#[derive(Args)]
pub struct TaskChange {
    pub id: Uuid,
    #[arg(long)]
    pub project: Uuid,
    /// The task version the change expects; the current one is read when omitted.
    #[arg(long, value_parser = revision)]
    pub revision: Option<String>,
}
fn revision(value: &str) -> Result<String, String> {
    let ok = !value.is_empty()
        && value.len() <= 19
        && !value.starts_with('0')
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<i64>().is_ok();
    ok.then(|| value.to_string())
        .ok_or_else(|| "a revision is a positive decimal number".into())
}
/// A machine key, or a node ID (hyphenated, with or without `node:`) for a
/// single legacy node. Anything else that fits a key is a key.
fn machine(value: &str) -> Result<String, String> {
    let id = value.strip_prefix("node:").unwrap_or(value);
    if id.len() == 36
        && let Ok(node) = id.parse::<Uuid>()
    {
        return Ok(format!("node:{node}"));
    }
    let key = value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && value.bytes().next().is_some_and(|b| b != b'-');
    key.then(|| value.to_string())
        .ok_or_else(|| "a machine is its key or a node ID".into())
}
#[derive(Args)]
pub struct CreateTask {
    #[arg(long)]
    pub project: Uuid,
    #[arg(long)]
    pub expected_project_revision: String,
    #[arg(long)]
    pub title: String,
    #[arg(long, default_value = "")]
    pub body: String,
    #[arg(long, default_value = "")]
    pub acceptance_criteria: String,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[arg(long, value_enum, default_value = "draft")]
    pub task_state: TaskState,
}
#[derive(Clone, Copy, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Draft,
    Active,
}
#[derive(Subcommand)]
pub enum Decisions {
    List {
        #[arg(long)]
        project: Option<Uuid>,
        #[arg(long, value_enum, default_value = "open")]
        state: DecisionState,
        #[command(flatten)]
        page: Page,
    },
    Show {
        id: Uuid,
    },
    /// Respond using the dashboard decision.resolve payload; a fresh operation ID is minted.
    Respond {
        id: Uuid,
        #[arg(long)]
        request: PathBuf,
    },
}
#[derive(Clone, Copy, ValueEnum)]
pub enum DecisionState {
    Open,
    Closed,
}
#[derive(Args, Default)]
pub struct History {
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=16))]
    pub limit: Option<u8>,
    #[arg(long, conflicts_with = "after")]
    pub before: Option<String>,
    #[arg(long, conflicts_with = "before")]
    pub after: Option<String>,
}
#[derive(Subcommand)]
pub enum Messages {
    Inbox(History),
    Send { text: String },
}
#[derive(Args)]
pub struct Chat {
    #[arg(long)]
    pub once: Option<String>,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub timeout: u64,
    #[command(flatten)]
    pub history: History,
}
#[derive(Subcommand)]
pub enum Workers {
    List,
    /// Revoke every node of a machine (`worker.revoke`).
    Revoke {
        /// The machine's key (`id` of a machine in `run dashboard.swarm`), or the
        /// node ID of a legacy node that belongs to no service machine.
        #[arg(value_parser = machine)]
        machine: String,
    },
    Enroll,
}
#[derive(Subcommand)]
pub enum Projects {
    List {
        #[command(flatten)]
        page: Page,
    },
    Show {
        id: Uuid,
    },
    Onboard,
}
#[derive(Subcommand)]
pub enum Operations {
    Show { id: Uuid },
}

/// `result submit|status`: the worker-local result channel (spec 0006 §9 step 4).
#[derive(Subcommand)]
pub enum ResultCommand {
    /// Publish the one original local claim; exit 10 means pending (inspect `phase`).
    Submit {
        /// The bounded request: {"schema_version":1,"conclusion":"…"}.
        #[arg(long)]
        request_file: PathBuf,
    },
    /// Read the worker's acknowledgment and recovery data; triggers no effects.
    Status,
    /// The registry's `result …` commands (`result show`, `result list`, …).
    #[command(external_subcommand)]
    Command(Vec<String>),
}

/// `run NAME [flags]`.
#[derive(Args)]
pub struct RunArgs {
    /// The command name, e.g. `task.create` (see `commands`).
    pub name: String,
    #[command(flatten)]
    pub flags: RunFlags,
}
/// How `run` builds the CommandRequest (protocol/commands/v1 `CommandRequest`).
#[derive(Args, Default)]
pub struct RunFlags {
    /// A param: `KEY=VALUE` (a string) or `KEY:=JSON` (any JSON value). Repeatable.
    #[arg(long = "param", short = 'p', value_name = "KEY=VALUE|KEY:=JSON")]
    pub params: Vec<String>,
    /// A JSON object of params from a file, or `-` for stdin; `--param` adds to it.
    #[arg(long, value_name = "FILE|-")]
    pub params_file: Option<String>,
    /// The action subject of an action-union mutation.
    #[arg(long, value_name = "TYPE:ID[:REVISION]")]
    pub subject: Option<String>,
    /// An expected version (defaults to the subject's revision). Repeatable.
    #[arg(long = "expect", value_name = "TYPE=REVISION")]
    pub expect: Vec<String>,
    #[arg(long)]
    pub reason: Option<String>,
    /// Reuse an operation ID (to inspect a replay); a mutation otherwise gets a fresh one.
    #[arg(long)]
    pub operation_id: Option<Uuid>,
    /// Print the built-in description of the command (grant, target, params) and exit.
    #[arg(long)]
    pub describe: bool,
}

impl Command {
    /// The subcommand group's name, for messages.
    pub fn group(&self) -> &'static str {
        match self {
            Self::Tasks(_) => "tasks",
            Self::Decisions(_) => "decisions",
            Self::Messages(_) => "messages",
            Self::Chat(_) => "chat",
            Self::Workers(_) => "workers",
            Self::Projects(_) => "projects",
            Self::Operations(_) => "operations",
            _ => "command",
        }
    }
    /// How a successful result is shown without `--json`.
    pub fn render(&self) -> crate::render::Render {
        use crate::render::Render;
        match self {
            Self::Discover { .. } => Render::Commands,
            Self::Whoami => Render::Whoami,
            _ => Render::Json,
        }
    }
    pub fn unsupported(&self) -> Option<&'static str> {
        match self {
            Self::Workers(Workers::Enroll) => Some("worker enrollment"),
            Self::Projects(Projects::Onboard) => Some("project onboarding"),
            _ => None,
        }
    }
}
