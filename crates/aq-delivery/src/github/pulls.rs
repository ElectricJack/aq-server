//! [`GitHubAdapter`] over the GitHub App (spec 0016 §6.3): pull requests,
//! checks, mergeability, merge, close and branch deletion. Each call uses an
//! installation token for the one repository it touches. Every refusal comes
//! back as a typed outcome or [`GitHubApiError`]; nothing retries or waits.
use super::{
    Auth, GitHubApiError, GitHubApp, GitHubResult, InstallationToken, valid_login, valid_repository,
};
use crate::policy::{
    BranchDeletion, Checks, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod,
    MergeOutcome, MergeRefusal, Mergeability, PullRequest, PullRequestState,
};
use reqwest::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// GitHub protocol: the largest `per_page` list endpoints accept.
const GITHUB_MAX_PER_PAGE: usize = 100;

#[derive(Deserialize)]
struct WireRef {
    sha: String,
    #[serde(rename = "ref")]
    name: String,
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
        Ok(PullRequest {
            number: self.number,
            url: self.html_url.clone(),
            head_sha: self.head.sha.clone(),
            state: self.state()?,
        })
    }
    fn mergeability(&self) -> GitHubResult<Mergeability> {
        if self.state()? != PullRequestState::Open {
            return Ok(Mergeability::Blocked);
        }
        Ok(match (self.mergeable, self.mergeable_state.as_deref()) {
            (Some(false), _) | (_, Some("dirty")) => Mergeability::Conflict,
            (Some(true), Some("clean" | "unstable" | "has_hooks")) => Mergeability::Clean,
            (Some(true), Some("blocked" | "behind" | "draft")) => Mergeability::Blocked,
            // `null` while GitHub computes it lazily, or a state we do not know.
            _ => Mergeability::Unknown,
        })
    }
}

