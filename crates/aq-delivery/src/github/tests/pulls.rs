//! Pull-request operations against the scripted GitHub stand-in.
use super::*;
use crate::policy::{
    BranchDeletion, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod, MergeOutcome,
    MergeRefusal, Mergeability, PullRequestState, QueueWait,
};

fn head() -> String {
    "a".repeat(40)
}
fn moved() -> String {
    "b".repeat(40)
}
fn repo() -> GitHubRepository {
    GitHubRepository {
        owner: "Acme".into(),
        name: "widgets".into(),
    }
}
fn pull(number: u64, extra: Value) -> Value {
    let mut v = json!({
        "number": number,
        "html_url": format!("https://github.com/Acme/widgets/pull/{number}"),
        "state": "open",
        "merged": false,
        "merged_at": null,
        "mergeable": true,
        "mergeable_state": "clean",
        "head": {"ref": "aq/task-1", "sha": head()},
        "base": {"ref": "main", "sha": "c".repeat(40)},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}
/// The App with a scripted installation token for `Acme/widgets`.
async fn ready() -> (GitHubApp, Mock) {
    let (app, mock) = app().await;
    mock.token("ghs_pulls", Utc::now() + Span::hours(1));
    (app, mock)
}
fn requests(mock: &Mock) -> Vec<(String, String)> {
    mock.seen()
        .iter()
        .skip(1)
        .map(|s| (s.method.clone(), s.path.clone()))
        .collect()
}
fn graphql() -> (String, String) {
    ("POST".into(), "/api/graphql".into())
}
fn req(method: &str, rest: &str) -> (String, String) {
    (method.into(), format!("/api/v3/repos/Acme/widgets{rest}"))
}
fn runs(runs: &[(&str, &str, Option<&str>)]) -> Value {
    let runs: Vec<Value> = runs
        .iter()
        .map(|(name, status, conclusion)| {
            json!({"name": name, "status": status, "conclusion": conclusion})
        })
        .collect();
    json!({"total_count": runs.len(), "check_runs": runs})
}
fn statuses(statuses: &[(&str, &str)]) -> Value {
    let statuses: Vec<Value> = statuses
        .iter()
        .map(|(context, state)| json!({"context": context, "state": state}))
        .collect();
    json!({"state": "pending", "total_count": statuses.len(), "statuses": statuses})
}

#[tokio::test]
async fn open_or_find_returns_the_open_pull_request_for_the_head() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], json!([pull(7, json!({}))]));
    mock.reply(200, vec![], pull(7, json!({})));
    let pr = app
        .open_or_find(&repo(), "refs/heads/aq/task-1", "main", "Task 1", "body")
        .await
        .unwrap();
    assert_eq!(
        (pr.number, pr.url.as_str(), pr.head_sha, pr.state),
        (
            7,
            "https://github.com/Acme/widgets/pull/7",
            head(),
            PullRequestState::Open
        )
    );
    assert_eq!(
        requests(&mock),
        [req("GET", "/pulls"), req("GET", "/pulls/7")]
    );
    let seen = mock.seen();
    assert_eq!(
        seen[1].query.as_deref(),
        Some("state=all&head=Acme%3Aaq%2Ftask-1&base=main")
    );
    assert_eq!(seen[1].headers["authorization"], "Bearer ghs_pulls");
}

#[tokio::test]
async fn a_closed_task_pr_is_reconciled_without_creating_a_replacement() {
    for (merged, expected) in [
        (false, PullRequestState::Closed),
        (true, PullRequestState::Merged),
    ] {
        let (app, mock) = ready().await;
        mock.reply(200, vec![], json!([pull(7, json!({"state": "closed"}))]));
        mock.reply(
            200,
            vec![],
            pull(7, json!({"state": "closed", "merged": merged})),
        );
        let pr = app
            .open_or_find(&repo(), "aq/task-1", "main", "Task 1", "Report")
            .await
            .unwrap();
        assert_eq!((pr.number, pr.state), (7, expected));
        assert_eq!(
            requests(&mock),
            [req("GET", "/pulls"), req("GET", "/pulls/7")]
        );
    }
}

