use super::*;
use crate::policy::{Checks, PullRequest};
use std::ops::ControlFlow::{self, Break, Continue};

pub struct CheckRequest<'a> {
    number: u64,
    required: Option<&'a [String]>,
    log_bytes: u64,
    expected_head: Option<&'a str>,
    repair_behind: bool,
}
impl<'a> CheckRequest<'a> {
    pub fn new(number: u64, required: Option<&'a [String]>, log_bytes: u64) -> Self {
        Self {
            number,
            required,
            log_bytes,
            expected_head: None,
            repair_behind: false,
        }
    }
    pub(super) fn for_head(mut self, head: Option<&'a str>, frozen_target: bool) -> Self {
        self.expected_head = head;
        self.repair_behind = head.is_some() && frozen_target;
        self
    }
    fn matches(&self, pr: &PullRequest, base: &str, head: &str) -> bool {
        pr.number == self.number && pr.base_branch == base && pr.head_sha == head
    }
    fn closed(&self, state: PullRequestState) -> bool {
        state == PullRequestState::Closed
            || (state == PullRequestState::Merged && self.expected_head.is_none())
    }
    /// A conflict is final. A behind head can request repair only for a frozen target and head.
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
            Mergeability::Behind if self.repair_behind => Break(Some(Completion::behind_base())),
            Mergeability::Behind => Break(None),
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
        let head = self.expected_head.unwrap_or(&pr.head_sha);
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
        let head = self.expected_head.unwrap_or(&initial.head_sha);
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
            PullRequestState::Open => {
                self.repair(pr.mergeability, seen)?;
                if pr.base_sha != initial.base_sha {
                    return Break(None);
                }
                Continue(())
            }
            PullRequestState::Closed => Break(Some(Completion::closed())),
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
        let initial = self
            .github
            .pull_request(&target.github, request.number)
            .await
            .ok();
        let mut seen = initial.as_ref().map(PullRequestObserved::from_pull);
        let decided = match (initial.as_ref(), seen.as_mut()) {
            (Some(pr), Some(seen)) => self.poll_checks(target, &request, (pr, seen)).await,
            _ => None,
        };
        let completion = decided
            .or_else(|| (now_ms >= deadline_at_ms).then(|| Completion::new("timeout", json!({}))));
        CheckPoll {
            completion,
            observed: seen,
        }
    }
    async fn poll_checks(
        &self,
        target: &DeliveryTarget,
        request: &CheckRequest<'_>,
        (initial, seen): (&PullRequest, &mut PullRequestObserved),
    ) -> Option<Completion> {
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
