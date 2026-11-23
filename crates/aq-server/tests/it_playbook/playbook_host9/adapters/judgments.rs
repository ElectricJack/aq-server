use super::*;

#[tokio::test]
async fn an_assessment_answers_by_name_and_reaches_the_playbook_as_labels_and_flags() {
    let answer = json!({"labels": {"kind": "bugfix", "risk": "high"},
        "flags": {"hard": true, "narrow": false}, "rationale": "Billing code, money at stake."});
    let judge = FakeAssessor::answering(Ok(answer));
    let h = host(true, Some(judge.clone())).await;
    let task = h.graph.task("Fix billing rounding", json!({}));
    let handle = h.assess("assess-1", assess_input(task)).await;
    let (outcome, output) = h.completed(&handle).await;
    assert_eq!(outcome, "assessed", "{output}");
    completion_valid(&outcome, &output);
    // One label per field, in field order; the flags that hold.
    assert_eq!(output["labels"], json!(["high", "bugfix"]));
    assert_eq!(output["flags"], json!(["hard"]));
    assert_eq!(output["rationale"], "Billing code, money at stake.");
    assert_eq!(output["reference"], format!("judgment:{handle}"));
    assert_eq!(
        output["subject"],
        json!({"id": task.to_string(), "revision": 3})
    );
    let p = &output["provenance"];
    assert_eq!(
        (&p["provider"], &p["model"]),
        (&json!("fixture"), &json!("model-a"))
    );
    assert_eq!(
        p["usage"],
        json!({"input_tokens": 40, "output_tokens": 9, "cost_microunits": 0})
    );
    // The model was asked by name, with the choices as the schema.
    let request = judge.requests.lock().unwrap()[0].clone();
    assert_eq!(request.purpose, aq_internal_work::Purpose::PlaybookCall);
    assert_eq!(
        request.schema["properties"]["labels"]["properties"]["risk"]["enum"],
        json!(["low", "medium", "high", "unknown"])
    );
    assert!(
        request
            .instructions
            .contains("Classify this task for routing.")
    );
    assert!(request.input.contains("Fix billing rounding"));
    assert_eq!(
        (request.deadline_ms, request.max_output_bytes),
        (120_000, 8192)
    );
    // Exactly one final answer: one event, and neither a second dispatch
    // of the intent nor the deadline sweep adds another.
    assert_eq!(h.events().await, 1);
    let again = h
        .run("assess-1", "judgment.assess", assess_input(task))
        .await;
    assert_eq!(again.output["handle"], handle.as_str());
    h.f.time
        .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
        0
    );
    assert_eq!(
        (h.events().await, judge.requests.lock().unwrap().len()),
        (1, 1)
    );
}

#[tokio::test]
async fn an_answer_outside_the_choices_fails_as_invalid_output() {
    for (answer, reason) in [
        (
            Ok(json!({"labels": {"kind": "bugfix", "risk": "severe"},
            "flags": {"hard": true, "narrow": false}, "rationale": "r"})),
            "indeterminate",
        ),
        (
            Ok(
                json!({"labels": {"risk": "low"}, "flags": {"hard": true, "narrow": false}, "rationale": "r"}),
            ),
            "indeterminate",
        ),
        (Err("provider_error"), "provider_error"),
        (Err("provider_unavailable"), "provider_unavailable"),
    ] {
        let h = host(true, Some(FakeAssessor::answering(answer))).await;
        let task = h.graph.task("Unclear", json!({}));
        let handle = h.assess("assess", assess_input(task)).await;
        let (outcome, output) = h.completed(&handle).await;
        assert_eq!(
            (outcome.as_str(), &output["reason"]),
            ("failed", &json!(reason)),
            "{output}"
        );
        completion_valid(&outcome, &output);
        assert_eq!(h.events().await, 1);
    }
    // No provider at all: accepted, then failed, never left unanswered.
    let h = host(true, None).await;
    let task = h.graph.task("Nobody to ask", json!({}));
    let handle = h.assess("assess", assess_input(task)).await;
    let (outcome, output) = h.completed(&handle).await;
    assert_eq!(
        (outcome.as_str(), &output["reason"]),
        ("failed", &json!("provider_unavailable"))
    );
}

