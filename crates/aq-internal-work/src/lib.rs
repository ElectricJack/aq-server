//! Internal work (spec 0012): one way for AQ components to ask a model.
//!
//! A caller describes a [`Request`]; the server keeps it as an internal task
//! (`internal_tasks`), outside the graph and outside project capacity, and
//! tries the ordered [`Candidate`] list until one answers or the task fails.
pub mod api_backend;
pub mod http;
mod notice;
pub mod resolve;
pub mod runs;
pub mod service;
pub mod settings;
pub mod store;
pub mod validate;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
/// The smallest candidate budget, unless less time is left to the deadline.
pub const MIN_CANDIDATE_BUDGET_MS: i64 = 30_000;
/// The most a runner stops a candidate before the end of its budget, so the
/// attempt is recorded within the budget. The sweep waits [`SWEEP_GRACE_MS`]
/// past the full budget, so it only catches a runner that died.
pub const RUNNER_MARGIN_MS: i64 = 1_000;
/// The failure detail of a candidate claimed with no time left.
pub const DEADLINE_PASSED: &str = "the task deadline passed before the run";
/// How long past its candidate budget a runner may take to report before the
/// sweeper takes the candidate back.
pub const SWEEP_GRACE_MS: i64 = 5_000;

/// How long candidate `candidate_index` of `candidate_count` may run when it
/// was claimed at `claimed_at_ms`: the time left to the task deadline shared
/// evenly among the candidates not yet tried (this one and those after it),
/// at least [`MIN_CANDIDATE_BUDGET_MS`], and never more than is left. The one
/// definition of the budget: runners stop at [`runner_budget_ms`] of it, and
/// [`store::sweep`] computes the same in SQL.
pub fn candidate_budget_ms(
    deadline_at_ms: i64,
    claimed_at_ms: i64,
    candidate_index: u32,
    candidate_count: usize,
) -> i64 {
    let remaining = deadline_at_ms.saturating_sub(claimed_at_ms);
    let count = i64::try_from(candidate_count).unwrap_or(i64::MAX);
    let left = count.saturating_sub(i64::from(candidate_index)).max(1);
    remaining
        .min(MIN_CANDIDATE_BUDGET_MS.max(remaining / left))
        .max(0)
}

/// How long after the claim a runner stops a candidate with `budget_ms` and
/// reports it: at the end of the budget, less [`RUNNER_MARGIN_MS`] or a
/// tenth of the budget, whichever is smaller.
pub fn runner_budget_ms(budget_ms: i64) -> i64 {
    budget_ms - RUNNER_MARGIN_MS.min(budget_ms / 10)
}
/// A purpose with this many failed runs within [`FAILURE_BURST_WINDOW_MS`]
/// goes to a supervisor even when no run asked to escalate (spec 0012 §8).
pub const FAILURE_BURST_RUNS: i64 = 5;
/// The window for [`FAILURE_BURST_RUNS`], and how often one purpose's burst
/// may reach a supervisor.
pub const FAILURE_BURST_WINDOW_MS: i64 = 10 * 60_000;
/// Retries a supervisor gets at one failure; a run carrying this many that
/// fails again goes straight to the owner.
pub const SUPERVISOR_ATTEMPTS: i32 = 3;
/// How long a run waits in the supervisor's hands before it goes to the owner.
pub const SUPERVISOR_SILENCE_MS: i64 = 30 * 60_000;
/// Largest diagnosis a supervisor may give when it escalates, in bytes.
pub const MAX_DIAGNOSIS_BYTES: usize = 4000;

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

impl Affected {
    /// The item in words, from its kind and id alone: "Routing policy,
    /// revision 7", "Playbook operation <id>".
    pub fn describe(kind: &str, id: &str) -> String {
        let words = kind.replace('_', " ");
        let mut chars = words.chars();
        let mut text: String = chars
            .next()
            .into_iter()
            .flat_map(char::to_uppercase)
            .collect();
        text.push_str(chars.as_str());
        match kind {
            "routing_policy" => format!("{text}, revision {id}"),
            _ => format!("{text} {id}"),
        }
    }
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
    /// A candidate passed over because its login was busy with another run
    /// on the host, so that the next candidate could run. Says nothing about
    /// the provider; only an attempt carries it, never a failed task.
    LoginBusy,
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
    /// The reasoning effort of the model choice the profile names, such as
    /// `low`; `None` leaves the harness its default. Never the choice's
    /// display label or the reference itself.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// The catalog's `reasoning_configuration_reference` the effort and
    /// options were resolved from, for the record.
    #[serde(default)]
    pub reasoning_reference: String,
    /// The model choice's provider options, such as a Codex `service_tier`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, serde_json::Value>,
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
    /// When this candidate's attempt began; its budget counts from here.
    pub claimed_at_ms: i64,
    /// How many candidates the task has.
    pub candidate_count: usize,
}

impl Claimed {
    /// This attempt's [`candidate_budget_ms`].
    pub fn budget_ms(&self) -> i64 {
        let (index, count) = (self.candidate_index, self.candidate_count);
        candidate_budget_ms(self.deadline_at_ms, self.claimed_at_ms, index, count)
    }

    /// When the runner stops this attempt and reports it, in Unix
    /// milliseconds: [`runner_budget_ms`] after the claim.
    pub fn stop_at_ms(&self) -> i64 {
        self.claimed_at_ms + runner_budget_ms(self.budget_ms())
    }

    /// The failure detail of an attempt stopped at [`Claimed::stop_at_ms`]:
    /// "`who` did not answer within N s".
    pub fn unanswered(&self, who: &str) -> String {
        let waited_ms = runner_budget_ms(self.budget_ms()).max(0);
        format!("{who} did not answer within {} s", (waited_ms + 999) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_candidate_gets_an_even_share_of_what_is_left_with_a_floor() {
        for (remaining, index, count, budget) in [
            // A 600 s compile with two candidates: half each.
            (600_000, 0, 2, 300_000),
            // The first used all of its share: the second gets what is left.
            (295_000, 1, 2, 295_000),
            // A 120 s request with two candidates: about 60 s each.
            (120_000, 0, 2, 60_000),
            (60_000, 1, 2, 60_000),
            // A single candidate gets the whole deadline.
            (600_000, 0, 1, 600_000),
            (100_000, 1, 3, 50_000),
            (100_001, 0, 2, 50_000),
            // The floor, but never more than is left.
            (60_000, 0, 3, 30_000),
            (20_000, 0, 3, 20_000),
            (7, 0, 2, 7),
            // The last candidate gets everything left.
            (450_000, 3, 4, 450_000),
            // Nothing is left.
            (0, 0, 2, 0),
            (-10_000, 1, 2, 0),
        ] {
            let claimed_at_ms = 1_000_000;
            let deadline_at_ms = claimed_at_ms + remaining;
            assert_eq!(
                candidate_budget_ms(deadline_at_ms, claimed_at_ms, index, count),
                budget,
                "remaining {remaining}, candidate {index} of {count}"
            );
        }
    }

    #[test]
    fn a_runner_stops_at_most_a_second_and_at_most_a_tenth_before_the_budget() {
        assert_eq!(RUNNER_MARGIN_MS, 1_000);
        for (budget, runner) in [
            (300_000, 299_000),
            (30_000, 29_000),
            (10_000, 9_000),
            (6_000, 5_400),
            (1_000, 900),
            (9, 9),
            (0, 0),
        ] {
            assert_eq!(runner_budget_ms(budget), runner, "budget {budget}");
        }
    }
}
