use super::*;

async fn merge(app: &GitHubApp, method: MergeMethod) -> MergeOutcome {
    app.merge(&repo(), 7, method, &head(), "main", QueueWait::default())
        .await
        .unwrap()
}
async fn refused_by_policy(app: &GitHubApp, mock: &Mock) {
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    assert_eq!(
        merge(app, MergeMethod::Rebase).await,
        MergeOutcome::Conflict
    );
    assert_eq!(requests(mock).len(), 1, "a conflict makes no merge call");

    mock.reply(200, vec![], pull(7, json!({"mergeable_state": "blocked"})));
    no_queue(mock);
    mock.reply(
        405,
        vec![],
        json!({"message": "Required status check \"ci\" is expected."}),
    );
    mock.reply(200, vec![], pull(7, json!({"mergeable_state": "blocked"})));
    assert_eq!(
        merge(app, MergeMethod::Rebase).await,
        MergeOutcome::NotMergeable {
            message: "Required status check \"ci\" is expected.".into()
        }
    );
    mock.reply(200, vec![], pull(7, json!({})));
    no_queue(mock);
    mock.reply(
        422,
        vec![],
        json!({"message": "Rebase merges are not allowed on this repository."}),
    );
    assert!(matches!(
        merge(app, MergeMethod::Rebase).await,
        MergeOutcome::Refused {
            reason: MergeRefusal::Invalid,
            ..
        }
    ));
}

async fn already_closed(app: &GitHubApp, mock: &Mock) {
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    assert!(matches!(
        merge(app, MergeMethod::Rebase).await,
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
        merge(app, MergeMethod::Rebase).await,
        MergeOutcome::Merged {
            sha: "e".repeat(40)
        },
        "already merged at the expected head"
    );
}

async fn rate_limited(app: &GitHubApp, mock: &Mock) {
    mock.reply(200, vec![], pull(7, json!({})));
    no_queue(mock);
    mock.reply(
        403,
        vec![("retry-after", "60".into())],
        json!({"message": "secondary rate limit"}),
    );
    let err = app
        .merge(
            &repo(),
            7,
            MergeMethod::Merge,
            &head(),
            "main",
            QueueWait::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "retryable");
    let puts = requests(mock).iter().filter(|(m, _)| m == "PUT").count();
    assert_eq!(puts, 3, "one merge call per attempt that reached GitHub");
}

async fn confirmed_behind(app: &GitHubApp, mock: &Mock) {
    let behind = json!({"mergeable_state": "behind"});
    mock.reply(200, vec![], pull(7, behind.clone()));
    no_queue(mock);
    assert_eq!(merge(app, MergeMethod::Merge).await, MergeOutcome::Behind);
    assert_eq!(requests(mock), [req("GET", "/pulls/7"), graphql()]);

    for (status, body) in [
        (405, json!({"message": "Merge cannot be performed"})),
        (
            200,
            json!({"merged": false, "message": "Merge cannot be performed"}),
        ),
    ] {
        let start = requests(mock).len();
        mock.reply(200, vec![], pull(7, json!({})));
        no_queue(mock);
        mock.reply(status, vec![], body);
        mock.reply(200, vec![], pull(7, behind.clone()));
        assert_eq!(merge(app, MergeMethod::Merge).await, MergeOutcome::Behind);
        assert_eq!(
            requests(mock)[start..],
            [
                req("GET", "/pulls/7"),
                graphql(),
                req("PUT", "/pulls/7/merge"),
                req("GET", "/pulls/7")
            ]
        );
    }
}

async fn unconfirmed_behind(app: &GitHubApp, mock: &Mock) {
    for extra in [
        json!({"mergeable_state": "blocked"}),
        json!({"mergeable_state": "behind", "base": {"ref": "other", "sha": head()}}),
        json!({"mergeable_state": "behind", "mergeable": null}),
    ] {
        mock.reply(200, vec![], pull(7, json!({})));
        no_queue(mock);
        mock.reply(
            405,
            vec![],
            json!({"message": "Required review is missing"}),
        );
        mock.reply(200, vec![], pull(7, extra));
        assert_eq!(
            merge(app, MergeMethod::Merge).await,
            MergeOutcome::NotMergeable {
                message: "Required review is missing".into()
            }
        );
    }
}

async fn moved_head(app: &GitHubApp, mock: &Mock) {
    mock.reply(200, vec![], pull(7, json!({})));
    no_queue(mock);
    mock.reply(405, vec![], json!({"message": "Merge cannot be performed"}));
    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({"mergeable_state": "behind", "head": {"ref": "aq/task", "sha": moved()}}),
        ),
    );
    assert!(matches!(
        merge(app, MergeMethod::Merge).await,
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            ..
        }
    ));

    mock.reply(200, vec![], pull(7, json!({})));
    no_queue(mock);
    mock.reply(409, vec![], json!({"message": "The head has changed"}));
    assert!(matches!(
        merge(app, MergeMethod::Merge).await,
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            ..
        }
    ));
    assert_eq!(
        requests(mock).last().unwrap(),
        &req("PUT", "/pulls/7/merge")
    );
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
        (json!({"mergeable_state": "draft"}), Mergeability::Blocked),
        (json!({"mergeable_state": "behind"}), Mergeability::Behind),
        (
            json!({"mergeable": null, "mergeable_state": "behind"}),
            Mergeability::Unknown,
        ),
        (
            json!({"mergeable": null, "mergeable_state": "unknown"}),
            Mergeability::Unknown,
        ),
        (json!({"state": "closed"}), Mergeability::Blocked),
    ] {
        mock.reply(200, vec![], pull(7, extra.clone()));
        assert_eq!(
            app.pull_request(&repo(), 7).await.unwrap().mergeability,
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
    no_queue(&mock);
    mock.reply(
        200,
        vec![],
        json!({"sha": "d".repeat(40), "merged": true, "message": "Pull Request successfully merged"}),
    );
    let outcome = app
        .merge(
            &repo(),
            7,
            MergeMethod::Squash,
            &head(),
            "main",
            QueueWait::default(),
        )
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
        [
            req("GET", "/pulls/7"),
            graphql(),
            req("PUT", "/pulls/7/merge")
        ]
    );
    assert_eq!(
        mock.seen()[3].body,
        json!({"merge_method": "squash", "sha": head()})
    );
}

