//! [`GitHubAdapter`] over the GitHub App (spec 0016 §6.3): pull requests,
//! checks, mergeability, merge, close and branch deletion. Each call uses an
//! installation token for the one repository it touches. Every refusal comes
//! back as a typed outcome or [`GitHubApiError`]; nothing retries or waits.
use super::{
    ApiRequest, Auth, GitHubApiError, GitHubApp, GitHubResult, InstallationToken, valid_login,
    valid_repository,
};
use super::{log_excerpt, log_tail};
use crate::policy::{
    BranchDeletion, Checks, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod,
    MergeOutcome, MergeRefusal, Mergeability, PullRequest, PullRequestState, QueueWait,
};
use reqwest::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod checks;
mod merge;
mod queue;

/// GitHub protocol: the largest `per_page` list endpoints accept.
const GITHUB_MAX_PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct WireRef {
    sha: String,
    #[serde(rename = "ref")]
    name: String,
}
#[derive(Deserialize)]
struct WireGitRef {
    #[serde(rename = "ref")]
    name: String,
    object: WireGitObject,
}
#[derive(Deserialize)]
struct WireGitObject {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}
#[derive(Deserialize)]
struct WirePull {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    merged: Option<bool>,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    #[serde(default)]
    mergeable: Option<bool>,
    #[serde(default)]
    mergeable_state: Option<String>,
    head: WireRef,
    base: WireRef,
}
impl WirePull {
    fn state(&self) -> GitHubResult<PullRequestState> {
        match self.state.as_str() {
            "open" => Ok(PullRequestState::Open),
            "closed" if self.merged == Some(true) || self.merged_at.is_some() => {
                Ok(PullRequestState::Merged)
            }
            "closed" => Ok(PullRequestState::Closed),
            _ => Err(GitHubApiError::Protocol(
                "unknown pull request state".into(),
            )),
        }
    }
    fn pull(&self) -> GitHubResult<PullRequest> {
        let state = self.state()?;
        Ok(PullRequest {
            number: self.number,
            url: self.html_url.clone(),
            head_sha: self.head.sha.clone(),
            base_branch: self.base.name.clone(),
            base_sha: self.base.sha.clone(),
            state,
            mergeability: self.mergeability(state),
        })
    }
    fn mergeability(&self, state: PullRequestState) -> Mergeability {
        if state != PullRequestState::Open {
            return Mergeability::Blocked;
        }
        match (self.mergeable, self.mergeable_state.as_deref()) {
            (Some(false), _) | (_, Some("dirty")) => Mergeability::Conflict,
            (Some(true), Some("clean" | "unstable" | "has_hooks")) => Mergeability::Clean,
            (Some(true), Some("behind")) => Mergeability::Behind,
            (Some(true), Some("blocked" | "draft")) => Mergeability::Blocked,
            // `null` while GitHub computes it lazily, or a state we do not know.
            _ => Mergeability::Unknown,
        }
    }
}

/// `owner/name` checked against GitHub's naming, as the API path prefix.
fn repo_path(repo: &GitHubRepository) -> GitHubResult<String> {
    if !valid_login(&repo.owner) || !valid_repository(&repo.name) {
        return Err(GitHubApiError::InvalidInput("invalid owner or repository"));
    }
    Ok(format!("/repos/{}/{}", repo.owner, repo.name))
}
/// A short branch name, `refs/heads/` stripped. It must be a valid git branch,
/// not another full ref, and need no escaping in a URL path.
fn short_branch(branch: &str) -> GitHubResult<&str> {
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let safe_path =
        short.len() <= 255 && !short.starts_with("refs/") && !short.contains(['%', '#']);
    if !safe_path || crate::branch(&format!("refs/heads/{short}")).is_err() {
        return Err(GitHubApiError::InvalidInput("invalid branch name"));
    }
    Ok(short)
}
fn pr_number(pr: u64) -> GitHubResult<u64> {
    if pr == 0 {
        return Err(GitHubApiError::InvalidInput("invalid pull request number"));
    }
    Ok(pr)
}
fn commit_sha(sha: &str) -> GitHubResult<&str> {
    let valid =
        matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(GitHubApiError::InvalidInput(
            "expected head sha is not a lowercase hex object id",
        ));
    }
    Ok(sha)
}

