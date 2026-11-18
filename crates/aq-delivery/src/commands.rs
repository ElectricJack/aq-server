//! The asynchronous delivery commands of host catalog 2.3.0 (Epic 0, spec 0016
//! section 6.2): `main.sync`, `branch.publish`, `pull_request.open`,
//! `pull_request.await_checks`, `pull_request.merge` and `pull_request.close`,
//! over the GitHub App adapter and aq-git. Compiled in only with the cargo
//! feature `delivery_playbook`.
//!
//! Code provides operations, facts and events; a playbook decides. Each
//! method makes the calls its command needs and reports a catalog outcome and
//! output as a [`Completion`]. Nothing here retries on its own, waits, or
//! chooses what a result is worth: a call that cannot be answered yet comes
//! back as [`Step::Retry`], and the caller (the host's operation loop) calls
//! it again with the same operation id, which aq-git and GitHub both treat as
//! the same operation. Time is a parameter (`now_ms`), never read here.
mod git;
mod host;

pub use git::{
    GitFailure, GitPort, RefDeletion, RefPublication, RetainedRef, SocketGit, TASK_BRANCH_PREFIX,
    object_id,
};
pub use host::{
    Accepting, due_operations, frozen_request, onboarded_target, pull_request_text, run_step,
};

use crate::github::GitHubApiError;
use crate::policy::{
    ChecksState, GitHubAdapter, GitHubRepository, MergeMethod, MergeOutcome, MergeRefusal,
    Mergeability, PullRequestState,
};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// The most names `failed` carries and the longest one, as the catalog bounds them.
const FAILED_NAMES: usize = 64;
const NAME_BYTES: usize = 256;

/// A repository in central aq-git (its binding's id and generation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTarget {
    pub repository_id: Uuid,
    pub binding_generation: u64,
}

/// Where a project's work is delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTarget {
    pub git: GitTarget,
    pub github: GitHubRepository,
    /// The project's default branch, a short name (`main`).
    pub default_branch: String,
}
impl DeliveryTarget {
    fn main_ref(&self) -> String {
        format!("refs/heads/{}", self.default_branch)
    }
}

/// A task's result branch in central aq-git and on the forge.
pub fn task_branch(task: Uuid) -> String {
    format!("{TASK_BRANCH_PREFIX}{task}")
}
fn short(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// The deadline of a check wait: what the playbook asked for, bounded by
/// Settings `delivery-limits.longest_check_wait_ms`.
pub fn bounded_wait_ms(requested_ms: u64, longest_check_wait_ms: u64) -> u64 {
    requested_ms.min(longest_check_wait_ms)
}

/// What the host knows of a task's pull request after a command: the row
/// behind `work.read`'s `pull_request` fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestObserved {
    pub number: u64,
    /// `None` when the command did not read it (the stored one stays).
    pub url: Option<String>,
    /// `open`, `closed` or `merged`.
    pub state: &'static str,
    /// `mergeable`, `conflicting` or `unknown`.
    pub mergeable: &'static str,
    /// `none`, `pending`, `passed` or `failed`.
    pub checks_state: &'static str,
    pub checks_failed: Vec<String>,
}

/// A command's one completion: a catalog outcome and its output.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub outcome: &'static str,
    pub output: Value,
    /// The pull request as this command left it, when it touched one.
    pub pull_request: Option<PullRequestObserved>,
}
impl Completion {
    pub fn new(outcome: &'static str, output: Value) -> Self {
        Self {
            outcome,
            output,
            pull_request: None,
        }
    }
    pub fn refused(reason: &'static str) -> Self {
        Self::new("refused", json!({"reason": reason}))
    }
    /// The one completion of a wait or a close that has nothing to wait for or close.
    pub fn closed() -> Self {
        Self::new("closed", json!({}))
    }
    fn with(mut self, pull_request: PullRequestObserved) -> Self {
        self.pull_request = Some(pull_request);
        self
    }
}

/// One look at a pull request for `pull_request.await_checks`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckPoll {
    /// The command's one completion, once something is decided or the deadline
    /// has passed; `None` means keep waiting.
    pub completion: Option<Completion>,
    /// The pull request as this look found it (`None` when it could not be read).
    pub observed: Option<PullRequestObserved>,
}

