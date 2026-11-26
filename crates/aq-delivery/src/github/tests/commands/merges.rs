use super::*;

#[tokio::test]
async fn merge_merges_then_deletes_the_branch_on_the_remote_and_in_the_pool() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!({"sha": sha('d'), "merged": true, "message": "ok"}),
    );
    let c = done(
        delivery
            .pull_request_merge(
                TaskOperation::new(Uuid::from_u128(6), &target(), TASK),
                7,
                MergeGuard {
                    method: MergeMethod::Merge,
                    expected_head: &sha('a'),
                    result_head: None,
                },
            )
            .await,
    );
    assert_eq!(
        (c.outcome, c.output, c.pull_request.unwrap().state),
        ("merged", json!({}), "merged")
    );
    assert_eq!(
        mock.seen()[2].body,
        json!({"merge_method": "merge", "sha": sha('a')})
    );
    assert_eq!(git.operations(), ["ref.delete"]);
    assert_eq!(
        git.request(0)["params"],
        json!({"ref": task_branch(TASK), "remote": true, "expected_old": {"algorithm": "sha1", "hex": sha('a')}})
    );
}

#[tokio::test]
async fn merge_outcomes_other_than_merged_delete_nothing() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        done(
            delivery
                .pull_request_merge(
                    TaskOperation::new(Uuid::from_u128(6), &target(), TASK),
                    7,
                    MergeGuard {
                        method: MergeMethod::Squash,
                        expected_head: &sha('a'),
                        result_head: None,
                    },
                )
                .await,
        )
    };
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    assert_eq!(merge().await.outcome, "conflict");
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        405,
        vec![],
        json!({"message": "Required review is missing"}),
    );
    mock.reply(200, vec![], pull(7, json!({"mergeable_state": "blocked"})));
    assert_eq!(merge().await.outcome, "not_mergeable");
    mock.reply(
        200,
        vec![],
        pull(7, json!({"head": {"ref": "x", "sha": sha('b')}})),
    );
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "head_changed"}))
    );
    mock.reply(404, vec![], json!({"message": "Not Found"}));
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "no_pull_request"}))
    );
    mock.reply(403, vec![], json!({"message": "forbidden"}));
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn a_merged_pull_request_whose_branch_is_not_deleted_yet_is_retried_not_refused() {
    let healthy_after = Arc::new(Mutex::new(false));
    let flag = healthy_after.clone();
    let git = FakeGit::start(move |r| {
        if *flag.lock().unwrap() {
            healthy(r)
        } else {
            json!({"error": {"code": "io_error", "message": "busy", "retry": "safe_same_operation"}})
        }
    });
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        delivery
            .pull_request_merge(
                TaskOperation::new(Uuid::from_u128(6), &target(), TASK),
                7,
                MergeGuard {
                    method: MergeMethod::Merge,
                    expected_head: &sha('a'),
                    result_head: None,
                },
            )
            .await
    };
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!({"sha": sha('d'), "merged": true, "message": "ok"}),
    );
    assert!(matches!(merge().await, Step::Retry(_)));
    // The retry finds the pull request merged at the expected head: no second
    // merge, and the deletion goes through with the same operation id.
    *healthy_after.lock().unwrap() = true;
    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({"state": "closed", "merged": true, "merge_commit_sha": sha('d')}),
        ),
    );
    assert_eq!(done(merge().await).outcome, "merged");
    assert_eq!(
        git.request(0)["operation_id"],
        git.request(1)["operation_id"]
    );
    let puts = paths(&mock).into_iter().filter(|(m, _)| m == "PUT").count();
    assert_eq!(puts, 1, "merged once");
}

