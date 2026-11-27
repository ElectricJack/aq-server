use super::*;
use std::ops::ControlFlow;

#[derive(Deserialize)]
struct MergeResponse {
    #[serde(default)]
    merged: bool,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    message: String,
}

pub(super) struct MergeTarget<'a> {
    pub(super) repo: &'a GitHubRepository,
    pub(super) head: &'a str,
    pub(super) base: &'a str,
    pub(super) queue: QueueWait,
}
impl MergeTarget<'_> {
    fn moved(&self, actual: &str) -> MergeOutcome {
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message: format!("head is {actual}, expected {}", self.head),
        }
    }
    fn inspect(&self, pull: &WirePull) -> GitHubResult<Option<MergeOutcome>> {
        if pull.base.name != self.base {
            return Ok(Some(MergeOutcome::NotMergeable {
                message: format!(
                    "pull request targets {}, expected {}",
                    pull.base.name, self.base
                ),
            }));
        }
        let outcome = match pull.state()? {
            PullRequestState::Merged if pull.head.sha == self.head => {
                let sha = pull.merge_commit_sha.clone().ok_or_else(|| {
                    GitHubApiError::Protocol("merged pull request without a merge commit".into())
                })?;
                MergeOutcome::Merged { sha }
            }
            PullRequestState::Closed => MergeOutcome::Refused {
                reason: MergeRefusal::Closed,
                message: "pull request is closed".into(),
            },
            _ if pull.head.sha != self.head => self.moved(&pull.head.sha),
            _ if pull.mergeability(PullRequestState::Open) == Mergeability::Conflict => {
                MergeOutcome::Conflict
            }
            _ => return Ok(None),
        };
        Ok(Some(outcome))
    }
    fn after_refusal(&self, current: WirePull) -> Option<MergeOutcome> {
        if current.base.name != self.base || !matches!(current.state(), Ok(PullRequestState::Open))
        {
            return None;
        }
        if current.head.sha != self.head {
            return Some(self.moved(&current.head.sha));
        }
        (current.mergeability(PullRequestState::Open) == Mergeability::Behind)
            .then_some(MergeOutcome::Behind)
    }
    pub(super) async fn merge(
        &self,
        s: &Session<'_>,
        pr: u64,
        method: MergeMethod,
    ) -> GitHubResult<MergeOutcome> {
        let pull = s.pull(pr).await?;
        if let Some(outcome) = self.inspect(&pull)? {
            return Ok(outcome);
        }
        // Behind is no obstacle to a merge queue: it merges onto the base it builds.
        if let Some(outcome) = self.through_queue(s, &pull).await? {
            return Ok(outcome);
        }
        if pull.mergeability(PullRequestState::Open) == Mergeability::Behind {
            return Ok(MergeOutcome::Behind);
        }
        let body = json!({"merge_method": method.as_str(), "sha": self.head});
        let merged: GitHubResult<MergeResponse> = s
            .call(ApiRequest::new(Method::PUT, &format!("/pulls/{pr}/merge")).json(&body))
            .await;
        let message = match merge_result(merged)? {
            ControlFlow::Break(outcome) => return Ok(outcome),
            ControlFlow::Continue(message) => message,
        };
        // GitHub can advance the base between the read and the merge request.
        // A refusal alone is also consistent with checks or review policy.
        Ok(s.pull(pr)
            .await
            .ok()
            .and_then(|current| self.after_refusal(current))
            .unwrap_or(MergeOutcome::NotMergeable { message }))
    }
}

fn merge_result(
    merged: GitHubResult<MergeResponse>,
) -> GitHubResult<ControlFlow<MergeOutcome, String>> {
    match merged {
        Ok(MergeResponse {
            merged: true,
            sha: Some(sha),
            ..
        }) => Ok(ControlFlow::Break(MergeOutcome::Merged { sha })),
        Ok(MergeResponse {
            merged: true,
            sha: None,
            ..
        }) => Err(GitHubApiError::Protocol(
            "merge response without a sha".into(),
        )),
        Ok(MergeResponse { message, .. })
        | Err(GitHubApiError::Status {
            status: 405,
            message,
        }) => Ok(ControlFlow::Continue(message)),
        Err(GitHubApiError::Status {
            status: 409,
            message,
        }) => Ok(ControlFlow::Break(MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message,
        })),
        Err(GitHubApiError::Invalid { message }) => Ok(ControlFlow::Break(MergeOutcome::Refused {
            reason: MergeRefusal::Invalid,
            message,
        })),
        Err(e) => Err(e),
    }
}