/// One attempt at a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The command's completion. It is final.
    Done(Completion),
    /// An effect of the command could not be finished yet (and nothing the
    /// playbook could be told about has happened). Call again, with the same
    /// operation id, later.
    Retry(String),
}

/// The commands, over a forge adapter and aq-git.
#[derive(Clone)]
pub struct Delivery {
    git: Arc<dyn GitPort>,
    github: Arc<dyn GitHubAdapter>,
}

impl Delivery {
    pub fn new(git: Arc<dyn GitPort>, github: Arc<dyn GitHubAdapter>) -> Self {
        Self { git, github }
    }

    /// `main.sync`: syncs the default branch from the forge into central
    /// aq-git, retains its head, and tells the head.
    pub async fn main_sync(&self, operation: Uuid, target: &DeliveryTarget) -> Step {
        Step::Done(
            match self
                .git
                .retain_ref(operation, &target.git, &target.main_ref())
                .await
            {
                Ok(retained) => Completion::new("synced", json!({"head": retained.head})),
                Err(_) => Completion::refused("unreachable"),
            },
        )
    }

    /// `branch.publish`: pushes `aq/<task>` from central aq-git to the forge
    /// under the same name. `expected_old` is the lease, the head the forge's
    /// branch was last published at (`None` when it was never).
    pub async fn branch_publish(
        &self,
        operation: Uuid,
        target: &DeliveryTarget,
        task: Uuid,
        expected_old: Option<&str>,
    ) -> Step {
        let published = self
            .git
            .publish_ref(operation, &target.git, &task_branch(task), expected_old)
            .await;
        Step::Done(match published {
            Ok(RefPublication::Published { head }) => {
                Completion::new("published", json!({"head": head}))
            }
            Ok(RefPublication::Moved) => Completion::new("conflict", json!({})),
            Ok(RefPublication::NoRef) => Completion::refused("no_branch"),
            Err(_) => Completion::refused("unreachable"),
        })
    }

    /// `pull_request.open`: opens the pull request of the task's branch
    /// against the default branch, or finds the one that is open.
    pub async fn pull_request_open(
        &self,
        target: &DeliveryTarget,
        task: Uuid,
        title: &str,
        body: &str,
    ) -> Step {
        let opened = self
            .github
            .open_or_find(
                &target.github,
                short(&task_branch(task)),
                &target.default_branch,
                title,
                body,
            )
            .await;
        Step::Done(match opened {
            Ok(pr) if pr.state == PullRequestState::Open => {
                Completion::new("opened", json!({"number": pr.number, "url": pr.url})).with(
                    PullRequestObserved {
                        number: pr.number,
                        url: Some(pr.url),
                        state: "open",
                        mergeable: "unknown",
                        checks_state: "none",
                        checks_failed: vec![],
                    },
                )
            }
            Ok(_) => Completion::refused("unreachable"),
            // GitHub answers 422 when the head branch is not there.
            Err(GitHubApiError::Invalid { .. }) => Completion::refused("not_published"),
            Err(_) => Completion::refused("unreachable"),
        })
    }