#[derive(Deserialize)]
struct MergeResponse {
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    message: String,
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
    if short.len() > 255
        || short.starts_with("refs/")
        || short.contains(['%', '#'])
        || crate::branch(&format!("refs/heads/{short}")).is_err()
    {
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
    if !matches!(sha.len(), 40 | 64)
        || !sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
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
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        rest: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> GitHubResult<T> {
        self.app
            .client()
            .send_query(
                method,
                &format!("{}{rest}", self.path),
                query,
                Auth::Installation(&self.token),
                body,
            )
            .await
    }
    async fn pull(&self, pr: u64) -> GitHubResult<WirePull> {
        self.call(
            Method::GET,
            &format!("/pulls/{}", pr_number(pr)?),
            &[],
            None,
        )
        .await
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
                    Method::GET,
                    rest,
                    &[("per_page", &per_page), ("page", &page)],
                    None,
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

    /// Required status check names on `base`: repository rulesets plus
    /// classic branch protection.
    async fn required_checks(&self, base: &str) -> GitHubResult<Vec<String>> {
        let mut names = BTreeSet::new();
        let rules = self
            .pages(&format!("/rules/branches/{base}"), |v| array(v, None))
            .await?;
        for rule in rules
            .iter()
            .filter(|r| r["type"] == "required_status_checks")
        {
            for check in rule["parameters"]["required_status_checks"]
                .as_array()
                .into_iter()
                .flatten()
            {
                names.extend(check["context"].as_str().map(str::to_string));
            }
        }
        let branch: Value = self
            .call(Method::GET, &format!("/branches/{base}"), &[], None)
            .await?;
        if branch["protected"] == true {
            let checks = &branch["protection"]["required_status_checks"];
            for context in checks["contexts"].as_array().into_iter().flatten() {
                names.extend(context.as_str().map(str::to_string));
            }
            for check in checks["checks"].as_array().into_iter().flatten() {
                names.extend(check["context"].as_str().map(str::to_string));
            }
        }
        Ok(names.into_iter().collect())
    }

    /// Check runs (latest per name) and commit statuses on `sha`, by name.
    async fn reported_checks(&self, sha: &str) -> GitHubResult<BTreeMap<String, ChecksState>> {
        let mut reported = BTreeMap::new();
        let runs = self
            .pages(&format!("/commits/{sha}/check-runs"), |v| {
                array(v, Some("check_runs"))
            })
            .await?;
        for run in runs {
            let Some(name) = run["name"].as_str() else {
                return Err(GitHubApiError::Protocol("check run without a name".into()));
            };
            let state = match (run["status"].as_str(), run["conclusion"].as_str()) {
                (Some("completed"), Some("success" | "neutral" | "skipped")) => ChecksState::Passed,
                (Some("completed"), Some(_)) => ChecksState::Failed,
                _ => ChecksState::Pending,
            };
            record(&mut reported, name, state);
        }
        let statuses = self
            .pages(&format!("/commits/{sha}/status"), |v| {
                array(v, Some("statuses"))
            })
            .await?;
        for status in statuses {
            let Some(name) = status["context"].as_str() else {
                return Err(GitHubApiError::Protocol(
                    "commit status without a context".into(),
                ));
            };
            let state = match status["state"].as_str() {
                Some("success") => ChecksState::Passed,
                Some("failure" | "error") => ChecksState::Failed,
                _ => ChecksState::Pending,
            };
            record(&mut reported, name, state);
        }
        Ok(reported)
    }
}

/// The array at `key` (or the body itself).
fn array(v: Value, key: Option<&str>) -> GitHubResult<Vec<Value>> {
    let v = match key {
        Some(key) => v.get(key).cloned().unwrap_or(Value::Null),
        None => v,
    };
    match v {
        Value::Array(items) => Ok(items),
        _ => Err(GitHubApiError::Protocol("expected a list".into())),
    }
}
/// A name reported twice (a check run and a status) keeps the worse state.
fn record(reported: &mut BTreeMap<String, ChecksState>, name: &str, state: ChecksState) {
    let rank = |s: ChecksState| match s {
        ChecksState::Passed => 0,
        ChecksState::Pending => 1,
        ChecksState::Failed => 2,
    };
    reported
        .entry(name.to_string())
        .and_modify(|s| {
            if rank(state) > rank(*s) {
                *s = state;
            }
        })
        .or_insert(state);
}
/// With required names, only they count and a missing one is pending.
/// Without, every reported check counts.
pub(super) fn evaluate(
    required: &[String],
    reported: &BTreeMap<String, ChecksState>,
) -> (ChecksState, Vec<String>) {
    let counted: Vec<(&str, ChecksState)> = if required.is_empty() {
        reported.iter().map(|(n, s)| (n.as_str(), *s)).collect()
    } else {
        required
            .iter()
            .map(|n| {
                (
                    n.as_str(),
                    reported.get(n).copied().unwrap_or(ChecksState::Pending),
                )
            })
            .collect()
    };
    let failed: BTreeSet<String> = counted
        .iter()
        .filter(|(_, s)| *s == ChecksState::Failed)
        .map(|(n, _)| n.to_string())
        .collect();
    let state = if !failed.is_empty() {
        ChecksState::Failed
    } else if counted.iter().any(|(_, s)| *s == ChecksState::Pending) {
        ChecksState::Pending
    } else {
        ChecksState::Passed
    };
    (state, failed.into_iter().collect())
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
        let open: Vec<WirePull> = s
            .call(
                Method::GET,
                "/pulls",
                &[("state", "open"), ("head", &head_filter), ("base", base)],
                None,
            )
            .await?;
        if let Some(found) = open.first() {
            return found.pull();
        }
        let body = json!({"title": title, "head": head, "base": base, "body": body});
        let created: WirePull = s.call(Method::POST, "/pulls", &[], Some(&body)).await?;
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
        let pull = s.pull(pr).await?;
        let base = short_branch(&pull.base.name)?;
        let head_sha = commit_sha(&pull.head.sha)
            .map_err(|_| GitHubApiError::Protocol("pull request head is not an object id".into()))?
            .to_string();
        let required = match required {
            Some(names) => names
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            None => s.required_checks(base).await?,
        };
        let reported = s.reported_checks(&head_sha).await?;
        let (state, failed) = evaluate(&required, &reported);
        Ok(Checks {
            state,
            failed,
            required,
            head_sha,
        })
    }

    async fn mergeability(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<Mergeability> {
        let s = Session::new(self, repo).await?;
        s.pull(pr).await?.mergeability()
    }

    async fn merge(
        &self,
        repo: &GitHubRepository,
        pr: u64,
        method: MergeMethod,
        expected_head_sha: &str,
    ) -> GitHubResult<MergeOutcome> {
        let expected = commit_sha(expected_head_sha)?;
        let s = Session::new(self, repo).await?;
        let pull = s.pull(pr).await?;
        let moved = || MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message: format!("head is {}, expected {expected}", pull.head.sha),
        };
        match pull.state()? {
            PullRequestState::Merged if pull.head.sha == expected => {
                return match &pull.merge_commit_sha {
                    Some(sha) => Ok(MergeOutcome::Merged { sha: sha.clone() }),
                    None => Err(GitHubApiError::Protocol(
                        "merged pull request without a merge commit".into(),
                    )),
                };
            }
            PullRequestState::Merged => return Ok(moved()),
            PullRequestState::Closed => {
                return Ok(MergeOutcome::Refused {
                    reason: MergeRefusal::Closed,
                    message: "pull request is closed".into(),
                });
            }
            PullRequestState::Open => {}
        }
        if pull.head.sha != expected {
            return Ok(moved());
        }
        if pull.mergeability()? == Mergeability::Conflict {
            return Ok(MergeOutcome::Conflict);
        }
        let body = json!({"merge_method": method.as_str(), "sha": expected});
        let merged: GitHubResult<MergeResponse> = s
            .call(Method::PUT, &format!("/pulls/{pr}/merge"), &[], Some(&body))
            .await;
        match merged {
            Ok(MergeResponse {
                merged: true,
                sha: Some(sha),
                ..
            }) => Ok(MergeOutcome::Merged { sha }),
            Ok(MergeResponse {
                merged: true,
                sha: None,
                ..
            }) => Err(GitHubApiError::Protocol(
                "merge response without a sha".into(),
            )),
            Ok(MergeResponse { message, .. }) => Ok(MergeOutcome::NotMergeable { message }),
            Err(GitHubApiError::Status {
                status: 405,
                message,
            }) => Ok(MergeOutcome::NotMergeable { message }),
            Err(GitHubApiError::Status {
                status: 409,
                message,
            }) => Ok(MergeOutcome::Refused {
                reason: MergeRefusal::HeadMoved,
                message,
            }),
            Err(GitHubApiError::Invalid { message }) => Ok(MergeOutcome::Refused {
                reason: MergeRefusal::Invalid,
                message,
            }),
            Err(e) => Err(e),
        }
    }

    async fn close(&self, repo: &GitHubRepository, pr: u64) -> GitHubResult<PullRequest> {
        let s = Session::new(self, repo).await?;
        let closed: WirePull = s
            .call(
                Method::PATCH,
                &format!("/pulls/{}", pr_number(pr)?),
                &[],
                Some(&json!({"state": "closed"})),
            )
            .await?;
        closed.pull()
    }

    async fn delete_branch(
        &self,
        repo: &GitHubRepository,
        branch: &str,
    ) -> GitHubResult<BranchDeletion> {
        let branch = short_branch(branch)?;
        let s = Session::new(self, repo).await?;
        let deleted: GitHubResult<()> = s
            .call(
                Method::DELETE,
                &format!("/git/refs/heads/{branch}"),
                &[],
                None,
            )
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
}
