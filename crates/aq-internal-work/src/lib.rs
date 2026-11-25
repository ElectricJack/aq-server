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
pub use resolve::{ObservedAt, Resolved, resolve};
pub use store::Row;

/// Largest `Request::instructions`, in bytes.
pub const MAX_INSTRUCTIONS_BYTES: usize = 256 * 1024;
/// Largest `Request::input`, in bytes.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Allowed `Request::deadline_ms`, counted from submission.
pub const DEADLINE_MS: std::ops::RangeInclusive<u64> = 1_000..=900_000;
/// Allowed `Request::deadline_ms` of agent work ([`Request::agent`]).
pub const AGENT_DEADLINE_MS: std::ops::RangeInclusive<u64> = 1_000..=1_800_000;
/// Allowed [`AgentWork::max_turns`].
pub const AGENT_MAX_TURNS: std::ops::RangeInclusive<u32> = 1..=400;
/// Most files [`AgentWork::files`] may hold.
pub const AGENT_MAX_FILES: usize = 256;
/// Most bytes of [`AgentWork::files`], paths and contents together.
pub const AGENT_FILES_BYTES: usize = 4 * 1024 * 1024;
/// The file in the workspace an agent's answer is read from; it is the raw
/// answer `validate::answer` checks, at most `Request::max_output_bytes`.
pub const AGENT_OUTCOME: &str = "out/outcome.json";
/// The agent's own temporary directory in its workspace, made by the host;
/// no request file may be under it.
pub const AGENT_TMP: &str = ".tmp";
/// What an agent candidate leaves for each later candidate of its task, and
/// the smallest agent candidate budget unless less time is left.
pub const AGENT_FALLBACK_RESERVE_MS: i64 = 300_000;
/// Most bytes of one candidate's stored transcript (its JSON array).
pub const TRANSCRIPT_BYTES: usize = 262_144;
/// Most bytes of one transcript item (its JSON object).
pub const TRANSCRIPT_ITEM_BYTES: usize = 2048;
/// Allowed `Request::max_output_bytes`.
pub const MAX_OUTPUT_BYTES: std::ops::RangeInclusive<u64> = 1..=1_048_576;
/// Largest outcome file agent work may leave (the engine's outcome bound).
pub const AGENT_MAX_OUTPUT_BYTES: std::ops::RangeInclusive<u64> = 1..=4_194_304;
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
    let share = remaining / candidates_left(candidate_index, candidate_count);
    remaining.min(share.max(MIN_CANDIDATE_BUDGET_MS)).max(0)
}

/// [`candidate_budget_ms`] for agent work: the time left to the task
/// deadline less [`AGENT_FALLBACK_RESERVE_MS`] for each later candidate, at
/// least [`AGENT_FALLBACK_RESERVE_MS`], and never more than is left. So a
/// fallback still gets five minutes. [`store::sweep`] computes the same in SQL.
pub fn agent_candidate_budget_ms(
    deadline_at_ms: i64,
    claimed_at_ms: i64,
    candidate_index: u32,
    candidate_count: usize,
) -> i64 {
    let later = candidates_left(candidate_index, candidate_count) - 1;
    let reserve = later.saturating_mul(AGENT_FALLBACK_RESERVE_MS);
    let remaining = deadline_at_ms.saturating_sub(claimed_at_ms);
    let available = remaining
        .saturating_sub(reserve)
        .max(AGENT_FALLBACK_RESERVE_MS);
    available.min(remaining).max(0)
}

fn candidates_left(index: u32, count: usize) -> i64 {
    i64::try_from(count)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::from(index))
        .max(1)
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
    /// The item in words, from its kind and id alone: "Playbook source
    /// routing", "Playbook operation <id>".
    pub fn describe(kind: &str, id: &str) -> String {
        let words = kind.replace('_', " ");
        let mut chars = words.chars();
        let mut text: String = chars
            .next()
            .into_iter()
            .flat_map(char::to_uppercase)
            .collect();
        text.push_str(chars.as_str());
        format!("{text} {id}")
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
    /// Run as an agent with tools in a workspace holding these files, until
    /// it writes [`AGENT_OUTCOME`]; `None` is one prompt with tools off.
    #[serde(default)]
    pub agent: Option<AgentWork>,
}