    /// One look at a pull request for `pull_request.await_checks`. The
    /// completion is one of `closed`, `conflict`, `failed`, `passed` or, once
    /// `now_ms` has reached `deadline_at_ms` with nothing decided, `timeout`;
    /// a decision that is made wins over a deadline that has also passed. A
    /// read that fails counts as nothing decided; the deadline still ends the
    /// wait.
    pub async fn await_checks_step(
        &self,
        target: &DeliveryTarget,
        pr: u64,
        required: Option<&[String]>,
        now_ms: i64,
        deadline_at_ms: i64,
    ) -> CheckPoll {
        let repo = &target.github;
        let observed = self.github.pull_request(repo, pr).await.ok();
        let mut seen = observed.as_ref().map(|p| PullRequestObserved {
            number: p.number,
            url: Some(p.url.clone()),
            state: match p.state {
                PullRequestState::Open => "open",
                PullRequestState::Closed => "closed",
                PullRequestState::Merged => "merged",
            },
            mergeable: "unknown",
            checks_state: "none",
            checks_failed: vec![],
        });
        let decided = 'decide: {
            let Some(p) = &observed else {
                break 'decide None;
            };
            if p.state != PullRequestState::Open {
                break 'decide Some(Completion::new("closed", json!({})));
            }
            if let Ok(Mergeability::Conflict) = self.github.mergeability(repo, pr).await {
                if let Some(s) = seen.as_mut() {
                    s.mergeable = "conflicting";
                }
                break 'decide Some(Completion::new("conflict", json!({})));
            }
            let Ok(checks) = self.github.checks(repo, pr, required).await else {
                break 'decide None;
            };
            if let Some(s) = seen.as_mut() {
                s.checks_failed = checks
                    .failed
                    .iter()
                    .take(FAILED_NAMES)
                    .map(|n| n.chars().take(NAME_BYTES).collect())
                    .collect();
                s.checks_state = match checks.state {
                    ChecksState::Passed => "passed",
                    ChecksState::Failed => "failed",
                    ChecksState::Pending => "pending",
                };
            }
            match checks.state {
                ChecksState::Failed => Some(Completion::new(
                    "failed",
                    json!({"names": seen.as_ref().map(|s| s.checks_failed.clone()).unwrap_or_default()}),
                )),
                ChecksState::Passed => Some(Completion::new("passed", json!({}))),
                ChecksState::Pending => None,
            }
        };
        let completion = decided
            .or_else(|| (now_ms >= deadline_at_ms).then(|| Completion::new("timeout", json!({}))));
        CheckPoll {
            completion,
            observed: seen,
        }
    }

    /// `pull_request.merge`: merges with `method` if the head is still
    /// `expected_head_sha`, then deletes the task's branch on the remote and in
    /// central aq-git. A merge that is already done is not done twice, so a
    /// retry after a failed deletion only deletes.
    #[allow(clippy::too_many_arguments)]
    pub async fn pull_request_merge(
        &self,
        operation: Uuid,
        target: &DeliveryTarget,
        task: Uuid,
        pr: u64,
        method: MergeMethod,
        expected_head_sha: &str,
    ) -> Step {
        let merged = self
            .github
            .merge(&target.github, pr, method, expected_head_sha)
            .await;
        let done = Step::Done;
        match merged {
            Ok(MergeOutcome::Merged { .. }) => {
                if let Err(e) = self
                    .git
                    .delete_ref(operation, &target.git, &task_branch(task))
                    .await
                {
                    return Step::Retry(format!("the merged branch is not deleted yet: {e}"));
                }
                done(
                    Completion::new("merged", json!({})).with(PullRequestObserved {
                        number: pr,
                        url: None,
                        state: "merged",
                        mergeable: "unknown",
                        checks_state: "passed",
                        checks_failed: vec![],
                    }),
                )
            }
            Ok(MergeOutcome::Conflict) => done(Completion::new("conflict", json!({}))),
            Ok(MergeOutcome::NotMergeable { .. }) => {
                done(Completion::new("not_mergeable", json!({})))
            }
            Ok(MergeOutcome::Refused { reason, .. }) => done(match reason {
                MergeRefusal::HeadMoved => Completion::refused("head_changed"),
                MergeRefusal::Closed => Completion::refused("no_pull_request"),
                MergeRefusal::Invalid => Completion::new("not_mergeable", json!({})),
            }),
            Err(GitHubApiError::NotFound) => done(Completion::refused("no_pull_request")),
            Err(_) => done(Completion::refused("unreachable")),
        }
    }

    /// `pull_request.close`: closes the pull request without merging it and
    /// deletes the task's branch on the remote and in central aq-git.
    pub async fn pull_request_close(
        &self,
        operation: Uuid,
        target: &DeliveryTarget,
        task: Uuid,
        pr: u64,
    ) -> Step {
        let closed = match self.github.close(&target.github, pr).await {
            Ok(p) => p,
            // No such pull request: nothing is open, which is what was asked.
            Err(GitHubApiError::NotFound) => crate::policy::PullRequest {
                number: pr,
                url: String::new(),
                head_sha: String::new(),
                state: PullRequestState::Closed,
            },
            Err(e) => return Step::Retry(format!("the forge did not close it: {}", e.code())),
        };
        if let Err(e) = self
            .git
            .delete_ref(operation, &target.git, &task_branch(task))
            .await
        {
            return Step::Retry(format!("the branch is not deleted yet: {e}"));
        }
        Step::Done(
            Completion::new("closed", json!({})).with(PullRequestObserved {
                number: pr,
                url: (!closed.url.is_empty()).then_some(closed.url),
                state: "closed",
                mergeable: "unknown",
                checks_state: "none",
                checks_failed: vec![],
            }),
        )
    }
}

