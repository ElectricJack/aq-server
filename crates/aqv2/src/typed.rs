//! Typed subcommands (spec 0006 §4.4). Each is a thin wrapper that builds one
//! [`Invocation`] for a registered command and sends it through the generic
//! surface. Params follow the route that serves the caller's principal
//! (commands.json `principals`): the agent's `/v1/agent/*` body in channel
//! mode, the owner's REST read or action-union payload in owner mode. Any
//! group also accepts a registered command's other CLI tokens
//! (`aq task graph show --param task_id=…`), which go through `run`.
use crate::{
    Ctx, Mode, Result, error, input,
    surface::{self, Invocation, invoke},
};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn invalid(message: &str) -> Value {
    error("invalid_arguments", message)
}
fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
fn put(m: &mut Map<String, Value>, key: &str, value: Option<impl serde::Serialize>) {
    if let Some(v) = value {
        m.insert(key.into(), json!(v));
    }
}
fn need<T: Copy>(value: Option<T>, flag: &str) -> Result<T> {
    value.ok_or_else(|| invalid(&format!("{flag} is required in owner mode")))
}
async fn other(ctx: &Ctx, group: &str, rest: &[String]) -> Result<Value> {
    let tokens: Vec<String> = std::iter::once(group.to_owned())
        .chain(rest.iter().cloned())
        .collect();
    surface::external(ctx, &tokens).await
}

// ---- task ----------------------------------------------------------------

#[derive(Subcommand)]
pub enum Task {
    /// Show a task. `self` (the default) is your own task in a sandbox.
    Show {
        #[arg(default_value = "self", value_name = "self|TASK_ID")]
        task: String,
        /// The task's project (owner mode).
        #[arg(long)]
        project: Option<Uuid>,
    },
    /// Create a task. `--parent self` makes a subtask of your own task.
    Create(TaskCreate),
    #[command(external_subcommand)]
    Other(Vec<String>),
}
#[derive(Args)]
pub struct TaskCreate {
    #[arg(long)]
    pub title: String,
    /// Markdown body; `-` reads it from stdin.
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub acceptance_criteria: Option<String>,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    /// The parent task: `self` (your own task) or a task ID.
    #[arg(long, value_name = "self|TASK_ID")]
    pub parent: Option<String>,
    /// Create it as a draft instead of schedulable (`active`).
    #[arg(long)]
    pub draft: bool,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=300))]
    pub priority: Option<u16>,
    #[arg(long)]
    pub task_type: Option<String>,
    /// The role the task expects (an existing role; default: the project's).
    #[arg(long)]
    pub role: Option<String>,
    /// The project (owner mode; in a sandbox it is the attempt's project).
    #[arg(long)]
    pub project: Option<Uuid>,
    /// The project revision the create expects (required in owner mode).
    #[arg(long)]
    pub expected_project_revision: Option<String>,
}
fn task_id(task: &str) -> Result<Option<Uuid>> {
    if task == "self" {
        return Ok(None);
    }
    task.parse()
        .map(Some)
        .map_err(|_| invalid("A task is `self` or a task ID"))
}
pub fn task_invocation(mode: Mode, c: &Task) -> Result<Option<Invocation>> {
    Ok(Some(match c {
        Task::Show { task, project } => {
            let id = task_id(task)?;
            let mut p = Map::new();
            match mode {
                // An omitted task is the attempt's own task.
                Mode::Channel => put(&mut p, "task_id", id),
                Mode::Owner => {
                    let id = id.ok_or_else(|| invalid("Owner mode needs a task ID, not `self`"))?;
                    p.insert("project_id".into(), json!(need(*project, "--project")?));
                    p.insert("task_id".into(), json!(id));
                }
            }
            if mode == Mode::Channel {
                put(&mut p, "project_id", *project);
            }
            Invocation::read("task.show", p)
        }
        Task::Create(a) => {
            let body = a.body.as_deref().map(input::text).transpose()?;
            let parent = match a.parent.as_deref() {
                None => None,
                Some("self") if mode == Mode::Channel => Some(json!("self")),
                Some("self") => {
                    return Err(invalid("`--parent self` is only meaningful in a sandbox"));
                }
                Some(id) => Some(json!(
                    id.parse::<Uuid>()
                        .map_err(|_| invalid("--parent is `self` or a task ID"))?
                )),
            };
            let state = if a.draft { "draft" } else { "active" };
            match mode {
                Mode::Channel => {
                    if a.project.is_some() {
                        return Err(invalid(
                            "In a sandbox a task is created in the attempt's project; drop --project",
                        ));
                    }
                    let mut p = Map::new();
                    p.insert("title".into(), json!(a.title));
                    put(&mut p, "body", body);
                    put(
                        &mut p,
                        "acceptance_criteria",
                        a.acceptance_criteria.as_ref(),
                    );
                    if !a.labels.is_empty() {
                        p.insert("labels".into(), json!(a.labels));
                    }
                    put(&mut p, "parent_id", parent);
                    if a.draft {
                        p.insert("task_state".into(), json!(state));
                    }
                    put(&mut p, "priority", a.priority);
                    put(&mut p, "task_type", a.task_type.as_ref());
                    put(&mut p, "role", a.role.as_ref());
                    put(
                        &mut p,
                        "expected_project_revision",
                        a.expected_project_revision.as_ref(),
                    );
                    Invocation::mutation("task.create", p, true)
                }
                Mode::Owner => {
                    let project = need(a.project, "--project")?;
                    let revision = a.expected_project_revision.as_deref().ok_or_else(|| {
                        invalid("--expected-project-revision is required in owner mode")
                    })?;
                    let mut p = obj(json!({"expected_project_revision":revision,"title":a.title,
                        "body":body.unwrap_or_default(),"acceptance_criteria":a.acceptance_criteria.clone().unwrap_or_default(),
                        "labels":a.labels,"task_state":state}));
                    put(&mut p, "parent_id", parent);
                    put(&mut p, "priority", a.priority);
                    put(&mut p, "task_type", a.task_type.as_ref());
                    put(&mut p, "role", a.role.as_ref());
                    Invocation::mutation("task.create", p, true).subject(
                        "project",
                        project,
                        Some(revision),
                    )
                }
            }
        }
        Task::Other(_) => return Ok(None),
    }))
}
pub async fn task(ctx: &Ctx, c: &Task) -> Result<Value> {
    match task_invocation(ctx.mode, c)? {
        Some(inv) => invoke(ctx, &inv).await,
        None => match c {
            Task::Other(rest) => other(ctx, "task", rest).await,
            _ => unreachable!(),
        },
    }
}

