//! Pull-request operations against the scripted GitHub stand-in.
use super::*;
use crate::policy::{
    BranchDeletion, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod, MergeOutcome,
    MergeRefusal, Mergeability, PullRequestState,
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
    let pr = app
        .open_or_find(&repo(), "refs/heads/aq/task-1", "main", "Task 1", "body")
        .await
        .unwrap();
    assert_eq!(pr.number, 7);
    assert_eq!(pr.url, "https://github.com/Acme/widgets/pull/7");
    assert_eq!(pr.head_sha, head());
    assert_eq!(pr.state, PullRequestState::Open);
    assert_eq!(requests(&mock), [req("GET", "/pulls")]);
    let seen = mock.seen();
    assert_eq!(
        seen[1].query.as_deref(),
        Some("state=open&head=Acme%3Aaq%2Ftask-1&base=main")
    );
    assert_eq!(seen[1].headers["authorization"], "Bearer ghs_pulls");
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
async fn checks_require_what_rulesets_and_branch_protection_require() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!([
            {"type": "required_status_checks", "ruleset_id": 1,
             "parameters": {"strict_required_status_checks_policy": false,
                            "required_status_checks": [{"context": "ci/test", "integration_id": 15368}]}},
            {"type": "deletion", "ruleset_id": 1}
        ]),
    );
    mock.reply(
        200,
        vec![],
        json!({"name": "main", "protected": true, "protection": {"enabled": true,
            "required_status_checks": {"enforcement_level": "everyone",
                "contexts": ["lint"], "checks": [{"context": "lint", "app_id": null}]}}}),
    );
    let mut check_runs = runs(&[
        ("ci/test", "completed", Some("success")),
        ("optional", "completed", Some("failure")),
    ]);
    check_runs["check_runs"][0]["app"] = json!({"id": 15368});
    mock.reply(200, vec![], check_runs);
    mock.reply(200, vec![], statuses(&[("lint", "success")]));
    let checks = app.checks(&repo(), 7, None).await.unwrap();
    assert_eq!(
        checks.state,
        ChecksState::Passed,
        "unrequired failure ignored"
    );
    assert_eq!(checks.required, ["ci/test", "lint"]);
    assert!(checks.failed.is_empty());
    assert_eq!(checks.head_sha, head());
    let sha = head();
    assert_eq!(
        requests(&mock),
        [
            req("GET", "/pulls/7"),
            req("GET", "/rules/branches/main"),
            req("GET", "/branches/main"),
            req("GET", &format!("/commits/{sha}/check-runs")),
            req("GET", &format!("/commits/{sha}/status")),
        ]
    );
    assert_eq!(mock.seen()[2].query.as_deref(), Some("per_page=100&page=1"));
}