#[tokio::test]
async fn retargeted_pull_request_is_refused_before_merge() {
    let (app, mock) = ready().await;
    mock.reply(200, vec![], pull(7, json!({})));
    let outcome = app
        .merge(
            &repo(),
            7,
            MergeMethod::Merge,
            &head(),
            "release",
            QueueWait::default(),
        )
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
        .merge(
            &repo(),
            7,
            MergeMethod::Merge,
            &head(),
            "main",
            QueueWait::default(),
        )
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
    no_queue(&mock);
    mock.reply(
        409,
        vec![],
        json!({"message": "Head branch was modified. Review and try the merge again."}),
    );
    let outcome = app
        .merge(
            &repo(),
            7,
            MergeMethod::Merge,
            &head(),
            "main",
            QueueWait::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        MergeOutcome::Refused {
            reason: MergeRefusal::HeadMoved,
            message: "Head branch was modified. Review and try the merge again.".into()
        }
    );
    assert_eq!(requests(&mock).len(), 4, "no retry after the refusal");
    let err = app
        .merge(
            &repo(),
            7,
            MergeMethod::Merge,
            "HEAD",
            "main",
            QueueWait::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "github_invalid_input");
}

#[tokio::test]
async fn merge_outcomes_for_conflict_not_mergeable_closed_and_merged() {
    let (app, mock) = ready().await;
    refused_by_policy(&app, &mock).await;
    already_closed(&app, &mock).await;
    rate_limited(&app, &mock).await;
}

#[tokio::test]
async fn a_declined_merge_repairs_only_a_confirmed_behind_head_on_the_tracked_base() {
    let (app, mock) = ready().await;
    confirmed_behind(&app, &mock).await;
    unconfirmed_behind(&app, &mock).await;
    moved_head(&app, &mock).await;
}