#[tokio::test]
async fn open_or_find_opens_a_pull_request_when_none_is_open() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], json!([]));
    mock.reply(201, vec![], pull(8, json!({})));
    let pr = app
        .open_or_find(&repo(), "aq/task-1", "main", "Task 1", "Report")
        .await
        .unwrap();
    assert_eq!(pr.number, 8);
    assert_eq!(
        requests(&mock),
        [req("GET", "/pulls"), req("POST", "/pulls")]
    );
    assert_eq!(
        mock.seen()[2].body,
        json!({"title": "Task 1", "head": "aq/task-1", "base": "main", "body": "Report"})
    );
}

#[tokio::test]
async fn open_or_find_refusals_are_typed_and_not_retried() {
    let (app, mock) = ready().await;
    assert_invalid_open(&app, &mock).await;
    mock.reply(200, vec![], json!([]));
    mock.reply(
        422,
        vec![],
        json!({"message": "Validation Failed", "errors": [{"message": "No commits between main and aq/task-1"}]}),
    );
    let err = app
        .open_or_find(&repo(), "aq/task-1", "main", "Task 1", "")
        .await
        .unwrap_err();
    assert_eq!(
        err,
        GitHubApiError::Invalid {
            message: "Validation Failed; No commits between main and aq/task-1".into()
        }
    );
    assert_eq!(mock.seen().len(), 3, "token, find, open; no retry");
}

#[tokio::test]
async fn close_patches_the_pull_request_state() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    let pr = app.close(&repo(), 7).await.unwrap();
    assert_eq!(
        (pr.state, requests(&mock), &mock.seen()[1].body),
        (
            PullRequestState::Closed,
            vec![req("PATCH", "/pulls/7")],
            &json!({"state":"closed"})
        )
    );
    assert_eq!(
        app.close(&repo(), 0).await.unwrap_err().code(),
        "github_invalid_input"
    );
}

#[tokio::test]
async fn delete_branch_treats_a_missing_branch_as_success() {
    let (app, mock) = ready().await;
    mock.reply(204, vec![], Value::Null);
    mock.reply(404, vec![], json!({"message": "Not Found"}));
    mock.reply(422, vec![], json!({"message": "Reference does not exist"}));
    mock.reply(
        422,
        vec![],
        json!({"message": "Cannot delete protected branch"}),
    );
    let delete = async || app.delete_branch(&repo(), "refs/heads/aq/task-1").await;
    for expected in [
        BranchDeletion::Deleted,
        BranchDeletion::AlreadyAbsent,
        BranchDeletion::AlreadyAbsent,
    ] {
        assert_eq!(delete().await.unwrap(), expected);
    }
    assert_eq!(
        delete().await.unwrap_err(),
        GitHubApiError::Invalid {
            message: "Cannot delete protected branch".into()
        }
    );
    let expected = req("DELETE", "/git/refs/heads/aq/task-1");
    assert_eq!(requests(&mock), vec![expected; 4]);
}

#[tokio::test]
async fn operations_refuse_an_invalid_repository_without_calls() {
    let (app, mock) = app().await;
    let bad = GitHubRepository {
        owner: "Acme".into(),
        name: "../x".into(),
    };
    assert_eq!(
        app.pull_request(&bad, 7).await.unwrap_err().code(),
        "github_invalid_input"
    );
    assert_eq!(
        app.delete_branch(&bad, "aq/x").await.unwrap_err().code(),
        "github_invalid_input"
    );
    assert!(mock.seen().is_empty());
}

async fn assert_invalid_open(app: &GitHubApp, mock: &Mock) {
    for (head, base, title) in [
        ("aq/../x", "main", "t"),
        ("aq/task%2f", "main", "t"),
        ("aq/task-1", "refs/tags/v1", "t"),
        ("aq/task-1", "main", " "),
    ] {
        let err = app
            .open_or_find(&repo(), head, base, title, "")
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            "github_invalid_input",
            "{head} {base} {title:?}"
        );
    }
    assert!(mock.seen().is_empty(), "invalid input makes no call");
}

mod checks;
mod merges;