/// Work done by an agent in a scratch workspace (spec 0013 §4.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentWork {
    /// Workspace files by relative path (`/`-separated, no `.` or `..`
    /// segment), at most [`AGENT_MAX_FILES`] of at most [`AGENT_FILES_BYTES`].
    pub files: BTreeMap<String, String>,
    /// Within [`AGENT_MAX_TURNS`].
    pub max_turns: u32,
}

impl AgentWork {
    fn validate(&self) -> Result<()> {
        if !AGENT_MAX_TURNS.contains(&self.max_turns) {
            return Err(invalid("agent max_turns must be between 1 and 400"));
        }
        if self.files.len() > AGENT_MAX_FILES {
            return Err(invalid("agent work has more than 256 files"));
        }
        let bytes: usize = self.files.iter().map(|(p, c)| p.len() + c.len()).sum();
        if bytes > AGENT_FILES_BYTES {
            return Err(invalid("agent files exceed 4 MiB"));
        }
        self.files
            .keys()
            .try_for_each(|path| self.validate_path(path))
    }

    fn validate_path(&self, path: &str) -> Result<()> {
        let reserved = path == AGENT_OUTCOME || path.split('/').next() == Some(AGENT_TMP);
        if reserved || !workspace_path(path) {
            return Err(invalid("agent file paths must be relative workspace paths"));
        }
        let mut ancestors = path.match_indices('/').map(|(at, _)| &path[..at]);
        if ancestors.any(|dir| self.files.contains_key(dir)) {
            return Err(invalid("an agent file path is also a directory"));
        }
        Ok(())
    }
}

