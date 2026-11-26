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
mod checks;
mod run;
use checks::Refresh;
pub use checks::{CheckRequest, Refreshed};
pub use run::FrozenCommand;
mod git;
mod host;

pub use git::{
    GitFailure, GitPort, RefDeletion, RefPublication, RetainedRef, SocketGit, TASK_BRANCH_PREFIX,
    TRY_PREFIX, object_id,
};
pub use host::{
    Accepting, DeliveryRuntime, DueOperations, accepted_head, due_operations, frozen_request,
    onboarded_target, published_head, pull_request_text, run_step,
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GitTarget {
    pub repository_id: Uuid,
    pub binding_generation: u64,
}

/// Where a project's work is delivered.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    /// The redacted log tail of the failed checks on the head that was
    /// checked, when the forge gave one.
    pub checks_log: Option<CheckLog>,
}

/// The tail of the failed jobs' logs (spec 0021 section 5.3) and the pull
/// request head it was read for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLog {
    pub head: String,
    pub tail: String,
}

/// A command's one completion: a catalog outcome and its output.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub outcome: &'static str,
    pub output: Value,
    /// The pull request as this command left it, when it touched one.
    pub pull_request: Option<PullRequestObserved>,
    /// Diagnostic retained with the operation, outside the versioned catalog response.
    pub diagnostic: Option<&'static str>,
}
impl Completion {
    pub fn new(outcome: &'static str, output: Value) -> Self {
        Self {
            outcome,
            output,
            pull_request: None,
            diagnostic: None,
        }
    }
    fn behind_base() -> Self {
        Self {
            diagnostic: Some("behind_base"),
            ..Self::new("conflict", json!({}))
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
    /// A branch refresh this look asked for or found done.
    pub refreshed: Option<Refreshed>,
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

enum MergeAttempt {
    Confirmed(String),
    Unconfirmed(Step),
}

/// The task branch and the idempotency key for one operation on it.
#[derive(Clone, Copy)]
pub struct TaskOperation<'a> {
    pub operation: Uuid,
    pub target: &'a DeliveryTarget,
    pub task: Uuid,
}
impl<'a> TaskOperation<'a> {
    pub fn new(operation: Uuid, target: &'a DeliveryTarget, task: Uuid) -> Self {
        Self {
            operation,
            target,
            task,
        }
    }
}
pub struct OpenPullRequest<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub known_pr: Option<u64>,
}
#[derive(Clone, Copy)]
pub struct MergeGuard<'a> {
    pub method: MergeMethod,
    /// The pull request head the merge is pinned to.
    pub expected_head: &'a str,
    /// The result head, when a recorded refresh moved the pull request past
    /// it: the central branch is still there.
    pub result_head: Option<&'a str>,
}
/// The commands, over a forge adapter and aq-git.
#[derive(Clone)]
pub struct Delivery {
    git: Arc<dyn GitPort>,
    github: Arc<dyn GitHubAdapter>,
}

