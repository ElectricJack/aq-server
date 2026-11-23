use super::*;

#[tokio::test]
async fn independent_review_reads_the_assigned_result_revision_and_retained_commit() {
    use aq_playbook_domain::delivery_store::{self, DeliveryPort, PgDelivery};
    let target = Box::pin(super::super::auto_env(
        "not_required",
        axum::http::StatusCode::OK,
    ))
    .await;
    let pool = target.f.db.pool().await;
    let id = target.report.report_id;
    let pin: String =
        sqlx::query_scalar("SELECT report_revision FROM result_revision_pins WHERE report_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let lineage = aq_selection::TaskLineage {
        task_id: Uuid::new_v4(),
        ..target.f.selection.lineage.clone()
    };
    let reviewer = reviewer(&target, &lineage, &pin).await;
    let latest = PgDelivery(pool.clone())
        .latest(lineage.task_id)
        .await
        .unwrap()
        .unwrap();
    let version = reviewer.expected_version;
    let graph = json!({"version":version,"labels":["review-only"],"body":format!("Review target: {id}@{pin}")});
    let observed = |node: Value| {
        let pool = pool.clone();
        let latest = latest.clone();
        let lineage = lineage.clone();
        async move {
            let mut conn = pool.acquire().await.unwrap();
            delivery_store::observed(
                &mut conn,
                lineage.task_id,
                (lineage.workspace_id, lineage.project_id),
                Some(&latest),
                &node,
                (false, 0, 1),
            )
            .await
            .unwrap()["facts"]
                .clone()
        }
    };
    let facts = observed(graph.clone()).await;
    assert_eq!(facts["kind"], "review-only");
    assert_eq!(facts["outcome"], "review_changes_requested");
    let mut moved = graph.clone();
    moved["version"] = json!(version + 1);
    assert_eq!(observed(moved).await["outcome"], "review_incomplete");
    let mut wrong = graph.clone();
    wrong["body"] = json!(format!("Review target: {id}@{}", "0".repeat(64)));
    assert_eq!(observed(wrong).await["outcome"], "review_incomplete");
    let mut unmarked = graph;
    unmarked["labels"] = json!([]);
    assert_eq!(observed(unmarked).await["outcome"], "success");
}

#[tokio::test]
async fn two_projects_keep_distinct_result_paths_after_enrollment_changes() {
    use aq_playbook_domain::delivery_store::{self, DeliveryPort, PgDelivery};
    let mut legacy = super::super::auto_env("not_required", axum::http::StatusCode::OK).await;
    let lineage = aq_selection::TaskLineage {
        project_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        task_id: Uuid::new_v4(),
    };
    let mut enrolled = enroll_project(&legacy, &lineage).await;
    switch_enrollment(&mut legacy, &mut enrolled);

    let pool = legacy.f.db.pool().await;
    let port = PgDelivery(pool.clone());
    let pending = port.pending(None, 8).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].project_id, lineage.project_id);
    assert_eq!(pending[0].result_id, enrolled.report.report_id);
    assert!(
        port.latest(legacy.f.selection.lineage.task_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        delivery_store::undecided(
            &mut *pool.acquire().await.unwrap(),
            legacy.report.binding.attempt_id
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        complete_by_playbook(&legacy.f.state, legacy.report.report_id)
            .await
            .is_err()
    );
    assert!(
        settlement::settle_after_record(&enrolled.f.state, enrolled.report.report_id)
            .await
            .unwrap()
            .is_none()
    );

    replay_legacy(&legacy).await;
    // The enrolled attempt replays its original pushed facts after the
    // config switch and waits for its own completion decision.
    results::record_pushed(
        &enrolled.f.state,
        &context(&enrolled.f).await,
        &enrolled.report,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        settlement_rows(&enrolled.f, enrolled.report.report_id).await,
        0
    );
    assert_eq!(
        result_facts(&enrolled.f, enrolled.report.report_id)
            .await
            .unwrap()["source"]["checkpoint"],
        enrolled.checkpoint
    );
    assert_eq!(complete(&enrolled).await, Settled::Completed);
    assert_eq!(enrolled.writes.load(Ordering::SeqCst), 1);
    assert!(port.pending(None, 8).await.unwrap().is_empty());
}

async fn reviewer(target: &AutoEnv, lineage: &aq_selection::TaskLineage, pin: &str) -> AutoEnv {
    let id = target.report.report_id;
    let mut f = Box::pin(common::profiled::fixture_on(
        target.f.db.clone(),
        Some((target.f.actor, lineage.clone())),
    ))
    .await;
    f.state.config = target.f.state.config.clone();
    let (f, _, mut report) = Box::pin(prepare_on(
        f,
        true,
        true,
        None,
        false,
        Some(default_policy("not_required")),
        true,
    ))
    .await;
    let signed = format!(
        "Outcome: done; Changes: none; Copied: none; Claude exit 0; Result: Review verdict: changes_requested {id}@{pin} {} Unit checks exposed a defect.",
        target.checkpoint
    );
    report.conclusion = Label::new(signed).unwrap();
    Box::pin(finish_auto_env(
        f,
        report,
        axum::http::StatusCode::OK,
        ReportSource::Pushed { changed: false },
    ))
    .await
}

async fn enroll_project(legacy: &AutoEnv, lineage: &aq_selection::TaskLineage) -> AutoEnv {
    let mut f =
        common::profiled::fixture_on(legacy.f.db.clone(), Some((legacy.f.actor, lineage.clone())))
            .await;
    f.state.config = legacy.f.state.config.clone();
    let (f, _, report) = Box::pin(prepare_on(
        f,
        true,
        true,
        None,
        false,
        Some(default_policy("not_required")),
        true,
    ))
    .await;
    finish_auto_env(
        f,
        report,
        axum::http::StatusCode::OK,
        ReportSource::Pushed { changed: true },
    )
    .await
}

fn switch_enrollment(legacy: &mut AutoEnv, enrolled: &mut AutoEnv) {
    // An in-flight legacy attempt's project is now enabled. The project's
    // earlier enrollment for the other attempt was removed.
    let mut config = (*legacy.f.state.config).clone();
    config.delivery = Some(aq_delivery::github::DeliveryConfig {
        project_allowlist: [legacy.f.selection.lineage.project_id].into(),
        git_http_socket: Some(
            enrolled
                .f
                .state
                .source_retention
                .as_ref()
                .unwrap()
                .config
                .git_socket
                .clone(),
        ),
        ..Default::default()
    });
    let config = Arc::new(config);
    legacy.f.state.config = config.clone();
    enrolled.f.state.config = config;
}

async fn replay_legacy(legacy: &AutoEnv) {
    legacy
        .check(
            CheckedSource {
                id: legacy.source,
                checkpoint: &legacy.checkpoint,
            },
            &legacy.validator,
            "passed",
        )
        .await;
    // Replaying the report drives the frozen legacy policy to completion.
    results::record_pushed(
        &legacy.f.state,
        &context(&legacy.f).await,
        &legacy.report,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        settlement_row(&legacy.f, legacy.report.report_id).await.0,
        "confirmed"
    );
    assert_eq!(legacy.writes.load(Ordering::SeqCst), 1);
    assert!(
        result_facts(&legacy.f, legacy.report.report_id)
            .await
            .is_none()
    );
}
