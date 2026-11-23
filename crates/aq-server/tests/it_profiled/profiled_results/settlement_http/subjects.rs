use super::*;

#[tokio::test]
async fn combined_http_exact_profile_subject_and_explicit_answer_replay() {
    let c = Box::pin(setup()).await;
    let decision = pending(&c).await;
    let router = app(&c);
    let path = format!("/api/reviews/combined/{}", decision.decision_id);
    check_subject_routes(&c, &router, &decision, &path).await;
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision.decision_id,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Reject,
        comment: Some(
            "Explicit synthetic HTTP component-test rejection, not physical review".into(),
        ),
    };
    let legacy = owner_post(
        &router,
        &format!("/api/reviews/decisions/{}/resolve", decision.decision_id),
        json!(answer),
    )
    .await;
    assert_eq!(
        legacy.status(),
        StatusCode::FORBIDDEN,
        "old code-only resolution cannot answer a combined subject"
    );
    let resolve = format!("{path}/resolve");
    let body = json!({"expected_subject":decision.subject,"answer":answer});
    let mut wrong = body.clone();
    wrong["expected_subject"]["digest"] = json!(Digest::of(b"changed exact subject"));
    let refused = owner_post(&router, &resolve, wrong).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let mut wrong = body.clone();
    wrong["answer"]["decision_id"] = json!(Uuid::new_v4());
    assert_eq!(
        owner_post(&router, &resolve, wrong).await.status(),
        StatusCode::BAD_REQUEST
    );
    let resolved = owner_post(&router, &resolve, body.clone()).await;
    assert_eq!(resolved.status(), StatusCode::OK);
    let receipt = value(resolved).await;
    assert_eq!(receipt["subject"], json!(decision.subject));
    assert_eq!(receipt["finality"]["choice"], "reject");
    let replay = owner_post(&router, &resolve, body.clone()).await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(value(replay).await, receipt);
    let mut changed = body;
    changed["answer"]["choice"] = json!("approve");
    assert_eq!(
        owner_post(&router, &resolve, changed).await.status(),
        StatusCode::CONFLICT
    );
    finish(&c);
}

async fn check_subject_routes(
    c: &Case,
    router: &Router,
    decision: &aq_review_protocol::DecisionRequest,
    path: &str,
) {
    let response = owner_get(router, path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let inspected = value(response).await;
    assert_eq!(inspected["request"]["subject"], json!(decision.subject));
    assert_eq!(
        inspected["subject"]["profile"],
        "source_and_effects_review_v1"
    );
    let old = owner_get(
        router,
        &format!("/api/reviews/decisions/{}", decision.decision_id),
    )
    .await;
    assert_eq!(old.status(), StatusCode::FORBIDDEN);
    let old_request = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: c.env.prepared.subject().artifact().unwrap(),
        eligible_owners: vec![c.env.f.actor],
        eligibility_project_revision: 1,
        review_policy: c.env.prepared.subject().validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    c.env
        .service
        .admit(c.env.f.actor, &c.env.prepared, &old_request)
        .await
        .unwrap();
    let hidden = owner_get(
        router,
        &format!("/api/reviews/combined/{}", old_request.decision_id),
    )
    .await;
    assert_eq!(hidden.status(), StatusCode::FORBIDDEN);
    let old = owner_get(
        router,
        &format!("/api/reviews/decisions/{}", old_request.decision_id),
    )
    .await;
    assert_eq!(old.status(), StatusCode::OK);
}