// ---- knowledge -----------------------------------------------------------

#[derive(Clone, Copy, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Task,
    Knowledge,
}
#[derive(Clone, Copy, ValueEnum, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    Fact,
    Decision,
    Policy,
    Procedure,
    Incident,
    Reference,
    Note,
}
#[derive(Subcommand)]
pub enum Knowledge {
    /// Search the project's knowledge (and tasks, unless `--kind` narrows it).
    Search {
        text: String,
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=64))]
        limit: Option<u8>,
        #[arg(long, value_enum)]
        kind: Option<SearchKind>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Read one knowledge record.
    Show {
        id: Uuid,
        /// The record's project (owner mode).
        #[arg(long)]
        project: Option<Uuid>,
    },
    /// Record a new finding (owner mode: author a knowledge record).
    Author(KnowledgeAuthor),
    #[command(external_subcommand)]
    Other(Vec<String>),
}
#[derive(Args)]
pub struct KnowledgeAuthor {
    #[arg(long)]
    pub title: String,
    /// Markdown body; `-` reads it from stdin.
    #[arg(long, default_value = "")]
    pub body: String,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[arg(long, value_enum)]
    pub content_type: Option<ContentType>,
    /// Why this is being recorded (sandbox; defaults to a generic reason).
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub project: Option<Uuid>,
    #[arg(long)]
    pub expected_project_revision: Option<String>,
}
pub fn knowledge_invocation(mode: Mode, c: &Knowledge) -> Result<Option<Invocation>> {
    Ok(Some(match c {
        Knowledge::Search {
            text,
            limit,
            kind,
            cursor,
        } => {
            let mut p = Map::new();
            match mode {
                Mode::Channel => {
                    p.insert("text".into(), json!(text));
                    put(&mut p, "kind_filter", *kind);
                }
                Mode::Owner => {
                    p.insert("q".into(), json!(text));
                    put(&mut p, "types", *kind);
                }
            }
            put(&mut p, "limit", *limit);
            put(&mut p, "cursor", cursor.as_ref());
            Invocation::read("knowledge.search", p)
        }
        Knowledge::Show { id, project } => {
            let mut p = obj(json!({"node_id":id}));
            if mode == Mode::Owner {
                p.insert("project_id".into(), json!(need(*project, "--project")?));
            }
            Invocation::read("knowledge.show", p)
        }
        Knowledge::Author(a) => {
            let body = input::text(&a.body)?;
            match mode {
                // An agent records a finding through its graph command route.
                Mode::Channel => {
                    let finding = json!({"kind":"create_finding","id":Uuid::new_v4(),"title":a.title,"body":body,
                        "labels":a.labels,"content_type":a.content_type.unwrap_or(ContentType::Note)});
                    Invocation::mutation("graph.command", obj(json!({"commands":[finding]})), true)
                        .reason(
                            a.reason.as_deref().unwrap_or(
                                "Finding recorded by the agent with `aq knowledge author`",
                            ),
                        )
                }
                Mode::Owner => {
                    let project = need(a.project, "--project")?;
                    let revision = a.expected_project_revision.as_deref().ok_or_else(|| {
                        invalid("--expected-project-revision is required in owner mode")
                    })?;
                    let p = obj(
                        json!({"project_id":project,"expected_project_revision":revision,"title":a.title,
                        "body":body,"labels":a.labels,"content_type":a.content_type}),
                    );
                    Invocation::mutation("knowledge.author", p, true).subject(
                        "project",
                        project,
                        Some(revision),
                    )
                }
            }
        }
        Knowledge::Other(_) => return Ok(None),
    }))
}
pub async fn knowledge(ctx: &Ctx, c: &Knowledge) -> Result<Value> {
    match knowledge_invocation(ctx.mode, c)? {
        Some(inv) => invoke(ctx, &inv).await,
        None => match c {
            Knowledge::Other(rest) => other(ctx, "knowledge", rest).await,
            _ => unreachable!(),
        },
    }
}

