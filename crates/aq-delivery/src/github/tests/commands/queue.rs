//! `pull_request.merge` into a base branch that requires a merge queue.
use super::*;
use crate::policy::QueueWait;

/// 2026-11-26T00:00:00Z: the merge began here.
const SINCE_MS: i64 = 1_795_651_200_000;
const AFTER: &str = "2026-11-26T00:05:00Z";
const BEFORE: &str = "2026-11-25T23:55:00Z";

async fn merge(delivery: &Delivery, give_up: bool, check_log_bytes: u64) -> Step {
    delivery
        .pull_request_merge(
            TaskOperation::new(Uuid::from_u128(6), &target(), TASK),
            7,
            MergeGuard {
                method: MergeMethod::Merge,
                expected_head: &sha('a'),
                result_head: None,
                queue: QueueWait {
                    since_ms: SINCE_MS,
                    give_up,
                },
                check_log_bytes,
            },
        )
        .await
}
fn queued(mock: &Mock, entry: Option<&str>, last: Option<(&str, &str, &str)>) {
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(200, vec![], queue_look(true, entry, last));
}
fn removed(mock: &Mock, reason: &'static str) {
    queued(
        mock,
        None,
        Some(("RemovedFromMergeQueueEvent", reason, AFTER)),
    );
}
fn mutation(mock: &Mock, entry: &str) {
    let name = if entry == "dequeue" {
        "dequeuePullRequest"
    } else {
        "enqueuePullRequest"
    };
    mock.reply(
        200,
        vec![],
        json!({"data": {name: {"mergeQueueEntry": {"state": "QUEUED"}}}}),
    );
}
fn graphql_bodies(mock: &Mock) -> Vec<Value> {
    mock.seen()
        .iter()
        .filter(|r| r.path == "/api/graphql")
        .map(|r| r.body.clone())
        .collect()
}

#[tokio::test]
async fn a_merge_queue_merge_is_enqueued_at_the_head_and_completes_when_github_merges() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // Behind is no obstacle: the queue builds onto main itself.
    mock.reply(200, vec![], pull(7, json!({"mergeable_state": "behind"})));
    mock.reply(200, vec![], queue_look(true, None, None));
    mutation(&mock, "enqueue");
    assert!(matches!(merge(&delivery, false, 0).await, Step::Retry(_)));
    let enqueue = graphql_bodies(&mock)[1].clone();
    assert!(
        enqueue["query"]
            .as_str()
            .unwrap()
            .contains("enqueuePullRequest")
    );
    assert_eq!(
        enqueue["variables"],
        json!({"id": "PR_7", "head": sha('a')})
    );

    // Queued, and an entry whose removal GitHub has not shown yet, are waited on.
    queued(
        &mock,
        Some("AWAITING_CHECKS"),
        Some(("AddedToMergeQueueEvent", "", AFTER)),
    );
    assert!(matches!(merge(&delivery, false, 0).await, Step::Retry(_)));
    queued(&mock, None, Some(("AddedToMergeQueueEvent", "", AFTER)));
    assert!(matches!(merge(&delivery, false, 0).await, Step::Retry(_)));
    removed(&mock, "merged");
    assert!(matches!(merge(&delivery, false, 0).await, Step::Retry(_)));
    assert!(git.operations().is_empty());

    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({"state": "closed", "merged": true, "merge_commit_sha": sha('d')}),
        ),
    );
    let c = done(merge(&delivery, false, 0).await);
    assert_eq!((c.outcome, c.output), ("merged", json!({})));
    assert_eq!(git.operations(), ["ref.delete"]);
    assert_eq!(graphql_bodies(&mock).len(), 5, "one enqueue, four looks");
    assert!(
        !paths(&mock).iter().any(|(m, _)| m == "PUT"),
        "never a direct merge"
    );
}