/// One repository's API calls under one installation token.
struct Session<'a> {
    app: &'a GitHubApp,
    token: InstallationToken,
    path: String,
}
impl<'a> Session<'a> {
    async fn new(app: &'a GitHubApp, repo: &GitHubRepository) -> GitHubResult<Self> {
        let path = repo_path(repo)?;
        let token = app.installation_token(&repo.owner, &repo.name).await?;
        Ok(Self { app, token, path })
    }
    async fn call<T: DeserializeOwned>(&self, request: ApiRequest<'_>) -> GitHubResult<T> {
        self.app
            .client()
            .send(request.under(&self.path), Auth::Installation(&self.token))
            .await
    }
    async fn pull(&self, pr: u64) -> GitHubResult<WirePull> {
        let pull: WirePull = self
            .call(ApiRequest::new(
                Method::GET,
                &format!("/pulls/{}", pr_number(pr)?),
            ))
            .await?;
        if pull.number != pr {
            return Err(GitHubApiError::Protocol(
                "pull request number differs from the requested number".into(),
            ));
        }
        Ok(pull)
    }
    /// Every page of a list endpoint; `items` picks the page's entries.
    async fn pages(
        &self,
        rest: &str,
        items: impl Fn(Value) -> GitHubResult<Vec<Value>>,
    ) -> GitHubResult<Vec<Value>> {
        let per_page = GITHUB_MAX_PER_PAGE.to_string();
        let mut all = Vec::new();
        for page in 1.. {
            let page = page.to_string();
            let body: Value = self
                .call(
                    ApiRequest::new(Method::GET, rest)
                        .query(&[("per_page", &per_page), ("page", &page)]),
                )
                .await?;
            let entries = items(body)?;
            let full = entries.len() >= GITHUB_MAX_PER_PAGE;
            all.extend(entries);
            if !full {
                break;
            }
        }
        Ok(all)
    }
}

#[async_trait::async_trait]
impl GitHubAdapter for GitHubApp {
    async fn open_or_find(
        &self,
        repo: &GitHubRepository,
        head_branch: &str,
        base_branch: &str,
        title: &str,
        body: &str,
    ) -> GitHubResult<PullRequest> {
        let head = short_branch(head_branch)?;
        let base = short_branch(base_branch)?;
        if title.trim().is_empty() {
            return Err(GitHubApiError::InvalidInput("pull request title is empty"));
        }
        let s = Session::new(self, repo).await?;
        let head_filter = format!("{}:{head}", repo.owner);
        let found: Vec<WirePull> = s
            .call(ApiRequest::new(Method::GET, "/pulls").query(&[
                ("state", "all"),
                ("head", &head_filter),
                ("base", base),
            ]))
            .await?;
        match found.as_slice() {
            [] => {}
            [found] if found.head.name == head && found.base.name == base => {
                // The detail endpoint distinguishes closed from merged.
                return s.pull(found.number).await?.pull();
            }
            _ => {
                return Err(GitHubApiError::Protocol(
                    "could not identify the task branch's unique pull request".into(),
                ));
            }
        }
        let body = json!({"title": title, "head": head, "base": base, "body": body});
        let created: WirePull = s
            .call(ApiRequest::new(Method::POST, "/pulls").json(&body))
            .await?;
        created.pull()
    }