/// Whether `path` names a file inside a workspace: at most 4096 bytes of
/// relative, `/`-separated segments of at most 255 bytes that are neither
/// empty, `.` nor `..`, and no NUL.
pub fn workspace_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| segment.len() <= 255 && !matches!(segment, "" | "." | ".."))
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
        let deadlines = match &self.agent {
            Some(_) => AGENT_DEADLINE_MS,
            None => DEADLINE_MS,
        };
        if !deadlines.contains(&self.deadline_ms) {
            let (low, high) = deadlines.into_inner();
            return Err(Error::Invalid(format!(
                "deadline_ms must be between {low} and {high}"
            )));
        }
        let outputs = match &self.agent {
            Some(_) => AGENT_MAX_OUTPUT_BYTES,
            None => MAX_OUTPUT_BYTES,
        };
        if !outputs.contains(&self.max_output_bytes) {
            return Err(Error::Invalid(format!(
                "max_output_bytes must be between 1 and {}",
                outputs.end()
            )));
        }
        match &self.agent {
            Some(agent) => agent.validate(),
            None => Ok(()),
        }
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
    /// The reasoning effort the level document states, such as `low`; `None`
    /// (the document's `default`) leaves the harness its own setting.
    #[serde(default)]
    pub reasoning: Option<String>,
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
    /// This attempt's [`candidate_budget_ms`], or [`agent_candidate_budget_ms`]
    /// for agent work.
    pub fn budget_ms(&self) -> i64 {
        let (index, count) = (self.candidate_index, self.candidate_count);
        let budget = match self.request.agent {
            Some(_) => agent_candidate_budget_ms,
            None => candidate_budget_ms,
        };
        budget(self.deadline_at_ms, self.claimed_at_ms, index, count)
    }

    /// When the runner stops this attempt and reports it, in Unix
    /// milliseconds: [`runner_budget_ms`] after the claim.
    pub fn stop_at_ms(&self) -> i64 {
        self.claimed_at_ms + runner_budget_ms(self.budget_ms())
    }

    /// The failure detail of an attempt stopped at [`Claimed::stop_at_ms`]:
    /// "`who` did not answer within N s", or "did not finish" for agent work.
    pub fn unanswered(&self, who: &str) -> String {
        let waited_ms = runner_budget_ms(self.budget_ms()).max(0);
        let verb = match self.request.agent {
            Some(_) => "finish",
            None => "answer",
        };
        format!("{who} did not {verb} within {} s", (waited_ms + 999) / 1000)
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
    fn agent_budget_keeps_five_minutes_for_each_later_candidate() {
        for (remaining, index, count, budget) in [
            (1_800_000, 0, 2, 1_500_000),
            (400_000, 0, 2, 300_000),
            // The last candidate gets everything left.
            (1_500_000, 1, 2, 1_500_000),
            (1_800_000, 0, 3, 1_200_000),
            (1_800_000, 0, 1, 1_800_000),
            // The floor, but never more than is left.
            (200_000, 0, 2, 200_000),
            (0, 0, 2, 0),
            (-10_000, 1, 2, 0),
        ] {
            let claimed_at_ms = 1_000_000;
            let deadline_at_ms = claimed_at_ms + remaining;
            assert_eq!(
                agent_candidate_budget_ms(deadline_at_ms, claimed_at_ms, index, count),
                budget,
                "remaining {remaining}, candidate {index} of {count}"
            );
        }
    }

    fn agent_request(files: &[(&str, &str)], max_turns: u32, deadline_ms: u64) -> Request {
        Request {
            purpose: Purpose::PlaybookCompilation,
            instructions: "Write the playbook.".into(),
            input: String::new(),
            answer: AnswerShape::Json {
                schema: serde_json::json!({"type": "object"}),
            },
            blocking: false,
            affected: Affected {
                kind: "playbook_source".into(),
                id: "routing".into(),
                project_id: None,
            },
            deadline_ms,
            max_output_bytes: 1_048_576,
            escalate: true,
            profile_override: None,
            agent: Some(AgentWork {
                files: files
                    .iter()
                    .map(|(path, text)| (path.to_string(), text.to_string()))
                    .collect(),
                max_turns,
            }),
        }
    }

    #[test]
    fn agent_work_allows_thirty_minutes_and_bounds_its_files() {
        let ok = agent_request(
            &[("doc.md", "# Doc"), ("guide/language.md", "x")],
            200,
            1_800_000,
        );
        assert!(ok.validate().is_ok());
        let answer_only = Request {
            agent: None,
            ..ok.clone()
        };
        assert!(answer_only.validate().is_err(), "900 s is the answer limit");
        for (bad, why) in [
            (agent_request(&[], 200, 1_800_001), "deadline"),
            (agent_request(&[], 0, 60_000), "no turns"),
            (agent_request(&[], 401, 60_000), "too many turns"),
            (agent_request(&[("/etc/passwd", "")], 1, 60_000), "absolute"),
            (agent_request(&[("a/../../b", "")], 1, 60_000), "dot dot"),
            (agent_request(&[("./a", "")], 1, 60_000), "dot"),
            (agent_request(&[("a//b", "")], 1, 60_000), "empty segment"),
            (agent_request(&[("", "")], 1, 60_000), "empty"),
            (agent_request(&[("a\0b", "")], 1, 60_000), "NUL"),
            (
                agent_request(&[(AGENT_OUTCOME, "{}")], 1, 60_000),
                "outcome",
            ),
            (
                agent_request(&[("a", ""), ("a/b", "")], 1, 60_000),
                "file and dir",
            ),
            (
                agent_request(&[(".tmp/x", "")], 1, 60_000),
                "the agent's temp",
            ),
            (
                agent_request(&[(&"s".repeat(256), "")], 1, 60_000),
                "long segment",
            ),
            (
                agent_request(&[(&format!("{}s", "s/".repeat(2048)), "")], 1, 60_000),
                "long path",
            ),
        ] {
            assert!(bad.validate().is_err(), "{why}");
        }
        assert!(
            agent_request(&[(&"s".repeat(255), "")], 1, 60_000)
                .validate()
                .is_ok()
        );
        let big = "x".repeat(AGENT_FILES_BYTES);
        assert!(
            agent_request(&[("big", &big)], 1, 60_000)
                .validate()
                .is_err()
        );
        let many: Vec<(String, String)> = (0..=AGENT_MAX_FILES)
            .map(|i| (format!("f{i}"), String::new()))
            .collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        assert!(agent_request(&many, 1, 60_000).validate().is_err());
        assert!(agent_request(&many[1..], 1, 60_000).validate().is_ok());
    }

    #[test]
    fn a_stored_request_without_agent_work_still_decodes() {
        let mut stored = serde_json::to_value(agent_request(&[], 1, 60_000)).unwrap();
        stored.as_object_mut().unwrap().remove("agent");
        let request: Request = serde_json::from_value(stored).unwrap();
        assert!(request.agent.is_none());
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