// ---- comment -------------------------------------------------------------

#[derive(Subcommand)]
pub enum Comment {
    /// Comment on a task (in a sandbox, your own task). `-` reads the body from stdin.
    Add {
        body: String,
        #[arg(long)]
        task: Option<Uuid>,
        #[arg(long)]
        project: Option<Uuid>,
    },
    /// A task's comment thread, newest first (in a sandbox, your own task's
    /// unless `--task` names another you may read).
    List {
        #[arg(long)]
        task: Option<Uuid>,
        #[arg(long)]
        project: Option<Uuid>,
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=100))]
        limit: Option<u8>,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Edit a comment at its current revision (in a sandbox, only your own).
    Edit {
        comment: Uuid,
        #[arg(long)]
        body: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        project: Option<Uuid>,
    },
    /// Delete a comment at its current revision (in a sandbox, only your own).
    Delete {
        comment: Uuid,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        project: Option<Uuid>,
    },
    #[command(external_subcommand)]
    Other(Vec<String>),
}
/// A revisioned comment edit or delete. In a sandbox the agent route takes
/// the comment in `params` (an agent changes only its own comments); the
/// owner's action takes it as the subject.
fn comment_change(
    name: &str,
    mode: Mode,
    mut p: Map<String, Value>,
    comment: &Uuid,
    revision: &str,
    project: Option<Uuid>,
) -> Result<Invocation> {
    if mode == Mode::Channel {
        if project.is_some() {
            return Err(invalid(
                "In a sandbox the comment is in the attempt's project; drop --project",
            ));
        }
        p.insert("comment_id".into(), json!(comment));
        return Ok(Invocation::mutation(name, p, true));
    }
    p.insert("project_id".into(), json!(need(project, "--project")?));
    Ok(Invocation::mutation(name, p, true).subject("task_comment", comment, Some(revision)))
}
pub fn comment_invocation(mode: Mode, c: &Comment) -> Result<Option<Invocation>> {
    Ok(Some(match c {
        Comment::Add {
            body,
            task,
            project,
        } => {
            let body = input::text(body)?;
            match mode {
                Mode::Channel => {
                    if task.is_some() || project.is_some() {
                        return Err(invalid(
                            "In a sandbox a comment goes on your own task; drop --task and --project",
                        ));
                    }
                    Invocation::mutation("task.comment.add", obj(json!({"body":body})), true)
                }
                Mode::Owner => {
                    let task = need(*task, "--task")?;
                    let p = obj(json!({"project_id":need(*project, "--project")?,"body":body}));
                    Invocation::mutation("task.comment.add", p, true).subject("task", task, None)
                }
            }
        }
        Comment::List {
            task,
            project,
            limit,
            cursor,
        } => {
            let mut p = Map::new();
            match mode {
                // An omitted task is the attempt's own task.
                Mode::Channel => {
                    put(&mut p, "task_id", *task);
                    put(&mut p, "project_id", *project);
                }
                Mode::Owner => {
                    p.insert("project_id".into(), json!(need(*project, "--project")?));
                    p.insert("task_id".into(), json!(need(*task, "--task")?));
                }
            }
            put(&mut p, "limit", *limit);
            put(&mut p, "cursor", cursor.as_ref());
            Invocation::read("comment.list", p)
        }
        Comment::Edit {
            comment,
            body,
            revision,
            project,
        } => {
            let p = obj(json!({"expected_revision":revision,"body":input::text(body)?}));
            comment_change("task.comment.edit", mode, p, comment, revision, *project)?
        }
        Comment::Delete {
            comment,
            revision,
            project,
        } => {
            let p = obj(json!({"expected_revision":revision}));
            comment_change("task.comment.delete", mode, p, comment, revision, *project)?
        }
        Comment::Other(_) => return Ok(None),
    }))
}
pub async fn comment(ctx: &Ctx, c: &Comment) -> Result<Value> {
    match comment_invocation(ctx.mode, c)? {
        Some(inv) => invoke(ctx, &inv).await,
        None => match c {
            Comment::Other(rest) => other(ctx, "comment", rest).await,
            _ => unreachable!(),
        },
    }
}

