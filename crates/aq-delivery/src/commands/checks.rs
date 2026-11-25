use super::*;
use crate::policy::{Checks, PullRequest};
use std::ops::ControlFlow::{self, Break, Continue};

/// The branch refresh the host recorded for a wait's result head
/// (`request.refresh`): the head the last refresh was asked from, the base
/// commit it was behind, the merge commit GitHub made of them once seen, and
/// whether this wait may ask for one now (`None`: a behind head is a
/// conflict, as it is without a refresh).
#[derive(Clone, Copy)]
pub struct Refresh<'a> {
    pub from: Option<&'a str>,
    pub base: Option<&'a str>,
    pub merged: Option<&'a str>,
    pub allow: Option<bool>,
}
impl<'a> Refresh<'a> {
    pub(super) fn from_request(refresh: &'a Value) -> Self {
        Self {
            from: refresh["from"].as_str(),
            base: refresh["base"].as_str(),
            merged: refresh["merged"].as_str(),
            allow: refresh["allow"].as_bool(),
        }
    }
}

/// What a wait did to the refresh of its pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refreshed {
    /// Behind with green checks at this head, it waits for the repository's
    /// refresh slot (it queues, oldest first).
    Queued(String),
    /// It asked GitHub to merge `base` into `from`.
    Requested { from: String, base: String },
    /// GitHub's merge commit of the requested refresh is the head now.
    Merged(String),
}

