//! Internal work (spec 0012): one way for AQ components to ask a model.
//!
//! A caller describes a [`Request`]; the server keeps it as an internal task
//! (`internal_tasks`), outside the graph and outside project capacity, and
//! tries the ordered [`Candidate`] list until one answers or the task fails.
pub mod api_backend;
pub mod http;
pub mod resolve;
pub mod settings;
pub mod store;
pub mod validate;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use api_backend::{ApiBackend, ApiEndpoint, InternalWorkConfig};
pub use resolve::{Resolved, resolve};
pub use store::Row;

/// Largest `Request::instructions`, in bytes.
pub const MAX_INSTRUCTIONS_BYTES: usize = 256 * 1024;
/// Largest `Request::input`, in bytes.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Allowed `Request::deadline_ms`, counted from submission.
pub const DEADLINE_MS: std::ops::RangeInclusive<u64> = 1_000..=900_000;
/// Allowed `Request::max_output_bytes`.
pub const MAX_OUTPUT_BYTES: std::ops::RangeInclusive<u64> = 1..=1_048_576;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("database")]
    Database(#[from] sqlx::Error),
    /// A stored row no longer decodes into the types of this crate.
    #[error("stored internal task is unreadable")]
    Corrupt,
}
pub type Result<T> = std::result::Result<T, Error>;

/// What the work is for. A fixed list, so a typo is an error rather than a
/// silent fall-through to the default profile (spec 0012 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    RoutingClassification,
    PlaybookCompilation,
    PlaybookCall,
    DecisionRevision,
    ResultProcessing,
}

impl Purpose {
    pub const ALL: [Purpose; 5] = [
        Purpose::RoutingClassification,
        Purpose::PlaybookCompilation,
        Purpose::PlaybookCall,
        Purpose::DecisionRevision,
        Purpose::ResultProcessing,
    ];
}

/// The structure the answer must have.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerShape {
    Choice {
        choices: Vec<String>,
        rationale_max_bytes: u64,
    },
    Json {
        schema: serde_json::Value,
    },
}

/// The item the work is about: a task, a policy, a playbook run, a decision
/// or a result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Affected {
    pub kind: String,
    pub id: String,
    pub project_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub purpose: Purpose,
    /// At most [`MAX_INSTRUCTIONS_BYTES`].
    pub instructions: String,
    /// At most [`MAX_INPUT_BYTES`].
    pub input: String,
    pub answer: AnswerShape,
    /// Something is waiting on the answer.
    pub blocking: bool,
    pub affected: Affected,
    /// From submission; within [`DEADLINE_MS`].
    pub deadline_ms: u64,
    /// Within [`MAX_OUTPUT_BYTES`].
    pub max_output_bytes: u64,
    /// False when the caller has its own safe default (spec 0012 §8 step 1).
    pub escalate: bool,
    /// Intelligence profile id; `None` uses Settings.
    pub profile_override: Option<String>,
}

impl Request {
    /// Enforces the size and time bounds of the fields above.
    pub fn validate(&self) -> Result<()> {
        if self.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(invalid("instructions exceed 256 KiB"));
        }
        if self.input.len() > MAX_INPUT_BYTES {
            return Err(invalid("input exceeds 1 MiB"));
        }
        if !DEADLINE_MS.contains(&self.deadline_ms) {
            return Err(invalid("deadline_ms must be between 1000 and 900000"));
        }
        if !MAX_OUTPUT_BYTES.contains(&self.max_output_bytes) {
            return Err(invalid("max_output_bytes must be between 1 and 1048576"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.into())
}

/// Why a candidate or a task failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    ProviderError,
    ProviderUnavailable,
    DeadlineExceeded,
    BudgetExceeded,
    InvalidOutput,
    NoCandidate,
}

/// One candidate's run of a task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub provider: String,
    pub model: String,
    pub way: Way,
    /// `None` for the attempt that answered.
    pub reason: Option<Reason>,
    pub detail: String,
    pub elapsed_ms: u64,
}

/// How a candidate is served: one API request, or a logged-in harness on the
/// server (spec 0012 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Way {
    Api,
    Login,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Answered {
        answer: serde_json::Value,
        provider: String,
        model: String,
        way: Way,
        elapsed_ms: u64,
        usage: Option<Usage>,
        attempts: Vec<Attempt>,
    },
    Failed {
        reason: Reason,
        detail: String,
        attempts: Vec<Attempt>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One entry of the resolved profile, in fallback order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    /// The logged-in harness for [`Way::Login`].
    pub harness: Option<String>,
    pub way: Way,
    pub credential_index: usize,
}

/// A task a runner now holds for its current candidate.
pub struct Claimed {
    pub id: Uuid,
    pub request: Request,
    pub candidate: Candidate,
    pub candidate_index: u32,
    /// The task deadline: submission time plus `Request::deadline_ms`.
    pub deadline_at_ms: i64,
}