// ---- question ------------------------------------------------------------

#[derive(Subcommand)]
pub enum Question {
    /// Ask the owner a question (`-` reads it from stdin). It returns at once;
    /// use `question wait` for the answer.
    Ask { text: String },
    /// Show one of your questions, with its answer once given.
    Show { id: Uuid },
    /// Wait, bounded, until the question is answered (exit 16 if it is not).
    Wait {
        id: Uuid,
        /// Seconds to wait in total.
        #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=3600))]
        timeout: u64,
        /// Seconds between checks.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=60))]
        interval: u64,
    },
    #[command(external_subcommand)]
    Other(Vec<String>),
}
fn show(id: Uuid) -> Invocation {
    Invocation::read("question.show", obj(json!({"decision_id":id})))
}
/// The question's state, from the agent view or the owner's detail.
fn state(value: &Value) -> Option<&str> {
    value["state"].as_str().or_else(|| {
        ["data", "question", "item", "decision"]
            .iter()
            .find_map(|k| value[*k]["state"].as_str())
    })
}
pub fn question_invocation(c: &Question) -> Result<Option<Invocation>> {
    Ok(match c {
        Question::Ask { text } => Some(Invocation::mutation(
            "question.ask",
            obj(json!({"question":input::text(text)?})),
            true,
        )),
        Question::Show { id } => Some(show(*id)),
        _ => None,
    })
}
pub async fn question(ctx: &Ctx, c: &Question) -> Result<Value> {
    match c {
        Question::Wait {
            id,
            timeout,
            interval,
        } => {
            let deadline = Instant::now() + Duration::from_secs(*timeout);
            loop {
                let value = invoke(ctx, &show(*id)).await?;
                match state(&value) {
                    Some("answered") => return Ok(value),
                    Some("pending" | "open") => {}
                    _ => {
                        let mut e =
                            error("question_closed", "The question closed without an answer");
                        e["question"] = value;
                        return Err(e);
                    }
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let mut e = error(
                        "wait_timeout",
                        &format!(
                            "Not answered within {timeout}s; the question stays open. `{} question wait {id}` waits again",
                            crate::program()
                        ),
                    );
                    e["question"] = value;
                    return Err(e);
                }
                tokio::time::sleep(left.min(Duration::from_secs(*interval))).await;
            }
        }
        Question::Other(rest) => other(ctx, "question", rest).await,
        _ => invoke(ctx, &question_invocation(c)?.expect("typed question")).await,
    }
}

// ---- wait ------------------------------------------------------------------