#[tokio::test]
async fn checks_report_pending_failed_and_passed_for_an_explicit_list() {
    let (app, mock) = ready().await;
    let required = ["build".to_string(), "deploy/preview".to_string()];
    let check = async |check_runs: Value, commit_statuses: Value| {
        mock.reply(200, vec![], pull(7, json!({})));
        mock.reply(200, vec![], json!([]));
        mock.reply(200, vec![], json!({"name": "main", "protected": false}));
        mock.reply(200, vec![], check_runs);
        mock.reply(200, vec![], commit_statuses);
        app.checks(&repo(), 7, Some(&required[..])).await.unwrap()
    };
    // A required check still running.
    let c = check(
        runs(&[("build", "in_progress", None)]),
        statuses(&[("deploy/preview", "success")]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Pending);
    assert_eq!(c.required, ["build", "deploy/preview"]);
    // A required check that has not reported yet.
    let c = check(
        runs(&[("build", "completed", Some("success"))]),
        statuses(&[]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Pending);
    // A required failure wins over a pending one, and names it.
    let c = check(
        runs(&[("build", "completed", Some("timed_out"))]),
        statuses(&[("deploy/preview", "pending")]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Failed);
    assert_eq!(c.failed, ["build"]);
    // Every required check passed; skipped and neutral count as passed.
    let c = check(
        runs(&[
            ("build", "completed", Some("neutral")),
            ("other", "completed", Some("failure")),
        ]),
        statuses(&[("deploy/preview", "success")]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Passed);
    assert!(c.failed.is_empty());
}

#[tokio::test]
async fn explicit_checks_preserve_protected_producers_for_the_same_name() {
    let (app, mock) = ready().await;
    let required = ["ci".to_string(), "project".to_string()];
    let check =
        async |reports: &[(Option<u64>, &str)]| {
            mock.reply(200, vec![], pull(7, json!({})));
            mock.reply(
                200,
                vec![],
                json!([
                    {"type": "required_status_checks", "parameters": {
                        "required_status_checks": [{"context": "ci", "integration_id": 1}]
                    }}
                ]),
            );
            mock.reply(200, vec![], json!({"protected": true, "protection": {
            "required_status_checks": {
                "contexts": ["ci", "legacy"],
                "checks": [{"context": "ci", "app_id": 2}, {"context": "legacy", "app_id": -1}]
            }
        }}));
            let checks: Vec<_> = reports
                .iter()
                .map(|(id, conclusion)| {
                    json!({"name": "ci", "status": "completed", "conclusion": conclusion,
                   "app": id.map(|id| json!({"id": id}))})
                })
                .collect();
            mock.reply(
                200,
                vec![],
                json!({"total_count": checks.len(), "check_runs": checks}),
            );
            mock.reply(
                200,
                vec![],
                statuses(&[
                    ("ci", "success"),
                    ("legacy", "success"),
                    ("project", "success"),
                ]),
            );
            app.checks(&repo(), 7, Some(&required)).await.unwrap()
        };

    for reports in [
        &[][..],
        &[(None, "success")][..],
        &[(Some(3), "success")][..],
        &[(Some(1), "success")][..],
        &[(Some(2), "success")][..],
    ] {
        let checks = check(reports).await;
        assert_eq!(checks.state, ChecksState::Pending, "{reports:?}");
        assert_eq!(checks.required, ["ci", "legacy", "project"]);
        assert!(checks.failed.is_empty());
    }

    let checks = check(&[
        (Some(1), "success"),
        (Some(2), "success"),
        (Some(3), "failure"),
    ])
    .await;
    assert_eq!(checks.state, ChecksState::Passed);
    assert!(checks.failed.is_empty());

    let checks = check(&[
        (Some(1), "success"),
        (Some(1), "failure"),
        (Some(2), "failure"),
    ])
    .await;
    assert_eq!(checks.state, ChecksState::Failed);
    assert_eq!(checks.failed, ["ci"]);
}

#[tokio::test]
async fn checks_without_required_names_count_every_reported_check() {
    let (app, mock) = ready().await;
    let check = async |check_runs: Value, commit_statuses: Value| {
        mock.reply(200, vec![], pull(7, json!({})));
        mock.reply(200, vec![], json!([]));
        mock.reply(200, vec![], json!({"name": "main", "protected": false}));
        mock.reply(200, vec![], check_runs);
        mock.reply(200, vec![], commit_statuses);
        app.checks(&repo(), 7, None).await.unwrap()
    };
    let c = check(runs(&[]), statuses(&[])).await;
    assert_eq!(c.state, ChecksState::Pending);
    assert!(c.required.is_empty());
    assert!(c.failed.is_empty());

    let c = check(
        runs(&[("a", "completed", Some("success")), ("b", "queued", None)]),
        statuses(&[]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Pending);
    assert!(c.required.is_empty());
    let c = check(
        runs(&[("a", "completed", Some("success"))]),
        statuses(&[("ci", "error"), ("lint", "success")]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Failed);
    assert_eq!(c.failed, ["ci"]);
    // The same name as a check run and a status keeps the worse state.
    let c = check(
        runs(&[("ci", "completed", Some("success"))]),
        statuses(&[("ci", "failure")]),
    )
    .await;
    assert_eq!(c.failed, ["ci"]);
    let c = check(
        runs(&[("a", "completed", Some("success"))]),
        statuses(&[("lint", "success")]),
    )
    .await;
    assert_eq!(c.state, ChecksState::Passed);
}

#[tokio::test]
async fn check_runs_are_read_across_pages() {
    let (app, mock) = ready().await;
    let names: Vec<String> = (0..100).map(|i| format!("job-{i}")).collect();
    let page: Vec<(&str, &str, Option<&str>)> = names
        .iter()
        .map(|n| (n.as_str(), "completed", Some("success")))
        .collect();
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(200, vec![], json!([]));
    mock.reply(200, vec![], json!({"name": "main", "protected": false}));
    mock.reply(200, vec![], runs(&page));
    mock.reply(200, vec![], runs(&[("late", "completed", Some("failure"))]));
    mock.reply(200, vec![], statuses(&[]));
    let c = app.checks(&repo(), 7, Some(&[][..])).await.unwrap();
    assert_eq!(c.failed, ["late"]);
    let seen = mock.seen();
    assert_eq!(seen[5].query.as_deref(), Some("per_page=100&page=2"));
    assert_eq!(seen.len(), 7);
}

#[tokio::test]
async fn mergeability_maps_github_lazy_state() {
    let (app, mock) = ready().await;
    for (extra, expected) in [
        (json!({}), Mergeability::Clean),
        (json!({"mergeable_state": "unstable"}), Mergeability::Clean),
        (
            json!({"mergeable": false, "mergeable_state": "dirty"}),
            Mergeability::Conflict,
        ),
        (json!({"mergeable_state": "blocked"}), Mergeability::Blocked),
        (json!({"mergeable_state": "behind"}), Mergeability::Blocked),
        (
            json!({"mergeable": null, "mergeable_state": "unknown"}),
            Mergeability::Unknown,
        ),
        (json!({"state": "closed"}), Mergeability::Blocked),
    ] {
        mock.reply(200, vec![], pull(7, extra.clone()));
        assert_eq!(
            app.mergeability(&repo(), 7).await.unwrap(),
            expected,
            "{extra}"
        );
    }
    assert!(
        requests(&mock).iter().all(|r| *r == req("GET", "/pulls/7")),
        "mergeability only reads"
    );
}

#[tokio::test]
async fn merge_sends_the_method_and_expected_sha() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!({"sha": "d".repeat(40), "merged": true, "message": "Pull Request successfully merged"}),
    );
    let outcome = app
        .merge(&repo(), 7, MergeMethod::Squash, &head(), "main")
        .await
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Merged {
            sha: "d".repeat(40)
        }
    );
    assert_eq!(
        requests(&mock),
        [req("GET", "/pulls/7"), req("PUT", "/pulls/7/merge")]
    );
    assert_eq!(
        mock.seen()[2].body,
        json!({"merge_method": "squash", "sha": head()})
    );
}

#[tokio::test]
async fn retargeted_pull_request_is_refused_before_merge() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({})));
    let outcome = app
        .merge(&repo(), 7, MergeMethod::Merge, &head(), "release")
        .await
        .unwrap();
    assert!(matches!(outcome, MergeOutcome::NotMergeable { .. }));
    assert_eq!(requests(&mock).len(), 1);
}