impl Delivery {
    /// Confirm the exact result is on the forge or the existing publish lease is usable.
    pub async fn publish_lease_usable(
        &self,
        target: &DeliveryTarget,
        task: Uuid,
        head: &str,
        expected_old: Option<&str>,
    ) -> bool {
        match self
            .github
            .branch_head(&target.github, &task_branch(task))
            .await
        {
            Ok(Some(observed)) => observed == head || expected_old == Some(observed.as_str()),
            Ok(None) => expected_old.is_none(),
            Err(_) => false,
        }
    }

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
        branch: TaskOperation<'_>,
        expected_old: Option<&str>,
        expected_source: Option<&str>,
    ) -> Step {
        let TaskOperation {
            operation,
            target,
            task,
        } = branch;
        let published = self
            .git
            .publish_ref(
                operation,
                &target.git,
                &task_branch(task),
                expected_old,
                expected_source,
            )
            .await;
        Step::Done(match published {
            Ok(RefPublication::Published { head }) => {
                Completion::new("published", json!({"head": head}))
            }
            Ok(RefPublication::Moved) => Completion::new("conflict", json!({})),
            Ok(RefPublication::NoRef) => Completion::refused("no_branch"),
            Err(e) if e.code == "rejected" && e.message == "remote_rejected" => Completion {
                diagnostic: Some("remote_rejected"),
                ..Completion::refused("unreachable")
            },
            Err(_) => Completion::refused("unreachable"),
        })
    }

    /// `pull_request.open`: opens the pull request of the task's branch
    /// against the default branch, or finds the one that is open.
    pub async fn pull_request_open(
        &self,
        target: &DeliveryTarget,
        task: Uuid,
        request: OpenPullRequest<'_>,
    ) -> Step {
        let OpenPullRequest {
            title,
            body,
            known_pr,
        } = request;
        let opened = match known_pr {
            Some(pr) => self.github.pull_request(&target.github, pr).await,
            None => {
                self.github
                    .open_or_find(
                        &target.github,
                        short(&task_branch(task)),
                        &target.default_branch,
                        title,
                        body,
                    )
                    .await
            }
        };
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
                        checks_log: None,
                    },
                )
            }
            Ok(pr) => Completion::refused("unreachable").with(PullRequestObserved {
                number: pr.number,
                url: Some(pr.url),
                state: if pr.state == PullRequestState::Merged {
                    "merged"
                } else {
                    "closed"
                },
                mergeable: "unknown",
                checks_state: "none",
                checks_failed: vec![],
                checks_log: None,
            }),
            // GitHub answers 422 when the head branch is not there.
            Err(GitHubApiError::Invalid { .. }) => Completion::refused("not_published"),
            Err(_) => Completion::refused("unreachable"),
        })
    }

    /// Read the forge PR and verify its exact revision and destination.
    pub async fn pull_request_at(
        &self,
        target: &DeliveryTarget,
        number: u64,
        head: &str,
    ) -> crate::github::GitHubResult<Option<crate::policy::PullRequest>> {
        let pr = self.github.pull_request(&target.github, number).await?;
        Ok(
            (pr.number == number && pr.head_sha == head && pr.base_branch == target.default_branch)
                .then_some(pr),
        )
    }

    /// Read external merge evidence without issuing a merge request.
    pub async fn verified_external_merge(
        &self,
        target: &DeliveryTarget,
        number: u64,
        head: &str,
    ) -> crate::github::GitHubResult<Option<crate::policy::PullRequest>> {
        Ok(self
            .pull_request_at(target, number, head)
            .await?
            .filter(|pr| pr.state == PullRequestState::Merged))
    }

    /// `pull_request.merge`: merges with `method` if the head is still
    /// `expected_head_sha`, then deletes the task's branch on the remote and in
    /// central aq-git. A merge that is already done is not done twice, so a
    /// retry after a failed deletion only deletes.
    pub async fn pull_request_merge(
        &self,
        scope: TaskOperation<'_>,
        pr: u64,
        guard: MergeGuard<'_>,
    ) -> Step {
        if let MergeAttempt::Unconfirmed(step) = self.merge_on_github(scope.target, pr, guard).await
        {
            return step;
        }
        let central = guard.result_head.unwrap_or(guard.expected_head);
        self.finish_merge(scope, pr, (central, guard.expected_head))
            .await
    }

    async fn merge_on_github(
        &self,
        target: &DeliveryTarget,
        pr: u64,
        guard: MergeGuard<'_>,
    ) -> MergeAttempt {
        let MergeGuard {
            method,
            expected_head,
            ..
        } = guard;
        let done = |completion| MergeAttempt::Unconfirmed(Step::Done(completion));
        let merged = self
            .github
            .merge(
                &target.github,
                pr,
                method,
                expected_head,
                &target.default_branch,
            )
            .await;
        match merged {
            Ok(MergeOutcome::Merged { sha })
                if object_id(&sha) && sha.bytes().any(|b| b != b'0') =>
            {
                MergeAttempt::Confirmed(sha)
            }
            Ok(MergeOutcome::Merged { .. }) => MergeAttempt::Unconfirmed(Step::Retry(
                "GitHub did not report a valid merge SHA".into(),
            )),
            Ok(MergeOutcome::Behind) => done(Completion::behind_base()),
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
            // On replay the adapter reads the PR and checks its head before any PUT.
            Err(
                error @ (GitHubApiError::Transport(_)
                | GitHubApiError::Protocol(_)
                | GitHubApiError::Status {
                    status: 500..=599, ..
                }),
            ) => MergeAttempt::Unconfirmed(Step::Retry(format!(
                "recheck the pull request: {}",
                error.code()
            ))),
            Err(_) => done(Completion::refused("unreachable")),
        }
    }

    /// Deletes the task branch, in central aq-git under the lease `central`
    /// and on the forge under `forge`; they differ after a branch refresh.
    async fn delete_task_branch(
        &self,
        scope: TaskOperation<'_>,
        (central, forge): (Option<&str>, Option<&str>),
    ) -> Result<RefDeletion, GitFailure> {
        let (repo, branch) = (&scope.target.git, task_branch(scope.task));
        if central != forge {
            // An absent central ref holds any lease, so a replay passes this.
            self.git
                .delete_central_ref(Uuid::new_v4(), repo, &branch, central)
                .await?;
        }
        self.git
            .delete_ref(scope.operation, repo, &branch, forge)
            .await
    }

    async fn finish_merge(
        &self,
        scope: TaskOperation<'_>,
        pr: u64,
        (central, forge): (&str, &str),
    ) -> Step {
        if let Err(e) = self
            .delete_task_branch(scope, (Some(central), Some(forge)))
            .await
        {
            return Step::Retry(format!("the merged branch is not deleted yet: {e}"));
        }
        Step::Done(
            Completion::new("merged", json!({})).with(PullRequestObserved {
                number: pr,
                url: None,
                state: "merged",
                mergeable: "unknown",
                checks_state: "passed",
                checks_failed: vec![],
                checks_log: None,
            }),
        )
    }

    /// `pull_request.close`: closes the pull request without merging it and
    /// deletes the task's branch on the remote and in central aq-git.
    pub async fn pull_request_close(&self, scope: TaskOperation<'_>, pr: u64) -> Step {
        self.close_result(scope, pr, (None, None)).await
    }
    async fn close_result(
        &self,
        scope: TaskOperation<'_>,
        pr: u64,
        leases: (Option<&str>, Option<&str>),
    ) -> Step {
        let target = scope.target;
        let closed = match self.github.close(&target.github, pr).await {
            Ok(p) => p,
            // No such pull request: nothing is open, which is what was asked.
            Err(GitHubApiError::NotFound) => crate::policy::PullRequest {
                number: pr,
                url: String::new(),
                head_sha: String::new(),
                base_branch: target.default_branch.clone(),
                base_sha: String::new(),
                state: PullRequestState::Closed,
                mergeability: Mergeability::Blocked,
            },
            Err(e) => return Step::Retry(format!("the forge did not close it: {}", e.code())),
        };
        if let Err(e) = self.delete_task_branch(scope, leases).await {
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
                checks_log: None,
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
    pub refreshed: Option<Refreshed>,
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

fn merge_method(method: &str) -> MergeMethod {
    match method {
        "squash" => MergeMethod::Squash,
        "rebase" => MergeMethod::Rebase,
        _ => MergeMethod::Merge,
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
