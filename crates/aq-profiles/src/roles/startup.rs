//! The startup text of one attempt (spec 0014 §6.1): a fixed order, joined
//! the way the renderer joins instructions (`utf8_double_newline_v1`), and
//! refused when over the byte limit. Nothing is optional and nothing is
//! dropped to fit.
use super::Role;

/// The separator of the `utf8_double_newline_v1` rendering.
const SEPARATOR: &str = "\n\n";

/// The parts of a startup text, in the order they are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StartupPart {
    Platform,
    Role,
    Rules,
    ProjectNotes,
    RepositoryNotes,
    Task,
    TaskThread,
    Reporting,
}
impl StartupPart {
    pub fn label(self) -> &'static str {
        match self {
            Self::Platform => "platform text",
            Self::Role => "role",
            Self::Rules => "role rules",
            Self::ProjectNotes => "project notes",
            Self::RepositoryNotes => "repository notes",
            Self::Task => "task",
            Self::TaskThread => "task thread",
            Self::Reporting => "reporting contract",
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct StartupInput<'a> {
    /// What is true of every task in this environment. Owned by the server.
    pub platform: &'a str,
    pub role: &'a Role,
    /// The project's notes for this role, if it has any.
    pub project_notes: Option<&'a str>,
    pub repository_notes: Option<&'a str>,
    /// The task: title, body, acceptance criteria and its context.
    pub task: &'a str,
    /// The task's own thread (comments, questions and mail), when it has
    /// one: part of the task's context, delivered right after it.
    pub task_thread: Option<&'a str>,
    /// The result-reporting contract of the harness adapter; `None` for an
    /// adapter that has none.
    pub reporting: Option<&'a str>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupText {
    /// The instructions in delivery order.
    pub instructions: Vec<(StartupPart, String)>,
    /// The instructions joined by a blank line: the bytes the harness gets.
    pub rendered: String,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartupError {
    #[error("the {} is empty", .0.label())]
    Empty(StartupPart),
    #[error(
        "the startup text is {bytes} bytes, over the execution profile's limit of {limit} bytes"
    )]
    OverLimit { bytes: u64, limit: u64 },
}

/// Assemble the startup text. `limit_bytes` is the execution profile's
/// `startup_context_bytes` after the project's ceiling. The role's two
/// sections are delivered under the headings they have in the document.
pub fn assemble_startup(
    input: &StartupInput<'_>,
    limit_bytes: u64,
) -> Result<StartupText, StartupError> {
    let role = format!("## Role\n{}", input.role.role_md.trim());
    let rules = input.role.rules_md.trim();
    let rules = (!rules.is_empty()).then(|| format!("## Rules\n{rules}"));
    let required = [
        (StartupPart::Platform, input.platform),
        (StartupPart::Role, input.role.role_md.as_str()),
        (StartupPart::Task, input.task),
        (StartupPart::Reporting, input.reporting.unwrap_or("none")),
    ];
    if let Some((part, _)) = required.iter().find(|(_, text)| text.trim().is_empty()) {
        return Err(StartupError::Empty(*part));
    }
    let optional = |text: Option<&str>| text.filter(|t| !t.trim().is_empty()).map(String::from);
    let instructions: Vec<(StartupPart, String)> = [
        (StartupPart::Platform, Some(input.platform.to_string())),
        (StartupPart::Role, Some(role)),
        (StartupPart::Rules, rules),
        (StartupPart::ProjectNotes, optional(input.project_notes)),
        (
            StartupPart::RepositoryNotes,
            optional(input.repository_notes),
        ),
        (StartupPart::Task, Some(input.task.to_string())),
        (StartupPart::TaskThread, optional(input.task_thread)),
        (StartupPart::Reporting, input.reporting.map(String::from)),
    ]
    .into_iter()
    .filter_map(|(part, text)| text.map(|text| (part, text)))
    .collect();
    let rendered = instructions
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(SEPARATOR);
    let bytes = rendered.len() as u64;
    if bytes > limit_bytes {
        return Err(StartupError::OverLimit {
            bytes,
            limit: limit_bytes,
        });
    }
    Ok(StartupText {
        instructions,
        rendered,
    })
}
