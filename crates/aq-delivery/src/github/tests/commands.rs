//! The delivery commands against the scripted GitHub stand-in and a fake
//! aq-git on a unix socket: one completion per outcome.
use super::*;
use crate::commands::{
    CheckPoll, CheckRequest, Completion, Delivery, DeliveryTarget, FrozenCommand, GitPort,
    GitTarget, MergeGuard, OpenPullRequest, SocketGit, Step, TaskOperation, bounded_wait_ms,
    task_branch,
};
use crate::policy::{GitHubRepository, MergeMethod};
use std::path::PathBuf;
use uuid::Uuid;

mod fixture;
use fixture::*;
mod branches;
mod checks;
mod frozen;
mod logs;
mod merges;
mod queue;
mod recovery;
mod refresh;
mod repair;

async fn open(delivery: &Delivery) -> Completion {
    done(
        delivery
            .pull_request_open(
                &target(),
                TASK,
                OpenPullRequest {
                    title: "Task",
                    body: "Report",
                    known_pr: None,
                },
            )
            .await,
    )
}
#[tokio::test]
async fn pull_request_open_finds_or_opens_and_refuses_an_unpublished_branch() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], json!([pull(7, json!({}))]));
    mock.reply(200, vec![], pull(7, json!({})));
    let c = open(&delivery).await;
    assert_eq!(
        (c.outcome, c.output, c.pull_request.unwrap().state),
        (
            "opened",
            json!({"number":7,"url":"https://github.com/Acme/widgets/pull/7"}),
            "open"
        )
    );
    assert_eq!(
        mock.seen()[1].query.as_deref(),
        Some(format!("state=all&head=Acme%3Aaq%2F{TASK}&base=main").as_str())
    );
    // None open: opened now.
    mock.reply(200, vec![], json!([]));
    mock.reply(201, vec![], pull(8, json!({})));
    let c = open(&delivery).await;
    assert_eq!(c.output["number"], 8);
    assert_eq!(
        mock.seen().last().unwrap().body,
        json!({"title": "Task", "head": format!("aq/{TASK}"), "base": "main", "body": "Report"})
    );
    // The branch is not on GitHub: 422.
    mock.reply(200, vec![], json!([]));
    mock.reply(422, vec![], json!({"message": "Validation Failed"}));
    let c = open(&delivery).await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "not_published"}))
    );
    // GitHub does not answer.
    mock.reply(503, vec![], json!({"message": "down"}));
    let c = open(&delivery).await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
}

#[tokio::test]
async fn close_closes_the_pull_request_and_deletes_the_branch() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    let c = done(
        delivery
            .pull_request_close(TaskOperation::new(Uuid::from_u128(8), &target(), TASK), 7)
            .await,
    );
    assert_eq!(
        (c.outcome, c.output, c.pull_request.unwrap().state),
        ("closed", json!({}), "closed")
    );
    assert_eq!(
        paths(&mock),
        [("PATCH".to_string(), "/pulls/7".to_string())]
    );
    assert_eq!(git.operations(), ["ref.delete"]);
    // GitHub down: the close is retried, the branch is left alone.
    mock.reply(503, vec![], json!({"message": "down"}));
    assert!(matches!(
        delivery
            .pull_request_close(TaskOperation::new(Uuid::from_u128(8), &target(), TASK), 7)
            .await,
        Step::Retry(_)
    ));
    assert_eq!(git.operations(), ["ref.delete"]);
}
mod retained_publication;
