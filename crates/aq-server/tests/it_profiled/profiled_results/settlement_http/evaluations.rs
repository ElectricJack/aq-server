use super::*;

#[tokio::test]
async fn settlement_http_lookup_is_readonly_and_reconcile_reuses_exact_effect() {
    let c = Box::pin(setup()).await;
    let router = app(&c);
    let email = &c.actor.email;
    let response = call(
        &router,
        request(
            "POST",
            "/api/settlement/evaluations",
            json!(c.request),
            email,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let evaluation = value(response).await;
    assert_eq!(evaluation["disposition"], "accepted");
    let completion = evaluation["completion_operation_id"].as_str().unwrap();
    let path = format!("/api/settlement/completions/{completion}/reconcile");
    let lookup = format!("/api/settlement/evaluations/{}", c.request.operation_id);
    let read = call(&router, request("GET", &lookup, Value::Null, email)).await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(value(read).await, evaluation);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    let foreign = owner_get(&router, &lookup).await;
    assert_eq!(
        foreign.status(),
        StatusCode::CONFLICT,
        "a different Owner cannot substitute for the retained evaluation requester"
    );
    let foreign = owner_post(&router, &path, json!({})).await;
    assert_eq!(foreign.status(), StatusCode::CONFLICT);
    check_reconcile_inputs(&c, &router, &path, &evaluation).await;
    c.loss.store(true, Ordering::SeqCst);
    let response = call(&router, request("POST", &path, json!({}), email)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let possible = value(response).await;
    assert_eq!(possible["state"], "possible");
    assert!(possible["receipt"].is_null());
    assert_eq!(possible["recovery"], "replay_identical");
    let response = call(&router, request("POST", &path, json!({}), email)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let done = value(response).await;
    assert_eq!(done["state"], "confirmed");
    assert_eq!(done["operation_id"], json!(completion));
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    let read = call(&router, request("GET", &lookup, Value::Null, email)).await;
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(value(read).await, evaluation);
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(c.env.report.binding.attempt_id)
            .fetch_one(&c.env.f.db.pool().await)
            .await
            .unwrap();
    assert!(!held);
    finish(&c);
}

#[tokio::test]
async fn settlement_http_pending_review_refuses_without_effect_then_same_intent_can_retry() {
    let c = Box::pin(setup()).await;
    let decision = pending(&c).await;
    let router = app(&c);
    let mut evaluation = c.request.clone();
    evaluation.operation_id = Uuid::new_v4();
    evaluation.decision_id = decision.decision_id;
    evaluation.subject = decision.subject.clone();
    let pool = c.env.f.db.pool().await;
    check_pending_review(&c, &router, &pool, &evaluation).await;
    // Explicit synthetic fixture choice, not a real human or physical proof.
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision.decision_id,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Approve,
        comment: Some("Synthetic pending-to-resolved HTTP fixture only".into()),
    };
    let path = format!("/api/reviews/combined/{}/resolve", decision.decision_id);
    let response = owner_post(
        &router,
        &path,
        json!({"expected_subject":decision.subject,"answer":answer}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = call(
        &router,
        request(
            "POST",
            "/api/settlement/evaluations",
            json!(evaluation),
            &c.actor.email,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let receipt = value(response).await;
    assert_eq!(receipt["operation_id"], json!(evaluation.operation_id));
    assert_eq!(receipt["disposition"], "accepted");
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(c.env.report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        held,
        "evaluation alone neither dispatches Graph nor releases the task"
    );
    let completion = receipt["completion_operation_id"].as_str().unwrap();
    let path = format!("/api/settlement/completions/{completion}/reconcile");
    for _ in 0..2 {
        let response = call(&router, request("POST", &path, json!({}), &c.actor.email)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(value(response).await["state"], "confirmed");
    }
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(c.env.report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!held, "confirmed completion releases the task exactly once");
    finish(&c);
}

async fn check_reconcile_inputs(c: &Case, router: &Router, path: &str, evaluation: &Value) {
    let email = &c.actor.email;
    let replay = call(
        router,
        request(
            "POST",
            "/api/settlement/evaluations",
            json!(c.request),
            email,
        ),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(value(replay).await, *evaluation);
    let mut changed = json!(c.request);
    changed["subject"]["digest"] = json!(Digest::of(b"changed evaluation"));
    assert_eq!(
        call(
            router,
            request("POST", "/api/settlement/evaluations", changed, email)
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    for body in [json!([]), Value::Null, json!({"retry":true})] {
        assert_eq!(
            call(router, request("POST", path, body, email))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(router, request("GET", path, Value::Null, email))
            .await
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
}

async fn check_pending_review(
    c: &Case,
    router: &Router,
    pool: &sqlx::PgPool,
    evaluation: &settlement::EvaluateRequest,
) {
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    let response = call(
        router,
        request(
            "POST",
            "/api/settlement/evaluations",
            json!(evaluation),
            &c.actor.email,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    for table in [
        "profiled_settlement_evaluations",
        "profiled_task_completions",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
}