#[tokio::test]
async fn merge_recovers_after_an_unknown_reply() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        delivery
            .pull_request_merge(
                TaskOperation::new(Uuid::from_u128(6), &target(), TASK),
                7,
                MergeGuard {
                    method: MergeMethod::Merge,
                    expected_head: &sha('a'),
                    result_head: None,
                },
            )
            .await
    };
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(503, vec![], json!({"message": "reply lost"}));
    assert!(matches!(merge().await, Step::Retry(_)));
    assert!(git.operations().is_empty());

    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({
                "state": "closed", "merged": true, "merge_commit_sha": sha('d')
            }),
        ),
    );
    let completion = done(merge().await);
    assert_eq!(
        (completion.outcome, completion.output),
        ("merged", json!({}))
    );
    assert_eq!(
        paths(&mock),
        [
            ("GET".into(), "/pulls/7".into()),
            ("PUT".into(), "/pulls/7/merge".into()),
            ("GET".into(), "/pulls/7".into()),
        ]
    );
    assert_eq!(git.operations(), ["ref.delete"]);
}

#[tokio::test]
async fn a_merged_pr_is_recovered_only_at_the_checked_head_without_another_forge_write() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let target = target();
    mock.reply(200, vec![], recovery::merged('b'));
    assert_eq!(
        done(
            recovery::poll_merged(&delivery, &target)
                .await
                .step
                .unwrap()
        )
        .outcome,
        "timeout"
    );
    mock.reply(200, vec![], recovery::merged('b'));
    assert_eq!(
        recovery::reconcile(&delivery, &target).await.output["reason"],
        "head_changed"
    );
    assert!(git.operations().is_empty());

    poll(
        &mock,
        recovery::merged('a'),
        recovery::merged('a'),
        runs(&[("check", "completed", Some("success"))]),
    );
    assert_eq!(
        done(
            recovery::poll_merged(&delivery, &target)
                .await
                .step
                .unwrap()
        )
        .outcome,
        "timeout"
    );
    assert!(git.operations().is_empty());

    // The forge can report the merge on the second read, after another PR
    // advances main. Green checks on the frozen result still permit cleanup.
    let mut recovered = recovery::merged('a');
    recovered["base"]["sha"] = json!(sha('e'));
    poll(
        &mock,
        pull(7, json!({})),
        recovered,
        runs(&[
            ("check", "completed", Some("success")),
            ("deployment", "completed", Some("success")),
        ]),
    );
    let checked = recovery::poll_merged(&delivery, &target).await;
    assert_eq!(done(checked.step.unwrap()).outcome, "passed");
    assert_eq!(checked.observed.unwrap().state, "merged");
    mock.reply(200, vec![], recovery::merged('a'));
    assert_eq!(
        recovery::reconcile(&delivery, &target).await.outcome,
        "merged"
    );

    assert!(paths(&mock).iter().all(|(method, _)| method == "GET"));
    assert_eq!(git.operations(), ["ref.delete"]);
    assert_eq!(git.request(0)["params"]["expected_old"]["hex"], sha('a'));
}

#[tokio::test]
async fn verifies_only_an_external_merge_of_the_exact_head_and_target_without_writes() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let target = target();
    let merged = |values: Value| {
        let mut values = values.as_object().cloned().unwrap();
        values.insert("state".into(), json!("closed"));
        values.insert("merged".into(), json!(true));
        values.insert("merge_commit_sha".into(), json!(sha('d')));
        pull(7, Value::Object(values))
    };
    for (answer, expected) in [
        (pull(7, json!({})), false),
        (
            merged(json!({"head":{"ref":"aq/task","sha":sha('b')}})),
            false,
        ),
        (
            merged(json!({"base":{"ref":"release","sha":sha('c')}})),
            false,
        ),
        (merged(json!({})), true),
    ] {
        mock.reply(200, vec![], answer);
        let evidence = delivery
            .verified_external_merge(&target, 7, &sha('a'))
            .await
            .unwrap();
        assert_eq!(evidence.is_some(), expected);
    }
    assert_eq!(paths(&mock), vec![("GET".into(), "/pulls/7".into()); 4]);
    assert!(git.operations().is_empty());
}
