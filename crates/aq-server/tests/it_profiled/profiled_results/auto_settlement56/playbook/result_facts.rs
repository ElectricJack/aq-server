use super::*;

/// Spec 0016 section 7: recording a result stores its facts and decides
/// nothing. With its source retained and nothing else to wait for, the
/// result is neither completed, published nor sent back, and the claim
/// cannot be released without a Graph completion receipt.
#[tokio::test]
async fn a_recorded_result_waits_with_its_facts_and_no_verdict() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    // The duplicate record, now that the source is retained, replaces the facts.
    let auth = context(&env.f).await;
    results::record(&env.f.state, &auth, &env.report)
        .await
        .unwrap();
    let facts = result_facts(&env.f, env.report.report_id)
        .await
        .expect("facts recorded");
    assert_eq!(facts["source"]["operation_id"], json!(env.source));
    assert_eq!(facts["source"]["checkpoint"], json!(env.checkpoint));
    assert_eq!(facts["source"]["changed"], json!(true));
    assert_eq!(facts["outcome"], json!(env.report.outcome));
    assert_eq!(
        facts["accepted"],
        json!({"current_report_head": true, "stopped": true, "cancelled": false, "revoked": false, "holds_task": true})
    );
    assert_eq!(facts["delivery"]["state"], json!("missing"));
    assert!(facts.get("policy_actor").is_none() && facts.get("verdict").is_none());
    // No verdict, publication or completion.
    assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
    assert!(evidence_rows(&env).await.is_empty(), "nothing published");
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
    // The release guard still refuses a claim release without a receipt.
    let released =
        sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id)
            .execute(&env.f.db.pool().await)
            .await;
    assert!(released.is_err(), "the claim stays until a Graph receipt");
    // The playbook decides it.
    assert_eq!(complete(&env).await, Settled::Completed);
    assert!(!env.holds_task().await);
}

/// The reasons of the task's work read (`GET .../tasks/{task}/work`),
/// checked against the human-graph-1 schema.
pub(super) async fn work_reasons(env: &AutoEnv) -> Vec<Value> {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let lineage = &env.f.selection.lineage;
    let app = aq_server::agent_access::human::routes(&env.f.state).with_state(env.f.state.clone());
    let uri = format!(
        "/api/human/projects/{}/tasks/{}/work",
        lineage.project_id, lineage.task_id
    );
    let request = axum::http::Request::get(uri)
        .header("x-aq-dev-email", "owner@localhost")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = app.oneshot(request).await.unwrap();
    let status = res.status();
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, 200, "{body}");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, &body),
        "schema: {body}"
    );
    assert_eq!(body["data"]["readiness"]["state"], "held", "{body}");
    body["data"]["readiness"]["reasons"]
        .as_array()
        .unwrap()
        .clone()
}

/// Spec 0016 section 7 and acceptance 6: with no delivery playbook active
/// the result waits, and the task read says "result ready, no delivery
/// policy"; a decided result is no longer listed as waiting.
#[tokio::test]
async fn the_task_read_says_a_result_waits_without_a_delivery_policy() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let reasons = work_reasons(&env).await;
    let waiting = (reasons.iter())
        .find(|r| r["code"] == "result_ready_no_delivery_policy")
        .unwrap_or_else(|| panic!("{reasons:?}"));
    assert_eq!(waiting["result_id"], json!(env.report.report_id));
    assert_eq!(waiting["attempt_id"], json!(env.report.binding.attempt_id));
    let message = waiting["message"].as_str().unwrap();
    assert!(message.starts_with("Result ready, no delivery policy"));
    // A playbook's decision ends the wait (here it sends the result back).
    assert_eq!(send_back(&env, "checks_failed").await, Settled::Returned);
    let reasons = work_reasons(&env).await;
    assert!(
        reasons
            .iter()
            .all(|r| r["code"] != "result_ready_no_delivery_policy"),
        "{reasons:?}"
    );
}

/// The conclusion-text heuristics are gone: a failed report whose agent
/// said it was blocked, or a success that claims changes that did not
/// land, waits like any other result instead of being reworked.
#[tokio::test]
async fn conclusion_text_never_reworks_a_result() {
    use aq_admission::transport::ProcessOutcome;
    for (outcome, changed, conclusion) in [
        (
            ProcessOutcome::Failed,
            false,
            "Outcome: blocked; Changes: none; Copied: none; Claude exit 0; Result: no access to the repo",
        ),
        (
            ProcessOutcome::Success,
            false,
            "Changes: src/lib.rs; Checks: cargo test",
        ),
    ] {
        let (f, _, mut report) = Box::pin(prepare_with_policy_checks(
            true,
            true,
            None,
            false,
            Some(default_policy("not_required")),
            false,
        ))
        .await;
        report.outcome = outcome;
        report.conclusion = Label::new(conclusion).unwrap();
        let env = finish_auto_env(
            f,
            report,
            axum::http::StatusCode::OK,
            ReportSource::Pushed { changed: changed },
        )
        .await;
        let auth = context(&env.f).await;
        results::record(&env.f.state, &auth, &env.report)
            .await
            .unwrap();
        assert_eq!(settlement_rows(&env.f, env.report.report_id).await, 0);
        let facts = result_facts(&env.f, env.report.report_id).await.unwrap();
        assert_eq!(facts["outcome"], json!(outcome));
        assert_eq!(facts["source"]["changed"], json!(false));
        assert_eq!(env.writes.load(Ordering::SeqCst), 0);
        assert!(env.holds_task().await);
    }
}
