//! A merge into a base branch that requires GitHub's merge queue: enqueue the
//! pull request at the expected head (GraphQL `enqueuePullRequest`), then
//! report its place until GitHub merges it or takes it out. Nothing is kept
//! between looks: the pull request's queue entry and its last queue event
//! say where it is.
use super::merge::MergeTarget;
use super::*;
use chrono::{DateTime, Utc};

const LOOK: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id isMergeQueueEnabled mergeQueueEntry{state} timelineItems(last:1,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT]){nodes{__typename ... on AddedToMergeQueueEvent{createdAt} ... on RemovedFromMergeQueueEvent{reason createdAt}}}}}}";
const ENQUEUE: &str = "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{state}}}";
const DEQUEUE: &str =
    "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{state}}}";

#[derive(Deserialize)]
struct Look {
    repository: Option<Repository>,
}
#[derive(Deserialize)]
struct Repository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<QueuedPull>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueuedPull {
    id: String,
    is_merge_queue_enabled: bool,
    merge_queue_entry: Option<Value>,
    timeline_items: Timeline,
}
#[derive(Deserialize)]
struct Timeline {
    nodes: Vec<QueueEvent>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueEvent {
    #[serde(rename = "__typename")]
    kind: String,
    #[serde(default)]
    reason: Option<String>,
    created_at: DateTime<Utc>,
}
/// The pull request's last queue event, when it happened at or after `since_ms`.
enum Since<'a> {
    Nothing,
    Added,
    Removed(&'a str),
}
impl QueuedPull {
    // shortcut: compares GitHub's clock with the host's; a removal within the
    // skew after acceptance reads as older and is enqueued again (the deadline
    // still ends it). Keep the enqueue time durably if that ever shows up.
    fn since(&self, since_ms: i64) -> Since<'_> {
        match self.timeline_items.nodes.last() {
            Some(event) if event.created_at.timestamp_millis() >= since_ms => {
                match (event.kind.as_str(), event.reason.as_deref()) {
                    ("RemovedFromMergeQueueEvent", Some(reason)) => Since::Removed(reason),
                    ("RemovedFromMergeQueueEvent", None) => Since::Removed(""),
                    _ => Since::Added,
                }
            }
            _ => Since::Nothing,
        }
    }
}

impl Session<'_> {
    async fn graphql<T: DeserializeOwned>(&self, query: &str, variables: Value) -> GitHubResult<T> {
        self.app
            .client()
            .graphql(query, variables, Auth::Installation(&self.token))
            .await
    }
    /// The failed required checks of the newest merge group GitHub built for
    /// this pull request at its head, and that group's commit.
    async fn merge_group_failures(
        &self,
        base: &str,
        pull: &WirePull,
    ) -> GitHubResult<(Vec<String>, Option<String>)> {
        let branch = format!(
            "gh-readonly-queue/{base}/pr-{}-{}",
            pull.number, pull.head.sha
        );
        let runs: Value = self
            .call(ApiRequest::new(Method::GET, "/actions/runs").query(&[
                ("event", "merge_group"),
                ("branch", &branch),
                ("per_page", "1"),
            ]))
            .await?;
        let Some(group) = runs["workflow_runs"][0]["head_sha"].as_str() else {
            return Ok((vec![], None));
        };
        let group = commit_sha(group)
            .map_err(|_| GitHubApiError::Protocol("merge group head is not an object id".into()))?;
        let required = self.required_checks(base).await?;
        let reported = self.reported_checks(group).await?;
        let (_, failed) = checks::evaluate(&required, &reported);
        Ok((failed, Some(group.to_owned())))
    }
}

impl MergeTarget<'_> {
    /// One look at an open pull request at the expected head that does not
    /// conflict: `None` when its base branch has no merge queue.
    pub(super) async fn through_queue(
        &self,
        s: &Session<'_>,
        pull: &WirePull,
    ) -> GitHubResult<Option<MergeOutcome>> {
        let variables =
            json!({"owner": self.repo.owner, "name": self.repo.name, "number": pull.number});
        let look: Look = s.graphql(LOOK, variables).await?;
        let queued = look
            .repository
            .and_then(|r| r.pull_request)
            .ok_or(GitHubApiError::NotFound)?;
        if !queued.is_merge_queue_enabled {
            return Ok(None);
        }
        let outcome = match (
            queued.merge_queue_entry.is_some(),
            queued.since(self.queue.since_ms),
        ) {
            (true, _) if self.queue.give_up => self.dequeue(s, &queued.id).await?,
            (true, _) | (false, Since::Added) => MergeOutcome::Queued,
            (false, Since::Removed(reason)) => self.removed(s, pull, reason).await?,
            (false, Since::Nothing) if self.queue.give_up => MergeOutcome::NotMergeable {
                message: "the merge queue wait ended before it was enqueued".into(),
            },
            (false, Since::Nothing) => {
                let enqueue = json!({"id": queued.id, "head": self.head});
                match s.graphql::<Value>(ENQUEUE, enqueue).await {
                    Ok(_) => MergeOutcome::Queued,
                    // For example a required check has not passed on the head.
                    Err(GitHubApiError::Invalid { message }) => {
                        MergeOutcome::NotMergeable { message }
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        Ok(Some(outcome))
    }
    async fn dequeue(&self, s: &Session<'_>, id: &str) -> GitHubResult<MergeOutcome> {
        match s.graphql::<Value>(DEQUEUE, json!({"id": id})).await {
            Ok(_) => Ok(MergeOutcome::NotMergeable {
                message: "the merge queue did not merge it before the deadline".into(),
            }),
            // GitHub is merging it already: the next look sees the merge.
            Err(GitHubApiError::Invalid { .. }) => Ok(MergeOutcome::Queued),
            Err(e) => Err(e),
        }
    }
    async fn removed(
        &self,
        s: &Session<'_>,
        pull: &WirePull,
        reason: &str,
    ) -> GitHubResult<MergeOutcome> {
        Ok(match reason {
            // The queue records the merge before the pull request reads merged.
            "merged" => MergeOutcome::Queued,
            "merge_conflict" => MergeOutcome::Conflict,
            "failed_checks" => {
                let (names, merge_group) = s.merge_group_failures(self.base, pull).await?;
                MergeOutcome::ChecksFailed { names, merge_group }
            }
            other => MergeOutcome::NotMergeable {
                message: format!("removed from the merge queue: {other}"),
            },
        })
    }
}