pub struct CheckRequest<'a> {
    number: u64,
    required: Option<&'a [String]>,
    log_bytes: u64,
    expected_head: Option<&'a str>,
    repair_behind: bool,
    refresh: Option<Refresh<'a>>,
    /// The pull request head this look accepted for `expected_head`.
    pin: Option<String>,
    refreshed: Option<Refreshed>,
}
impl<'a> CheckRequest<'a> {
    pub fn new(number: u64, required: Option<&'a [String]>, log_bytes: u64) -> Self {
        Self {
            number,
            required,
            log_bytes,
            expected_head: None,
            repair_behind: false,
            refresh: None,
            pin: None,
            refreshed: None,
        }
    }
    pub(super) fn for_head(mut self, head: Option<&'a str>, frozen_target: bool) -> Self {
        self.expected_head = head;
        self.repair_behind = head.is_some() && frozen_target;
        self
    }
    pub(super) fn with_refresh(mut self, refresh: Option<Refresh<'a>>) -> Self {
        self.refresh = refresh;
        self
    }
    fn head(&self) -> Option<&str> {
        self.pin.as_deref().or(self.expected_head)
    }
    fn matches(&self, pr: &PullRequest, base: &str, head: &str) -> bool {
        pr.number == self.number && pr.base_branch == base && pr.head_sha == head
    }
    fn closed(&self, state: PullRequestState) -> bool {
        state == PullRequestState::Closed
            || (state == PullRequestState::Merged && self.expected_head.is_none())
    }
    /// A conflict is final. A behind head is decided by its checks, and only for a frozen target and head.
    fn repair(
        &self,
        mergeability: Mergeability,
        seen: &mut PullRequestObserved,
    ) -> ControlFlow<Option<Completion>> {
        match mergeability {
            Mergeability::Conflict => {
                seen.mergeable = "conflicting";
                Break(Some(Completion::new("conflict", json!({}))))
            }
            Mergeability::Behind if !self.repair_behind => Break(None),
            _ => Continue(()),
        }
    }
    fn initial(
        &self,
        pr: &PullRequest,
        base: &str,
        seen: &mut PullRequestObserved,
    ) -> ControlFlow<Option<Completion>> {
        if self.closed(pr.state) {
            return Break(Some(Completion::closed()));
        }
        let head = self.head().unwrap_or(&pr.head_sha);
        if !self.matches(pr, base, head) {
            return Break(None);
        }
        if pr.state == PullRequestState::Open {
            return self.repair(pr.mergeability, seen);
        }
        Continue(())
    }
    fn checked(
        &self,
        checks: &Checks,
        base: &str,
        (initial, seen): (&PullRequest, &mut PullRequestObserved),
    ) -> ControlFlow<Option<Completion>> {
        let pr = &checks.pull_request;
        let head = self.head().unwrap_or(&initial.head_sha);
        if !self.matches(pr, base, head) {
            return Break(None);
        }
        if self.closed(pr.state) {
            return Break(Some(Completion::closed()));
        }
        self.checked_state(checks, initial, seen)
    }
    fn checked_state(
        &self,
        checks: &Checks,
        initial: &PullRequest,
        seen: &mut PullRequestObserved,
    ) -> ControlFlow<Option<Completion>> {
        let pr = &checks.pull_request;
        match pr.state {
            // A recovered merge needs green checks for its exact result head. Its base may have moved again since merge.
            PullRequestState::Merged if checks.state != ChecksState::Passed => Break(None),
            PullRequestState::Merged => {
                seen.state = "merged";
                Continue(())
            }
            PullRequestState::Open if initial.state == PullRequestState::Merged => Break(None),
            PullRequestState::Open => self.checked_open(pr, initial, seen),
            PullRequestState::Closed => Break(Some(Completion::closed())),
        }
    }
    fn checked_open(
        &self,
        pr: &PullRequest,
        initial: &PullRequest,
        seen: &mut PullRequestObserved,
    ) -> ControlFlow<Option<Completion>> {
        self.repair(pr.mergeability, seen)?;
        if pr.base_sha == initial.base_sha {
            Continue(())
        } else {
            Break(None)
        }
    }
}
impl PullRequestObserved {
    pub(super) fn from_pull(pr: &PullRequest) -> Self {
        Self {
            number: pr.number,
            url: Some(pr.url.clone()),
            state: match pr.state {
                PullRequestState::Open => "open",
                PullRequestState::Closed => "closed",
                PullRequestState::Merged => "merged",
            },
            mergeable: "unknown",
            checks_state: "none",
            checks_failed: vec![],
            checks_log: None,
        }
    }
    fn record_checks(&mut self, checks: &Checks) -> Option<Completion> {
        self.checks_failed = checks
            .failed
            .iter()
            .take(FAILED_NAMES)
            .map(|n| n.chars().take(NAME_BYTES).collect())
            .collect();
        self.checks_state = match checks.state {
            ChecksState::Passed => "passed",
            ChecksState::Failed => "failed",
            ChecksState::Pending => "pending",
        };
        match checks.state {
            ChecksState::Failed => Some(Completion::new(
                "failed",
                json!({"names":self.checks_failed}),
            )),
            ChecksState::Passed => Some(Completion::new("passed", json!({}))),
            ChecksState::Pending => None,
        }
    }
}
impl Delivery {
    /// A held result may rejoin delivery once the exact open PR is clean and
    /// its required checks have passed. This probe authorizes an observation,
    /// and the delivery run still fences every write against its input head.
    pub async fn held_checks_passed(
        &self,
        target: &DeliveryTarget,
        number: u64,
        head: &str,
        required: Option<&[String]>,
    ) -> bool {
        let request = CheckRequest::new(number, required, 0);
        self.github
            .checks(&target.github, number, required)
            .await
            .is_ok_and(|checked| {
                let pr = &checked.pull_request;
                checked.state == ChecksState::Passed
                    && request.matches(pr, &target.default_branch, head)
                    && pr.state == PullRequestState::Open
                    && pr.mergeability == Mergeability::Clean
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
        request: CheckRequest<'_>,
        (now_ms, deadline_at_ms): (i64, i64),
    ) -> CheckPoll {
        let mut request = request;
        let initial = self
            .github
            .pull_request(&target.github, request.number)
            .await
            .ok();
        let mut seen = initial.as_ref().map(PullRequestObserved::from_pull);
        let decided = match (initial.as_ref(), seen.as_mut()) {
            (Some(pr), Some(seen)) => self.poll_checks(target, &mut request, (pr, seen)).await,
            _ => None,
        };
        let completion = decided
            .or_else(|| (now_ms >= deadline_at_ms).then(|| Completion::new("timeout", json!({}))));
        CheckPoll {
            completion,
            observed: seen,
            refreshed: request.refreshed,
        }
    }
    /// Pins the wait to the pull request head when it is the result head or a
    /// head AQ refreshed it to: the one GitHub made last, or a merge commit of
    /// the last requested refresh whose parents are exactly [from, base].
    async fn accept_head(
        &self,
        target: &DeliveryTarget,
        request: &mut CheckRequest<'_>,
        pr: &PullRequest,
    ) {
        let (Some(result), Some(refresh)) = (request.expected_head, request.refresh) else {
            return;
        };
        let head = pr.head_sha.as_str();
        if head == result || refresh.merged.or(refresh.from) == Some(head) {
            request.pin = Some(head.to_owned());
            return;
        }
        let (None, Some(from), Some(base)) = (refresh.merged, refresh.from, refresh.base) else {
            return;
        };
        let parents = self.github.commit_parents(&target.github, head).await;
        // GitHub merges the base it has when it runs the refresh; main may have moved since.
        if let Ok([first, second]) = parents.as_deref()
            && first == from
            && (second == base || *second == pr.base_sha)
        {
            request.pin = Some(head.to_owned());
            request.refreshed = Some(Refreshed::Merged(head.to_owned()));
        }
    }
    /// Whether `pr`'s head is the result head `result` or one a refresh AQ
    /// recorded for it made ([`Delivery::accept_head`]).
    pub(super) async fn accepts(
        &self,
        target: &DeliveryTarget,
        (result, refresh): (&str, Option<Refresh<'_>>),
        pr: &PullRequest,
    ) -> bool {
        let mut request = CheckRequest::new(pr.number, None, 0)
            .for_head(Some(result), true)
            .with_refresh(refresh);
        self.accept_head(target, &mut request, pr).await;
        request.pin.as_deref() == Some(pr.head_sha.as_str())
    }
    /// A behind head with green checks: asks GitHub to refresh it when the
    /// host allows (keeps waiting otherwise); a conflict when there is no
    /// refresh to ask for or GitHub refuses it.
    async fn refresh(
        &self,
        target: &DeliveryTarget,
        request: &mut CheckRequest<'_>,
        pr: &PullRequest,
    ) -> Option<Completion> {
        match request.refresh.and_then(|refresh| refresh.allow) {
            None => return Some(Completion::behind_base()),
            Some(false) => {
                request.refreshed = Some(Refreshed::Queued(pr.head_sha.clone()));
                return None;
            }
            Some(true) => {}
        }
        // Each (head, base) is asked for once: a refresh needs a new head or a base that moved.
        let refresh = request.refresh.filter(|r| r.from.is_some());
        if refresh.is_some_and(|r| {
            r.from == Some(pr.head_sha.as_str()) && r.base == Some(pr.base_sha.as_str())
        }) {
            request.refreshed = Some(Refreshed::Queued(pr.head_sha.clone()));
            return None;
        }
        match self
            .github
            .update_branch(&target.github, pr.number, &pr.head_sha)
            .await
        {
            Ok(()) => {
                request.refreshed = Some(Refreshed::Requested {
                    from: pr.head_sha.clone(),
                    base: pr.base_sha.clone(),
                });
                None
            }
            Err(
                GitHubApiError::Transport(_)
                | GitHubApiError::Retryable { .. }
                | GitHubApiError::Status {
                    status: 500..=599, ..
                },
            ) => None,
            Err(_) => Some(Completion::behind_base()),
        }
    }
    async fn poll_checks(
        &self,
        target: &DeliveryTarget,
        request: &mut CheckRequest<'_>,
        (initial, seen): (&PullRequest, &mut PullRequestObserved),
    ) -> Option<Completion> {
        self.accept_head(target, request, initial).await;
        if let Break(result) = request.initial(initial, &target.default_branch, seen) {
            return result;
        }
        let checks = self
            .github
            .checks(&target.github, request.number, request.required)
            .await
            .ok()?;
        if let Break(result) = request.checked(&checks, &target.default_branch, (initial, seen)) {
            return result;
        }
        let completion = seen.record_checks(&checks);
        let pr = &checks.pull_request;
        if checks.state == ChecksState::Passed && pr.state == PullRequestState::Open {
            match pr.mergeability {
                Mergeability::Behind => return self.refresh(target, request, pr).await,
                // GitHub recomputes after main moves: a merge now could find it behind.
                Mergeability::Unknown if request.repair_behind => return None,
                _ => {}
            }
        }
        // A failed courtesy log read leaves the failed check recorded without a tail.
        if checks.state == ChecksState::Failed && request.log_bytes > 0 {
            let head = &checks.pull_request.head_sha;
            let log = self
                .github
                .failed_check_log(&target.github, head, &checks.failed, request.log_bytes)
                .await;
            seen.checks_log = log.ok().flatten().map(|tail| CheckLog {
                head: head.clone(),
                tail,
            });
        }
        completion
    }
}
