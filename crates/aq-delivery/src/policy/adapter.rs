//! Epic 0 (spec 0016 §6.2-6.3): the pull-request operations the delivery
//! playbook commands call. Mechanism only: each operation makes the GitHub
//! calls it needs once and reports a typed fact or outcome. Nothing here
//! retries, waits or decides; the playbook does.
use super::{ChecksState, UnconfiguredGitHub};
use crate::github::{GitHubApiError, GitHubResult};
use serde::{Deserialize, Serialize};

/// A GitHub repository, `owner/name`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubRepository {
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    /// The PR's web URL (`html_url`).
    pub url: String,
    pub head_sha: String,
    pub state: PullRequestState,
}

/// Check results for a PR's current head.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checks {
    /// `failed` when any counted check failed, else `pending` when any is
    /// pending or a required one has not reported, else `passed`.
    pub state: ChecksState,
    /// Names of the counted checks that failed, sorted.
    pub failed: Vec<String>,
    /// The required names that were applied: the explicit list when one was
    /// given, else what branch protection and rulesets require on the base
    /// branch. Empty means every reported check counts.
    pub required: Vec<String>,
    /// The head SHA the checks were read for.
    pub head_sha: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Clean,
    Conflict,
    /// Not mergeable for another reason: protection, a draft, a head behind
    /// the base, or a PR that is no longer open.
    Blocked,
    /// GitHub has not computed it yet; a later read may answer.
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}
impl MergeMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Squash => "squash",
            Self::Rebase => "rebase",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergeRefusal {
    /// The PR head is not the expected SHA (GitHub 409, or seen before merging).
    HeadMoved,
    /// The PR is closed without being merged.
    Closed,
    /// GitHub refused the request itself (422), for example a merge method
    /// the repository does not allow.
    Invalid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeOutcome {
    /// Merged, now or earlier at the expected head; `sha` is the merge commit.
    Merged {
        sha: String,
    },
    Conflict,
    /// GitHub 405: not mergeable now (protection, pending checks, ...).
    NotMergeable {
        message: String,
    },
    Refused {
        reason: MergeRefusal,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BranchDeletion {
    Deleted,
    /// GitHub reports no such branch: already gone, which is success.
    AlreadyAbsent,
}

/// Pull-request operations for the delivery playbook commands. Branch names
/// are short names (`aq/<task-id>`); a `refs/heads/` prefix is accepted.
/// Errors are [`GitHubApiError`]; rate limits
/// come back as `retryable` with the reset time.
#[async_trait::async_trait]
pub trait GitHubAdapter: Send + Sync {
    /// The open PR from `head_branch` into `base_branch`, or a new one.
    async fn open_or_find(
        &self,
        repo: &GitHubRepository,
        head_branch: &str,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> GitHubResult<PullRequest>;
    /// One read of the PR: its state and head. Safe to poll.
    async fn pull_request(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest>;
    /// Check runs and commit statuses on the PR head, against `required`
    /// when given, else against the base branch's protection and rulesets.
    async fn checks(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        required: Option<&[String]>,
    ) -> GitHubResult<Checks>;
    /// One read of the PR; safe to poll.
    async fn mergeability(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<Mergeability>;
    /// Merges only if the head is still `expected_head_sha`.
    async fn merge(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        method: MergeMethod,
        expected_head_sha: &str,
    ) -> GitHubResult<MergeOutcome>;
    async fn close(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest>;
    async fn delete_branch(
        &self,
        repo: &GitHubRepository,
        branch: &str,
    ) -> GitHubResult<BranchDeletion>;
}

/// Every operation refuses with `github_unconfigured`.
#[async_trait::async_trait]
impl GitHubAdapter for UnconfiguredGitHub {
    async fn open_or_find(
        &self,
        _: &GitHubRepository,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> GitHubResult<PullRequest> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn pull_request(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn checks(
        &self,
        _: &GitHubRepository,
        _: u64,
        _: Option<&[String]>,
    ) -> GitHubResult<Checks> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn mergeability(&self, _: &GitHubRepository, _: u64) -> GitHubResult<Mergeability> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn merge(
        &self,
        _: &GitHubRepository,
        _: u64,
        _: MergeMethod,
        _: &str,
    ) -> GitHubResult<MergeOutcome> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn close(&self, _: &GitHubRepository, _: u64) -> GitHubResult<PullRequest> {
        Err(GitHubApiError::Unconfigured)
    }
    async fn delete_branch(&self, _: &GitHubRepository, _: &str) -> GitHubResult<BranchDeletion> {
        Err(GitHubApiError::Unconfigured)
    }
}