    async fn pull_request(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest> {
        let s = Session::new(self, repo).await?;
        s.pull(pr).await?.pull()
    }

    async fn checks(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        required: Option<&[String]>,
    ) -> GitHubResult<Checks> {
        let s = Session::new(self, repo).await?;
        let pull_request = s.pull(pr).await?.pull()?;
        let base = short_branch(&pull_request.base_branch)?;
        let head_sha = commit_sha(&pull_request.head_sha).map_err(|_| {
            GitHubApiError::Protocol("pull request head is not an object id".into())
        })?;
        let (mut requirements, merge_queue) = s.base_rules(base).await?;
        for name in required.into_iter().flatten() {
            requirements.entry(name.clone()).or_default();
        }
        let reported = s.reported_checks(head_sha).await?;
        let (state, failed) = checks::evaluate(&requirements, &reported);
        Ok(Checks {
            state,
            failed,
            required: requirements.into_keys().collect(),
            pull_request,
            merge_queue,
        })
    }

    async fn failed_check_log(
        &self,
        repo: &GitHubRepository,
        head_sha: &str,
        failed: &[String],
        bytes: u64,
    ) -> GitHubResult<Option<String>> {
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        if bytes == 0 || failed.is_empty() {
            return Ok(None);
        }
        let s = Session::new(self, repo).await?;
        let runs = s.failed_runs(commit_sha(head_sha)?, failed).await?;
        if runs.is_empty() {
            return Ok(None);
        }
        // The bound is for the whole record: the failed jobs share it.
        let share = bytes / runs.len();
        let mut sections = Vec::new();
        for (name, (job, page)) in runs {
            let title: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
            let page: String = (page.iter().flat_map(|p| p.chars()))
                .take_while(|c| !c.is_control() && !c.is_whitespace())
                .take(200)
                .collect();
            let header = match page.is_empty() {
                true => format!("== {title} ==\n"),
                false => format!("== {title} ==\n{page}\n"),
            };
            let Some(room) = share.checked_sub(header.len() + 1).filter(|r| *r > 0) else {
                continue;
            };
            let mut excerpt = log_excerpt::Excerpt::new(room);
            // A log that cannot be read (no Actions read permission, gone)
            // leaves the check's name and page; one cut short, what it showed.
            let _ = (s.app.client())
                .send_text_lines(
                    &format!("{}/actions/jobs/{job}/logs", s.path),
                    Auth::Installation(&s.token),
                    room,
                    |line| excerpt.line(line),
                )
                .await;
            sections.push(
                match log_tail::log_tail(excerpt.finish().as_bytes(), room) {
                    Some(text) => format!("{header}{text}\n"),
                    None => header,
                },
            );
        }
        let joined = sections.concat();
        Ok((!joined.is_empty()).then(|| joined.trim_end().to_owned()))
    }

    async fn merge(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        method: MergeMethod,
        expected_head_sha: &str,
        expected_base: &str,
        queue: QueueWait,
    ) -> GitHubResult<MergeOutcome> {
        let expected = commit_sha(expected_head_sha)?;
        let s = Session::new(self, repo).await?;
        merge::MergeTarget {
            repo,
            head: expected,
            base: expected_base,
            queue,
        }
        .merge(&s, pr, method)
        .await
    }

    async fn close(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest> {
        let s = Session::new(self, repo).await?;
        let closed: WirePull = s
            .call(
                ApiRequest::new(Method::PATCH, &format!("/pulls/{}", pr_number(pr)?))
                    .json(&json!({"state": "closed"})),
            )
            .await?;
        closed.pull()
    }

    async fn branch_head(
        &self,
        repo: &GitHubRepository,
        branch: &str,
    ) -> GitHubResult<Option<String>> {
        let branch = short_branch(branch)?;
        let reference = format!("heads/{branch}");
        let s = Session::new(self, repo).await?;
        let refs: Vec<WireGitRef> = s
            .call(ApiRequest::new(
                Method::GET,
                &format!("/git/matching-refs/{reference}"),
            ))
            .await?;
        // A successful listing can prove absence. A 404 can also mean lost access.
        let full = format!("refs/{reference}");
        let Some(reference) = refs.iter().find(|reference| reference.name == full) else {
            return Ok(None);
        };
        if reference.object.kind != "commit" {
            return Err(GitHubApiError::Protocol(
                "unexpected branch reference".into(),
            ));
        }
        let sha = commit_sha(&reference.object.sha)?;
        Ok(Some(sha.to_owned()))
    }

    async fn delete_branch(
        &self,
        repo: &GitHubRepository,
        branch: &str,
    ) -> GitHubResult<BranchDeletion> {
        let branch = short_branch(branch)?;
        let s = Session::new(self, repo).await?;
        let deleted: GitHubResult<()> = s
            .call(ApiRequest::new(
                Method::DELETE,
                &format!("/git/refs/heads/{branch}"),
            ))
            .await;
        match deleted {
            Ok(()) => Ok(BranchDeletion::Deleted),
            Err(GitHubApiError::NotFound) => Ok(BranchDeletion::AlreadyAbsent),
            // GitHub answers a delete of a missing ref with 422 and this message.
            Err(GitHubApiError::Invalid { message })
                if message.starts_with("Reference does not exist") =>
            {
                Ok(BranchDeletion::AlreadyAbsent)
            }
            Err(e) => Err(e),
        }
    }

    async fn update_branch(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        expected_head_sha: &str,
    ) -> GitHubResult<()> {
        let body = json!({"expected_head_sha": commit_sha(expected_head_sha)?});
        let path = format!("/pulls/{}/update-branch", pr_number(pr)?);
        let s = Session::new(self, repo).await?;
        // 202: GitHub merges the base in later; the body only says so.
        s.call::<serde::de::IgnoredAny>(ApiRequest::new(Method::PUT, &path).json(&body))
            .await
            .map(drop)
    }

    async fn commit_parents(
        &self,
        repo: &GitHubRepository,
        sha: &str,
    ) -> GitHubResult<Vec<String>> {
        #[derive(Deserialize)]
        struct Commit {
            sha: String,
            parents: Vec<WireSha>,
        }
        #[derive(Deserialize)]
        struct WireSha {
            sha: String,
        }
        let path = format!("/git/commits/{}", commit_sha(sha)?);
        let s = Session::new(self, repo).await?;
        let commit: Commit = s.call(ApiRequest::new(Method::GET, &path)).await?;
        if commit.sha != sha {
            return Err(GitHubApiError::Protocol(
                "commit differs from the requested sha".into(),
            ));
        }
        Ok(commit.parents.into_iter().map(|p| p.sha).collect())
    }
}
