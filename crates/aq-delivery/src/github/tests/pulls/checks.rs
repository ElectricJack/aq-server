use super::*;

fn protected_reports(mock: &Mock, reports: &[(Option<u64>, &str)]) {
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
    mock.reply(
        200,
        vec![],
        json!({"protected": true, "protection": {
            "required_status_checks": {
                "contexts": ["ci", "legacy"],
                "checks": [{"context": "ci", "app_id": 2}, {"context": "legacy", "app_id": -1}]
            }
        }}),
    );
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
        (checks.state, checks.pull_request.head_sha),
        (ChecksState::Passed, head()),
        "unrequired failure ignored"
    );
    assert_eq!(
        (checks.required, checks.failed),
        (["ci/test", "lint"].map(str::to_owned).to_vec(), Vec::new())
    );
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
    let check = async |reports: &[(Option<u64>, &str)]| {
        protected_reports(&mock, reports);
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
