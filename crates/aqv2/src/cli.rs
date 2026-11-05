use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "aqv2",
    version,
    about = "Human access to AQ v2 domain operations"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub origin: Option<String>,
    /// Absolute path to an owner-only Cloudflare Access JWT file.
    #[arg(long, global = true)]
    pub cf_access_token_file: Option<PathBuf>,
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
    #[command(subcommand)]
    Tasks(Tasks),
    #[command(subcommand)]
    Decisions(Decisions),
    #[command(subcommand)]
    Messages(Messages),
    /// Read the supervisor conversation, or send and wait for a reply.
    Chat(Chat),
    #[command(subcommand)]
    Workers(Workers),
    #[command(subcommand)]
    Projects(Projects),
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
    Cancel {
        id: Uuid,
    },
    Retry {
        id: Uuid,
    },
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
    Revoke { id: Uuid },
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

impl Command {
    pub fn unsupported(&self) -> Option<&'static str> {
        match self {
            Self::Tasks(Tasks::Cancel { .. }) => Some("task cancellation"),
            Self::Tasks(Tasks::Retry { .. }) => Some("task retry"),
            Self::Workers(Workers::Revoke { .. }) => Some("worker revocation"),
            Self::Workers(Workers::Enroll) => Some("worker enrollment"),
            Self::Projects(Projects::Onboard) => Some("project onboarding"),
            _ => None,
        }
    }
}