#[derive(Subcommand)]
pub enum Wait {
    /// Register an H4 wait for your attempt (`--request` is the lifecycle request JSON).
    Register {
        #[arg(long, value_name = "FILE|-")]
        request: String,
    },
    /// Inspect your attempt's wait.
    Inspect {
        #[arg(long, value_name = "FILE|-")]
        request: String,
    },
    /// Acknowledge a continuation of your attempt's wait.
    Ack {
        #[arg(long, value_name = "FILE|-")]
        request: String,
    },
    /// List waits (in a sandbox, your attempt's own).
    List,
    /// Show one wait (in a sandbox, one of your attempt's).
    Show { id: Uuid },
    #[command(external_subcommand)]
    Other(Vec<String>),
}
pub fn wait_invocation(c: &Wait) -> Result<Option<Invocation>> {
    Ok(Some(match c {
        Wait::Register { request } => {
            Invocation::mutation("wait.register", input::object_source(request)?, false)
        }
        Wait::Ack { request } => {
            Invocation::mutation("wait.ack", input::object_source(request)?, false)
        }
        Wait::Inspect { request } => {
            Invocation::read("wait.inspect", input::object_source(request)?)
        }
        Wait::List => Invocation::read("wait.list", Map::new()),
        Wait::Show { id } => Invocation::read("wait.show", obj(json!({"wait_id":id}))),
        Wait::Other(_) => return Ok(None),
    }))
}
pub async fn wait(ctx: &Ctx, c: &Wait) -> Result<Value> {
    match wait_invocation(c)? {
        Some(inv) => invoke(ctx, &inv).await,
        None => match c {
            Wait::Other(rest) => other(ctx, "wait", rest).await,
            _ => unreachable!(),
        },
    }
}

// ---- message ---------------------------------------------------------------

