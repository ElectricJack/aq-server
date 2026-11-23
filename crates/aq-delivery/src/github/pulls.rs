//! [`GitHubAdapter`] over the GitHub App (spec 0016 §6.3): pull requests,
//! checks, mergeability, merge, close and branch deletion. Each call uses an
//! installation token for the one repository it touches. Every refusal comes
//! back as a typed outcome or [`GitHubApiError`]; nothing retries or waits.
use super::log_tail;
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

// An empty set allows any producer. A nonempty set requires each listed App.
type RequiredChecks = BTreeMap<String, BTreeSet<u64>>;
type ReportedChecks = BTreeMap<String, BTreeMap<Option<u64>, ChecksState>>;

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
        let pull: WirePull = self
            .call(
                Method::GET,
                &format!("/pulls/{}", pr_number(pr)?),
                &[],
                None,
            )
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

    /// Required check names and their App pins from rulesets and classic protection.
    async fn required_checks(&self, base: &str) -> GitHubResult<RequiredChecks> {
        let mut required = RequiredChecks::new();
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
                require_check(&mut required, check, "integration_id");
            }
        }
        let branch: Value = self
            .call(Method::GET, &format!("/branches/{base}"), &[], None)
            .await?;
        if branch["protected"] == true {
            let checks = &branch["protection"]["required_status_checks"];
            // Legacy contexts can repeat names from the structured checks.
            for context in checks["contexts"].as_array().into_iter().flatten() {
                if let Some(name) = context.as_str() {
                    required.entry(name.to_string()).or_default();
                }
            }
            for check in checks["checks"].as_array().into_iter().flatten() {
                require_check(&mut required, check, "app_id");
            }
        }
        Ok(required)
    }

    /// The ids of the failed check runs on `sha` named in `names`, by name. A
    /// GitHub Actions check run's id is its job's id.
    async fn failed_runs(
        &self,
        sha: &str,
        names: &[String],
    ) -> GitHubResult<BTreeMap<String, u64>> {
        let runs = self
            .pages(&format!("/commits/{sha}/check-runs"), |v| {
                array(v, Some("check_runs"))
            })
            .await?;
        let mut failed = BTreeMap::new();
        for run in runs {
            let broken = run["status"] == "completed"
                && !matches!(
                    run["conclusion"].as_str(),
                    Some("success" | "neutral" | "skipped") | None
                );
            if let (true, Some(name), Some(id)) = (broken, run["name"].as_str(), run["id"].as_u64())
                && names.iter().any(|n| n == name)
            {
                failed.entry(name.to_string()).or_insert(id);
            }
        }
        Ok(failed)
    }

    /// Check runs and commit statuses on `sha`, grouped by name and App.
    async fn reported_checks(&self, sha: &str) -> GitHubResult<ReportedChecks> {
        let mut reported = ReportedChecks::new();
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
            record(&mut reported, name, run["app"]["id"].as_u64(), state);
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
            // The combined status response supplies no GitHub App identity.
            record(&mut reported, name, None, state);
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
fn require_check(required: &mut RequiredChecks, check: &Value, app_field: &str) {
    if let Some(name) = check["context"].as_str() {
        let app_id = check[app_field].as_u64().filter(|id| *id > 0);
        required.entry(name.to_string()).or_default().extend(app_id);
    }
}

fn severity(state: ChecksState) -> u8 {
    match state {
        ChecksState::Passed => 0,
        ChecksState::Pending => 1,
        ChecksState::Failed => 2,
    }
}

/// Repeated reports from the same producer keep the worse state.
fn record(reported: &mut ReportedChecks, name: &str, app_id: Option<u64>, state: ChecksState) {
    reported
        .entry(name.to_string())
        .or_default()
        .entry(app_id)
        .and_modify(|current| {
            if severity(state) > severity(*current) {
                *current = state;
            }
        })
        .or_insert(state);
}

fn worst_state(states: impl Iterator<Item = ChecksState>) -> ChecksState {
    states
        .max_by_key(|state| severity(*state))
        .unwrap_or(ChecksState::Pending)
}

/// Missing reports from a required App stay pending.
fn evaluate(required: &RequiredChecks, reported: &ReportedChecks) -> (ChecksState, Vec<String>) {
    let counted: Vec<(&str, ChecksState)> = if required.is_empty() {
        reported
            .iter()
            .map(|(name, states)| (name.as_str(), worst_state(states.values().copied())))
            .collect()
    } else {
        required
            .iter()
            .map(|(name, apps)| {
                let state = match reported.get(name) {
                    None => ChecksState::Pending,
                    Some(states) if apps.is_empty() => worst_state(states.values().copied()),
                    Some(states) => worst_state(apps.iter().map(|id| {
                        states
                            .get(&Some(*id))
                            .copied()
                            .unwrap_or(ChecksState::Pending)
                    })),
                };
                (name.as_str(), state)
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
    } else if counted.is_empty() || counted.iter().any(|(_, s)| *s == ChecksState::Pending) {
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
        let found: Vec<WirePull> = s
            .call(
                Method::GET,
                "/pulls",
                &[("state", "all"), ("head", &head_filter), ("base", base)],
                None,
            )
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
        let pull_request = s.pull(pr).await?.pull()?;
        let base = short_branch(&pull_request.base_branch)?;
        let head_sha = commit_sha(&pull_request.head_sha).map_err(|_| {
            GitHubApiError::Protocol("pull request head is not an object id".into())
        })?;
        let mut requirements = s.required_checks(base).await?;
        for name in required.into_iter().flatten() {
            requirements.entry(name.clone()).or_default();
        }
        let reported = s.reported_checks(head_sha).await?;
        let (state, failed) = evaluate(&requirements, &reported);
        Ok(Checks {
            state,
            failed,
            required: requirements.into_keys().collect(),
            pull_request,
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
        for (name, job) in runs {
            let title: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
            let header = format!("== {title} ==\n");
            let Some(room) = share.checked_sub(header.len() + 1).filter(|r| *r > 0) else {
                continue;
            };
            let fetched = s
                .app
                .client()
                .send_text_tail(
                    &format!("{}/actions/jobs/{job}/logs", s.path),
                    Auth::Installation(&s.token),
                    room,
                )
                .await;
            // A job whose log cannot be read leaves the others.
            if let Some(tail) = fetched.ok().and_then(|w| log_tail::log_tail(&w, room)) {
                sections.push(format!("{header}{tail}\n"));
            }
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
    ) -> GitHubResult<MergeOutcome> {
        let expected = commit_sha(expected_head_sha)?;
        let s = Session::new(self, repo).await?;
        let pull = s.pull(pr).await?;
        if pull.base.name != expected_base {
            return Ok(MergeOutcome::NotMergeable {
                message: format!(
                    "pull request targets {}, expected {expected_base}",
                    pull.base.name
                ),
            });
        }
        let moved = |actual: &str| MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message: format!("head is {actual}, expected {expected}"),
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
            PullRequestState::Merged => return Ok(moved(&pull.head.sha)),
            PullRequestState::Closed => {
                return Ok(MergeOutcome::Refused {
                    reason: MergeRefusal::Closed,
                    message: "pull request is closed".into(),
                });
            }
            PullRequestState::Open => {}
        }
        if pull.head.sha != expected {
            return Ok(moved(&pull.head.sha));
        }
        match pull.mergeability(PullRequestState::Open) {
            Mergeability::Conflict => return Ok(MergeOutcome::Conflict),
            Mergeability::Behind => return Ok(MergeOutcome::Behind),
            _ => {}
        }
        let body = json!({"merge_method": method.as_str(), "sha": expected});
        let merged: GitHubResult<MergeResponse> = s
            .call(Method::PUT, &format!("/pulls/{pr}/merge"), &[], Some(&body))
            .await;
        let message = match merged {
            Ok(MergeResponse {
                merged: true,
                sha: Some(sha),
                ..
            }) => return Ok(MergeOutcome::Merged { sha }),
            Ok(MergeResponse {
                merged: true,
                sha: None,
                ..
            }) => {
                return Err(GitHubApiError::Protocol(
                    "merge response without a sha".into(),
                ));
            }
            Ok(MergeResponse { message, .. })
            | Err(GitHubApiError::Status {
                status: 405,
                message,
            }) => message,
            Err(GitHubApiError::Status {
                status: 409,
                message,
            }) => {
                return Ok(MergeOutcome::Refused {
                    reason: MergeRefusal::HeadMoved,
                    message,
                });
            }
            Err(GitHubApiError::Invalid { message }) => {
                return Ok(MergeOutcome::Refused {
                    reason: MergeRefusal::Invalid,
                    message,
                });
            }
            Err(e) => return Err(e),
        };
        // GitHub can advance the base between the read and the merge request.
        // A refusal alone is also consistent with checks or review policy.
        if let Ok(current) = s.pull(pr).await
            && current.base.name == expected_base
            && matches!(current.state(), Ok(PullRequestState::Open))
        {
            if current.head.sha != expected {
                return Ok(moved(&current.head.sha));
            }
            if current.mergeability(PullRequestState::Open) == Mergeability::Behind {
                return Ok(MergeOutcome::Behind);
            }
        }
        Ok(MergeOutcome::NotMergeable { message })
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

    async fn branch_head(
        &self,
        repo: &GitHubRepository,
        branch: &str,
    ) -> GitHubResult<Option<String>> {
        let branch = short_branch(branch)?;
        let reference = format!("heads/{branch}");
        let s = Session::new(self, repo).await?;
        let refs: Vec<WireGitRef> = s
            .call(
                Method::GET,
                &format!("/git/matching-refs/{reference}"),
                &[],
                None,
            )
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