#[tokio::test]
async fn a_request_over_sixteen_fields_is_refused() {
    let judge = FakeAssessor::answering(Err("provider_error"));
    let h = host(true, Some(judge.clone())).await;
    let task = h.graph.task("Too many questions", json!({}));
    // Seventeen fields, or more than 65536 bytes, are outside the
    // contract: the bridge refuses the intent itself, as the engine does
    // before it dispatches one.
    let mut many = assess_input(task);
    many["fields"] = (0..17)
        .map(|i| json!({"id": format!("f{i}"), "description": "d", "choices": ["a"]}))
        .collect();
    let mut large = assess_input(task);
    large["context"]["items"] = (0..9)
        .map(|n| json!({"label": format!("part {n}"), "text": "x".repeat(8000)}))
        .collect();
    for (id, input) in [("many", many), ("large", large)] {
        let intent = h.intent(id, "judgment.assess", input);
        let refused = (h.service).host_dispatch(&h.runtime, h.grant, 1, &intent);
        assert_eq!(refused.await, Err(Error::Invalid), "{id}");
    }
    // Requests the contract admits and the host does not run.
    type Change = fn(&mut Value);
    let changes: [(&str, Change); 4] = [
        ("duplicate", |i| i["fields"][1]["id"] = json!("risk")),
        ("no-choices", |i| i["fields"][0]["choices"] = json!([])),
        ("no-fields", |i| i["fields"] = json!([])),
        ("duplicate-flag", |i| i["flags"][1]["id"] = json!("narrow")),
    ];
    for (id, change) in changes {
        let mut input = assess_input(task);
        change(&mut input);
        let refused = h.run(id, "judgment.assess", input).await;
        assert_eq!(refused.outcome, "refused", "{id}");
        assert_eq!(refused.output["reason"], "invalid_request", "{id}");
    }
    // Nothing was asked, queued or left running.
    assert!(judge.requests.lock().unwrap().is_empty());
    assert_eq!(h.events().await, 0);
    let running: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_host_operations")
        .fetch_one(&h.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(running, 0);
}

#[tokio::test]
async fn an_accepted_judgment_without_an_answer_fails_at_its_deadline_exactly_once() {
    let answer = json!({"labels": {"kind": "bugfix", "risk": "low"},
        "flags": {"hard": false, "narrow": true}, "rationale": "late"});
    let judge = FakeAssessor::answering(Ok(answer));
    judge.held.store(true, std::sync::atomic::Ordering::SeqCst);
    let h = host(true, Some(judge.clone())).await;
    let task = h.graph.task("The provider hangs", json!({}));
    let handle = h.assess("assess", assess_input(task)).await;
    // Before the deadline (and its grace) the judgment stays open.
    h.f.time
        .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
        0
    );
    assert_eq!(h.operation(&handle).await.0, "running");
    h.f.time
        .fetch_add(5_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
        1
    );
    let (outcome, output) = h.completed(&handle).await;
    assert_eq!(
        (outcome.as_str(), &output["reason"]),
        ("failed", &json!("deadline_exceeded"))
    );
    completion_valid(&outcome, &output);
    assert_eq!(
        output["subject"],
        json!({"id": task.to_string(), "revision": 3})
    );
    // The provider's late answer changes nothing: still one final answer.
    judge.release.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(h.operation(&handle).await.0, "failed");
    assert_eq!(h.events().await, 1);
    assert_eq!(
        h.service.expire_assessments(&h.runtime, 2).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_routing_playbook_judgment_runs_with_the_routing_classification_purpose() {
    use crate::common::fake_api;
    use aq_server::playbooks::internal_judgments::InternalJudgments;
    // The production provider (internal work) against a local endpoint.
    let answer = r#"{"labels": {"risk": "medium", "kind": "feature"}, "flags": {"narrow": true, "hard": false}, "rationale": "A contained feature."}"#;
    let (endpoint, seen) = fake_api::serve(200, fake_api::completed(answer)).await;
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let (work, _dir) =
        crate::playbook_host_requests::internal_work(&f, endpoint, Some("engineering")).await;
    let (_stop, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(work.clone().serve(rx));
    let judgments = InternalJudgments::new(work, aq_internal_work::Purpose::PlaybookCall);
    let h = host_on(f, true, Some(Arc::new(judgments))).await;
    let pool = h.f.db.pool().await;
    // The grant the server holds for the routing source (R2).
    sqlx::query("INSERT INTO playbook_sources(id,kind,binding,scope,purpose,judgment_purpose,commands,events,capabilities,facts,revision,updated_at_ms) VALUES('routing','builtin','aq.routing','installation','Route every task','routing_classification','[]','[]','[]','[]',1,1)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO playbook_source_grants(source_id,grant_id,revision,capabilities,created_at_ms) VALUES('routing',$1,1,'[]',1)")
        .bind(h.grant)
        .execute(&pool)
        .await
        .unwrap();
    let task = h.graph.task("Add an export button", json!({}));
    let handle = h.assess("assess", assess_input(task)).await;
    let (outcome, output) = h.completed(&handle).await;
    assert_eq!(outcome, "assessed", "{output}");
    completion_valid(&outcome, &output);
    assert_eq!(output["labels"], json!(["medium", "feature"]));
    assert_eq!(output["flags"], json!(["narrow"]));
    let p = &output["provenance"];
    assert_eq!(
        (&p["provider"], &p["model"]),
        (&json!("fixture-a"), &json!("model-a"))
    );
    assert_eq!(p["credential_profile"], "internal-work");
    assert_eq!(seen.lock().unwrap().bodies.len(), 1, "one provider request");
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT purpose,affected_kind,affected_id FROM internal_tasks")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        rows,
        [(
            "routing_classification".to_string(),
            "playbook_operation".to_string(),
            handle
        )]
    );
}