/// A mailbox: `task` (your own task's), `task:ID` (a task in your project),
/// `session` (your own session's), `supervisor` (the current supervisor
/// conversation, from `whoami`) or `supervisor:CONVERSATION_ID`, or any
/// MailboxRecipient as `--to-json`.
#[derive(Args, Clone)]
pub struct To {
    #[arg(
        long,
        default_value = "task",
        value_name = "task|task:ID|session|supervisor|supervisor:CONVERSATION_ID"
    )]
    pub to: String,
    /// An agent-access MailboxRecipient object.
    #[arg(long, value_name = "JSON")]
    pub to_json: Option<String>,
}
#[derive(Subcommand)]
pub enum Message {
    /// Send a message (`-` reads the body from stdin).
    Send {
        body: String,
        #[command(flatten)]
        to: To,
        /// Continue a thread (a supervisor message always uses the conversation).
        #[arg(long)]
        thread: Option<Uuid>,
    },
    /// Read a mailbox page.
    Inbox {
        #[command(flatten)]
        to: To,
        #[arg(long, default_value = "0")]
        after: String,
        #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u8).range(1..=32))]
        limit: u8,
        #[arg(long)]
        thread: Option<Uuid>,
    },
    /// Read one message body, in a bounded chunk.
    Body {
        message: Uuid,
        #[command(flatten)]
        to: To,
        #[arg(long, default_value = "0")]
        offset: String,
        #[arg(long, default_value_t = 16384, value_parser = clap::value_parser!(u16).range(1..=16384))]
        max_bytes: u16,
    },
    /// Acknowledge a message as received (or `--read`).
    Ack {
        message: Uuid,
        #[command(flatten)]
        to: To,
        #[arg(long)]
        read: bool,
    },
    #[command(external_subcommand)]
    Other(Vec<String>),
}
/// The recipient; `identity` is the `whoami` of an agent (needed for task mailboxes).
pub fn recipient(to: &To, identity: Option<&Value>) -> Result<Value> {
    if let Some(raw) = &to.to_json {
        let v: Value = serde_json::from_str::<crate::json::UniqueJson>(raw)
            .map(|v| v.0)
            .map_err(|_| invalid("--to-json is a MailboxRecipient JSON object"))?;
        return v
            .is_object()
            .then_some(v)
            .ok_or_else(|| invalid("--to-json is a MailboxRecipient JSON object"));
    }
    let (kind, id) = to
        .to
        .split_once(':')
        .map_or((to.to.as_str(), None), |(k, i)| (k, Some(i)));
    let parse = |i: &str| {
        i.parse::<Uuid>()
            .map_err(|_| invalid("A mailbox ID is a UUID"))
    };
    let me =
        || {
            identity.filter(|v| v["principal"] == "agent").ok_or_else(|| {
            invalid("Owner mode addresses a mailbox with --to-json or supervisor:CONVERSATION_ID")
        })
        };
    match (kind, id) {
        ("supervisor", Some(c)) => {
            Ok(json!({"kind":"global_supervisor","conversation_id":parse(c)?}))
        }
        ("supervisor", None) => match me()?["supervisor_conversation_id"].as_str() {
            Some(c) => Ok(json!({"kind":"global_supervisor","conversation_id":parse(c)?})),
            None => Err(invalid(
                "No supervisor conversation is open; use `supervisor:CONVERSATION_ID` (the conversation of a supervisor message you received)",
            )),
        },
        ("session", None) => {
            let me = me()?;
            if !me["session_generation"].is_u64() {
                return Err(invalid(
                    "This server's whoami has no session_generation; address the session with --to-json",
                ));
            }
            Ok(
                json!({"kind":"session","workspace_id":me["workspace_id"],"project_id":me["project_id"],
                "task_id":me["task_id"],"session_id":me["session_id"],"attempt_id":me["attempt_id"],
                "generation":me["session_generation"]}),
            )
        }
        ("task", id) => {
            let me = me()?;
            let task = match id {
                Some(i) => json!(parse(i)?),
                None => me["task_id"].clone(),
            };
            Ok(
                json!({"kind":"task","workspace_id":me["workspace_id"],"project_id":me["project_id"],"task_id":task}),
            )
        }
        _ => Err(invalid(
            "--to is task, task:ID, session, supervisor or supervisor:CONVERSATION_ID",
        )),
    }
}
fn needs_identity(to: &To) -> bool {
    to.to_json.is_none()
        && (["task", "session", "supervisor"].contains(&to.to.as_str())
            || to.to.starts_with("task:"))
}
pub fn message_invocation(c: &Message, identity: Option<&Value>) -> Result<Option<Invocation>> {
    Ok(Some(match c {
        Message::Send { body, to, thread } => {
            let recipient = recipient(to, identity)?;
            let thread = match recipient["conversation_id"].as_str() {
                Some(c) => json!(c),
                None => json!(thread.unwrap_or_else(Uuid::new_v4)),
            };
            let p = obj(
                json!({"message_id":Uuid::new_v4(),"thread_id":thread,"recipient":recipient,"body":input::text(body)?}),
            );
            Invocation::mutation("message.send", p, true)
        }
        Message::Inbox {
            to,
            after,
            limit,
            thread,
        } => {
            let mut p =
                obj(json!({"recipient":recipient(to, identity)?,"after":after,"limit":limit}));
            put(&mut p, "thread_id", *thread);
            Invocation::read("message.inbox", p)
        }
        Message::Body {
            message,
            to,
            offset,
            max_bytes,
        } => Invocation::read(
            "message.body",
            obj(
                json!({"recipient":recipient(to, identity)?,"message_id":message,"offset":offset,"max_bytes":max_bytes}),
            ),
        ),
        Message::Ack { message, to, read } => Invocation::mutation(
            "message.ack",
            obj(
                json!({"recipient":recipient(to, identity)?,"message_id":message,
                "disposition":if *read {"read"} else {"received"}}),
            ),
            true,
        ),
        Message::Other(_) => return Ok(None),
    }))
}
pub async fn message(ctx: &Ctx, c: &Message) -> Result<Value> {
    let to = match c {
        Message::Send { to, .. }
        | Message::Inbox { to, .. }
        | Message::Body { to, .. }
        | Message::Ack { to, .. } => to,
        Message::Other(rest) => return other(ctx, "message", rest).await,
    };
    let identity = if ctx.mode == Mode::Channel && needs_identity(to) {
        Some(surface::whoami(ctx).await?)
    } else {
        None
    };
    let inv = message_invocation(c, identity.as_ref())?.expect("typed message");
    invoke(ctx, &inv).await
}

// ---- operation -------------------------------------------------------------

#[derive(Subcommand)]
pub enum Operation {
    /// Inspect an operation by its ID (read-only; never resends it).
    Show { id: Uuid },
    #[command(external_subcommand)]
    Other(Vec<String>),
}
pub async fn operation(ctx: &Ctx, c: &Operation) -> Result<Value> {
    match c {
        Operation::Show { id } => {
            invoke(
                ctx,
                &Invocation::read("operation.show", obj(json!({"operation_id":id}))),
            )
            .await
        }
        Operation::Other(rest) => other(ctx, "operation", rest).await,
    }
}
