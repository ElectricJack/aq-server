use super::*;

#[derive(Clone, Copy)]
pub struct FrozenCommand<'a> {
    pub operation: Uuid,
    pub name: &'a str,
    pub request: &'a Value,
}
impl<'a> FrozenCommand<'a> {
    pub fn new(operation: Uuid, name: &'a str, request: &'a Value) -> Self {
        Self {
            operation,
            name,
            request,
        }
    }
    fn text(&self, key: &str) -> Option<&'a str> {
        self.request[key].as_str()
    }
    fn number(&self) -> Option<u64> {
        self.request["pull_request"].as_u64()
    }
    fn task(&self) -> Option<Uuid> {
        Uuid::parse_str(self.text("task")?).ok()
    }
    fn open(&self) -> Option<OpenPullRequest<'a>> {
        Some(OpenPullRequest {
            title: self.text("title")?,
            body: self.text("body")?,
            known_pr: self.number(),
        })
    }
    /// A merge's place in a merge queue as its request froze it. A request
    /// frozen without a queue deadline has none to wait.
    pub(super) fn queue_wait(&self, now_ms: i64) -> QueueWait {
        QueueWait {
            since_ms: self.request["queued_since_ms"].as_i64().unwrap_or(0),
            give_up: now_ms >= self.request["queue_deadline_at_ms"].as_i64().unwrap_or(0),
        }
    }
    fn required(&self) -> Option<Vec<String>> {
        self.request["required"]
            .as_array()
            .filter(|names| !names.is_empty())
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
    }
    pub(super) async fn branch_preflight(
        self,
        delivery: &Delivery,
        target: &DeliveryTarget,
    ) -> Option<Step> {
        let result = self.text("result_head").unwrap_or_default();
        let refresh = self.request.get("refresh").map(Refresh::from_request);
        if let Some(number) = self.number() {
            match delivery.github.pull_request(&target.github, number).await {
                Ok(pr) if pr.state == PullRequestState::Merged && result == pr.head_sha => {
                    return Some(Step::Done(Completion::new(
                        "published",
                        json!({"head":pr.head_sha}),
                    )));
                }
                // A refresh AQ made of this result counts as its publication: never push over it.
                Ok(pr)
                    if pr.state != PullRequestState::Closed
                        && result != pr.head_sha
                        && delivery.accepts(target, (result, refresh), &pr).await =>
                {
                    return Some(Step::Done(Completion::new(
                        "published",
                        json!({"head":result}),
                    )));
                }
                Ok(pr) if pr.state == PullRequestState::Open => {}
                Ok(_) => return Some(Step::Done(Completion::refused("no_branch"))),
                Err(_) => return Some(Step::Retry("observe the retained pull request".into())),
            }
        }
        None
    }
    async fn branch(self, delivery: &Delivery, target: &DeliveryTarget, task: Uuid) -> Step {
        if let Some(step) = self.branch_preflight(delivery, target).await {
            return step;
        }
        delivery
            .branch_publish(
                TaskOperation::new(self.operation, target, task),
                self.text("expected_old"),
                self.text("result_head"),
            )
            .await
    }
    async fn merge(
        self,
        delivery: &Delivery,
        target: &DeliveryTarget,
        (task, now_ms): (Uuid, i64),
    ) -> Option<Step> {
        let method = merge_method(self.text("method")?);
        let Some(pr) = self.number() else {
            return Some(Step::Done(Completion::refused("no_pull_request")));
        };
        let result = self.text("expected_head_sha")?;
        let guard = MergeGuard {
            method,
            expected_head: self.text("pull_request_head").unwrap_or(result),
            result_head: Some(result),
            queue: self.queue_wait(now_ms),
            check_log_bytes: self.request["check_log_bytes"].as_u64().unwrap_or(0),
        };
        Some(
            delivery
                .pull_request_merge(TaskOperation::new(self.operation, target, task), pr, guard)
                .await,
        )
    }
    async fn close(self, delivery: &Delivery, target: &DeliveryTarget) -> Step {
        match (self.task(), self.number()) {
            (Some(task), Some(pr)) => {
                delivery
                    .close_result(
                        TaskOperation::new(self.operation, target, task),
                        pr,
                        (
                            self.text("result_head"),
                            self.text("pull_request_head").or(self.text("result_head")),
                        ),
                    )
                    .await
            }
            // No pull request is open for the task.
            _ => Step::Done(Completion::closed()),
        }
    }
    async fn checks(
        self,
        delivery: &Delivery,
        target: &DeliveryTarget,
        now_ms: i64,
    ) -> Option<Run> {
        let Some(number) = self.number() else {
            return Some(Run::done(Step::Done(Completion::closed())));
        };
        let required = self.required();
        let deadline = self.request["deadline_at_ms"].as_i64()?;
        let frozen = self.request.get("check_target");
        if let Some(frozen) = frozen
            && *frozen != serde_json::to_value(target).ok()?
        {
            return Some(Run {
                step: (now_ms >= deadline)
                    .then(|| Step::Done(Completion::new("timeout", json!({})))),
                observed: None,
                refreshed: None,
            });
        }
        let request = CheckRequest::new(
            number,
            required.as_deref(),
            self.request["check_log_bytes"].as_u64().unwrap_or(0),
        )
        .for_head(self.text("result_head"), frozen.is_some())
        .with_refresh(self.request.get("refresh").map(Refresh::from_request));
        let poll = delivery
            .await_checks_step(target, request, (now_ms, deadline))
            .await;
        Some(Run {
            step: poll.completion.map(Step::Done),
            observed: poll.observed,
            refreshed: poll.refreshed,
        })
    }
}
impl Run {
    fn done(step: Step) -> Self {
        Self {
            step: Some(step),
            observed: None,
            refreshed: None,
        }
    }
}
impl Delivery {
    /// One step from the request the host froze at acceptance. The operation UUID is
    /// aq-git's idempotency key. A request missing its command's inputs yields None.
    pub async fn step(
        &self,
        command: FrozenCommand<'_>,
        target: &DeliveryTarget,
        now_ms: i64,
    ) -> Option<Run> {
        let step = match command.name {
            "main.sync" => self.main_sync(command.operation, target).await,
            "branch.publish" => command.branch(self, target, command.task()?).await,
            "pull_request.open" => {
                self.pull_request_open(target, command.task()?, command.open()?)
                    .await
            }
            "pull_request.merge" => {
                command
                    .merge(self, target, (command.task()?, now_ms))
                    .await?
            }
            "pull_request.close" => command.close(self, target).await,
            "pull_request.await_checks" => return command.checks(self, target, now_ms).await,
            _ => return None,
        };
        Some(Run::done(step))
    }
}