/// One step of a running operation: its completion if it has one, and the
/// pull request as the step found it.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub step: Option<Step>,
    pub observed: Option<PullRequestObserved>,
}

/// What a command whose project is no longer bound answers: there is nothing
/// to deliver, to wait for or to close.
pub fn without_target(command: &str) -> Step {
    Step::Done(match command {
        "main.sync" => Completion::refused("unknown_project"),
        "pull_request.await_checks" | "pull_request.close" => Completion::closed(),
        _ => Completion::refused("unknown_task"),
    })
}

/// What a command answers when no forge or no central git is configured;
/// `None` keeps it waiting (a check wait ends at its deadline).
pub fn without_delivery(command: &str) -> Option<Step> {
    match command {
        "pull_request.await_checks" => None,
        "pull_request.close" => Some(Step::Retry("delivery is not configured".into())),
        _ => Some(Step::Done(Completion::refused("unreachable"))),
    }
}

impl Delivery {
    /// One step of `command` from the request the host froze when it accepted
    /// it: `task`, `pull_request`, `expected_old`, `title`, `body`, `method`,
    /// `expected_head_sha`, `required` and `deadline_at_ms`. `None` when the
    /// request does not fit the command (the host recorded it wrongly).
    /// `operation` is aq-git's idempotency key, so every step of one operation
    /// repeats the same effect rather than a new one.
    pub async fn step(
        &self,
        operation: Uuid,
        command: &str,
        request: &Value,
        target: &DeliveryTarget,
        now_ms: i64,
    ) -> Option<Run> {
        let task = request["task"]
            .as_str()
            .and_then(|t| Uuid::parse_str(t).ok());
        let pr = request["pull_request"].as_u64();
        let text = |field: &str| request[field].as_str();
        let done = |step: Step| {
            Some(Run {
                step: Some(step),
                observed: None,
            })
        };
        match (command, task) {
            ("main.sync", _) => done(self.main_sync(operation, target).await),
            ("branch.publish", Some(task)) => done(
                self.branch_publish(operation, target, task, text("expected_old"))
                    .await,
            ),
            ("pull_request.open", Some(task)) => done(
                self.pull_request_open(target, task, text("title")?, text("body")?)
                    .await,
            ),
            ("pull_request.merge", Some(task)) => {
                let method = match text("method")? {
                    "squash" => MergeMethod::Squash,
                    "rebase" => MergeMethod::Rebase,
                    _ => MergeMethod::Merge,
                };
                done(match pr {
                    Some(pr) => {
                        self.pull_request_merge(
                            operation,
                            target,
                            task,
                            pr,
                            method,
                            text("expected_head_sha")?,
                        )
                        .await
                    }
                    None => Step::Done(Completion::refused("no_pull_request")),
                })
            }
            ("pull_request.close", _) => done(match (task, pr) {
                (Some(task), Some(pr)) => {
                    self.pull_request_close(operation, target, task, pr).await
                }
                // No pull request is open for the task.
                _ => Step::Done(Completion::closed()),
            }),
            ("pull_request.await_checks", _) => {
                let Some(pr) = pr else {
                    return done(Step::Done(Completion::closed()));
                };
                let required: Option<Vec<String>> = request["required"]
                    .as_array()
                    .filter(|names| !names.is_empty())
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(|n| n.as_str().map(str::to_owned))
                            .collect()
                    });
                let deadline = request["deadline_at_ms"].as_i64()?;
                let poll = self
                    .await_checks_step(target, pr, required.as_deref(), now_ms, deadline)
                    .await;
                Some(Run {
                    step: poll.completion.map(Step::Done),
                    observed: poll.observed,
                })
            }
            _ => None,
        }
    }
}

/// `text` cut to at most `bytes` bytes at a character boundary.
pub fn bounded_text(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::bounded_text;

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(bounded_text("abc", 8), "abc");
        assert_eq!(bounded_text("abcdef", 3), "abc");
        // "é" is two bytes: a cut inside it backs up.
        assert_eq!(bounded_text("aé", 2), "a");
        assert_eq!(bounded_text("aé", 3), "aé");
    }
}