#[tokio::test]
async fn merge_with_a_stale_sha_is_refused() {
    let (app, mock) = ready().await;
    // The head has moved before the merge: refused without a merge call.
    mock.reply(
        200,
        vec![],
        pull(7, json!({"head": {"ref": "aq/task-1", "sha": moved()}})),
    );
    let outcome = app
        .merge(&repo(), 7, MergeMethod::Merge, &head(), "main")
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            ..
        }
    ));
    assert_eq!(requests(&mock), [req("GET", "/pulls/7")]);
    // The head moves between the read and the merge: GitHub's 409 refuses.
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        409,
        vec![],
        json!({"message": "Head branch was modified. Review and try the merge again."}),
    );
    let outcome = app
        .merge(&repo(), 7, MergeMethod::Merge, &head(), "main")
        .await
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message: "Head branch was modified. Review and try the merge again.".into()
        }
    );
    assert_eq!(requests(&mock).len(), 3, "no retry after the refusal");
    let err = app
        .merge(&repo(), 7, MergeMethod::Merge, "HEAD", "main")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "github_invalid_input");
}

#[tokio::test]
async fn merge_outcomes_for_conflict_not_mergeable_closed_and_merged() {
    let (app, mock) = ready().await;
    let merge = async || {
        app.merge(&repo(), 7, MergeMethod::Rebase, &head(), "main")
            .await
            .unwrap()
    };
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    assert_eq!(merge().await, MergeOutcome::Conflict);
    assert_eq!(requests(&mock).len(), 1, "a conflict makes no merge call");

    mock.reply(200, vec![], pull(7, json!({"mergeable_state": "blocked"})));
    mock.reply(
        405,
        vec![],
        json!({"message": "Required status check \"ci\" is expected."}),
    );
    assert_eq!(
        merge().await,
        MergeOutcome::NotMergeable {
            message: "Required status check \"ci\" is expected.".into()
        }
    );
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        422,
        vec![],
        json!({"message": "Rebase merges are not allowed on this repository."}),
    );
    assert!(matches!(
        merge().await,
        MergeOutcome::Refused {
            reason: MergeRefusal::Invalid,
            ..
        }
    ));
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    assert!(matches!(
        merge().await,
        MergeOutcome::Refused {
            reason: MergeRefusal::Closed,
            ..
        }
    ));
    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({"state": "closed", "merged": true, "merge_commit_sha": "e".repeat(40)}),
        ),
    );
    assert_eq!(
        merge().await,
        MergeOutcome::Merged {
            sha: "e".repeat(40)
        },
        "already merged at the expected head"
    );
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        403,
        vec![("retry-after", "60".into())],
        json!({"message": "secondary rate limit"}),
    );
    let err = app
        .merge(&repo(), 7, MergeMethod::Merge, &head(), "main")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "retryable");
    let puts = requests(&mock).iter().filter(|(m, _)| m == "PUT").count();
    assert_eq!(puts, 3, "one merge call per attempt that reached GitHub");
}

#[tokio::test]
async fn close_patches_the_pull_request_state() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    let pr = app.close(&repo(), 7).await.unwrap();
    assert_eq!(pr.state, PullRequestState::Closed);
    assert_eq!(requests(&mock), [req("PATCH", "/pulls/7")]);
    assert_eq!(mock.seen()[1].body, json!({"state": "closed"}));
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
    assert_eq!(delete().await.unwrap(), BranchDeletion::Deleted);
    assert_eq!(delete().await.unwrap(), BranchDeletion::AlreadyAbsent);
    assert_eq!(delete().await.unwrap(), BranchDeletion::AlreadyAbsent);
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
        app.mergeability(&bad, 7).await.unwrap_err().code(),
        "github_invalid_input"
    );
    assert_eq!(
        app.delete_branch(&bad, "aq/x").await.unwrap_err().code(),
        "github_invalid_input"
    );
    assert!(mock.seen().is_empty());
}
