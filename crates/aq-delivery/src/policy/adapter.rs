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
    pub base_branch: String,
    pub base_sha: String,
    pub state: PullRequestState,
    pub mergeability: Mergeability,
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
    /// The same pull request observation that selected the check head and base.
    pub pull_request: PullRequest,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Clean,
    Conflict,
    /// The head needs to incorporate changes from the base.
    Behind,
    /// Protection, a draft, or a PR that is no longer open.
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
    /// GitHub positively observed that the tracked head is behind its base.
    Behind,
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
    /// Check the PR head against base branch protection, rulesets, and any
    /// additional names supplied by the project.
    async fn checks(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        required: Option<&[String]>,
    ) -> GitHubResult<Checks>;
    /// The redacted tail of the logs of the failed check runs named `failed`
    /// on `head_sha`: at most `bytes` bytes in all. `None` when the forge has
    /// no log to give. A caller treats an error the same way: the check
    /// stays recorded as failed, without a tail.
    async fn failed_check_log(
        &self,
        _repo: &GitHubRepository,
        _head_sha: &str,
        _failed: &[String],
        _bytes: u64,
    ) -> GitHubResult<Option<String>> {
        Ok(None)
    }
    /// Check the base branch and submit the merge with the expected head SHA.
    async fn merge(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        method: MergeMethod,
        expected_head_sha: &str,
        expected_base: &str,
    ) -> GitHubResult<MergeOutcome>;
    async fn close(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest>;
    async fn branch_head(
        &self,
        _repo: &GitHubRepository,
        _branch: &str,
    ) -> GitHubResult<Option<String>> {
        Err(GitHubApiError::Unconfigured)
    }
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
    async fn merge(
        &self,
        _: &GitHubRepository,
        _: u64,
        _: MergeMethod,
        _: &str,
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