#[tokio::test]
async fn a_removal_maps_onto_conflict_not_mergeable_or_a_new_enqueue() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    removed(&mock, "merge_conflict");
    let c = done(merge(&delivery, false, 0).await);
    assert_eq!((c.outcome, c.diagnostic), ("conflict", None));
    removed(&mock, "manual");
    assert_eq!(
        done(merge(&delivery, false, 0).await).outcome,
        "not_mergeable"
    );
    // A removal before this merge began is an earlier merge's: enqueue again.
    queued(
        &mock,
        None,
        Some(("RemovedFromMergeQueueEvent", "manual", BEFORE)),
    );
    mutation(&mock, "enqueue");
    assert!(matches!(merge(&delivery, false, 0).await, Step::Retry(_)));
    // GitHub refuses the enqueue (a required check has not passed).
    queued(&mock, None, None);
    mock.reply(
        200,
        vec![],
        json!({"data": null, "errors": [{"type": "UNPROCESSABLE", "message": "Pull request is not mergeable"}]}),
    );
    assert_eq!(
        done(merge(&delivery, false, 0).await).outcome,
        "not_mergeable"
    );
    // A moved or conflicting head is refused before the queue is asked.
    mock.reply(
        200,
        vec![],
        pull(7, json!({"head": {"ref": "x", "sha": sha('b')}})),
    );
    let c = done(merge(&delivery, false, 0).await);
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "head_changed"}))
    );
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn failed_merge_group_checks_return_conflict_with_the_names_and_the_log() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    removed(&mock, "failed_checks");
    mock.reply(
        200,
        vec![],
        json!({"workflow_runs": [{"head_sha": sha('9')}]}),
    );
    let group = json!({"total_count": 2, "check_runs": [
        {"id": 9, "name": "unit", "status": "completed", "conclusion": "failure"},
        {"id": 8, "name": "lint", "status": "completed", "conclusion": "success"}]});
    mock.reply(200, vec![], json!([]));
    mock.reply(200, vec![], json!({"name": "main", "protected": false}));
    mock.reply(200, vec![], group.clone());
    mock.reply(200, vec![], no_statuses());
    // The log read lists the group's runs again and follows the job's redirect.
    mock.reply(200, vec![], group);
    mock.reply(
        302,
        vec![("location", "/blob/job-9.log".to_string())],
        json!({}),
    );
    mock.reply(
        200,
        vec![],
        json!("compiling\nFAILED unit on the merge group\n"),
    );
    let c = done(merge(&delivery, false, 4096).await);
    assert_eq!(
        (c.outcome, c.output, c.diagnostic),
        ("conflict", json!({}), Some("merge_group_failed"))
    );
    let pr = c.pull_request.unwrap();
    assert_eq!(
        (pr.checks_state, pr.checks_failed),
        ("failed", vec!["unit".to_string()])
    );
    let log = pr.checks_log.unwrap();
    assert_eq!(log.head, sha('a'), "kept for the head the group tested");
    assert!(
        log.tail.ends_with("FAILED unit on the merge group"),
        "{}",
        log.tail
    );
    let seen = mock.seen();
    let runs = seen
        .iter()
        .find(|r| r.path.ends_with("/actions/runs"))
        .unwrap();
    assert_eq!(
        runs.query.as_deref(),
        Some(
            format!(
                "event=merge_group&branch=gh-readonly-queue%2Fmain%2Fpr-7-{}&per_page=1",
                sha('a')
            )
            .as_str()
        )
    );
    assert!(seen.iter().any(|r| {
        r.path
            .ends_with(&format!("/commits/{}/check-runs", sha('9')))
    }));
    drop(seen);
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn a_merge_past_its_queue_deadline_is_taken_out_unless_github_is_merging_it() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    queued(
        &mock,
        Some("QUEUED"),
        Some(("AddedToMergeQueueEvent", "", AFTER)),
    );
    mutation(&mock, "dequeue");
    assert_eq!(
        done(merge(&delivery, true, 0).await).outcome,
        "not_mergeable"
    );
    assert_eq!(graphql_bodies(&mock)[1]["variables"], json!({"id": "PR_7"}));
    // Already being merged: GitHub refuses the dequeue and the merge is looked at again.
    queued(
        &mock,
        Some("LOCKED"),
        Some(("AddedToMergeQueueEvent", "", AFTER)),
    );
    mock.reply(
        200,
        vec![],
        json!({"errors": [{"type": "UNPROCESSABLE", "message": "cannot dequeue a locked entry"}]}),
    );
    assert!(matches!(merge(&delivery, true, 0).await, Step::Retry(_)));
    // Not in the queue: nothing is enqueued once the wait is over.
    queued(&mock, None, None);
    assert_eq!(
        done(merge(&delivery, true, 0).await).outcome,
        "not_mergeable"
    );
    assert_eq!(graphql_bodies(&mock).len(), 5);
    assert!(git.operations().is_empty());
}
