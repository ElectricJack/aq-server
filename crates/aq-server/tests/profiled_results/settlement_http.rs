//! Actual protected daemon routes + SQL owners, synthetic retained source/Graph
//! peers. Test choices are fixture data, never real human/physical qualification.
use super::*;
use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
    response::Response,
};
use tower::ServiceExt;

fn app(c: &Case) -> Router {
    let mut state = c.env.f.state.clone();
    let mut config = (*state.config).clone();
    config.reviews = Some(c.env.keys.clone());
    assert!(config.playbooks.is_none());
    state.config = Arc::new(config);
    // Exercise the ordinary production builder, without a policy runtime.
    aq_server::build_admin_router(state)
}
fn request(method: &str, path: &str, body: Value, email: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aq-dev-email", email);
    if method != "GET" {
        request = request.header("X-AQ-Request", "1");
    }
    request
        .body(if method == "GET" {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap()
}
async fn value(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 264192)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
async fn pending(c: &Case) -> aq_review_protocol::DecisionRequest {
    let actor = c
        .env
        .f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let router = app(c);
    let prepared = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/reviews/combined/prepare",
            json!({"report_id":c.env.report.report_id,"source_operation_id":c.env.operation,
            "external":external(c.env.report.binding.attempt_id)}),
            "owner@localhost",
        ))
        .await
        .unwrap();
    assert_eq!(prepared.status(), StatusCode::OK);
    let subject: aq_server::reviews::combined::CombinedSubject =
        serde_json::from_value(value(prepared).await).unwrap();
    let decision = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: subject.artifact().unwrap(),
        eligible_owners: vec![actor.id],
        eligibility_project_revision: 1,
        review_policy: subject.code.validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    let body = json!({"report_id":c.env.report.report_id,"source_operation_id":c.env.operation,
        "external":external(c.env.report.binding.attempt_id),"decision":decision});
    let mut wrong = body.clone();
    wrong["decision"]["subject"]["digest"] = json!(Digest::of(b"forged admission"));
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/api/reviews/combined",
                wrong,
                "owner@localhost",
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    for _ in 0..2 {
        let admitted = router
            .clone()
            .oneshot(request(
                "POST",
                "/api/reviews/combined",
                body.clone(),
                "owner@localhost",
            ))
            .await
            .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
        assert_eq!(value(admitted).await, json!(decision));
    }
    decision
}
fn finish(c: &Case) {
    c.env.peer.abort();
    c._graph.abort();
}

#[tokio::test]
async fn combined_http_exact_profile_subject_and_explicit_answer_replay() {
    let c = Box::pin(setup()).await;
    let decision = pending(&c).await;
    let router = app(&c);
    let path = format!("/api/reviews/combined/{}", decision.decision_id);
    let response = router
        .clone()
        .oneshot(request("GET", &path, Value::Null, "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let inspected = value(response).await;
    assert_eq!(inspected["request"]["subject"], json!(decision.subject));
    assert_eq!(
        inspected["subject"]["profile"],
        "source_and_effects_review_v1"
    );
    let old = router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/reviews/decisions/{}", decision.decision_id),
            Value::Null,
            "owner@localhost",
        ))
        .await
        .unwrap();
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
    let hidden = router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/reviews/combined/{}", old_request.decision_id),
            Value::Null,
            "owner@localhost",
        ))
        .await
        .unwrap();
    assert_eq!(hidden.status(), StatusCode::FORBIDDEN);
    let old = router
        .clone()
        .oneshot(request(
            "GET",
            &format!("/api/reviews/decisions/{}", old_request.decision_id),
            Value::Null,
            "owner@localhost",
        ))
        .await
        .unwrap();
    assert_eq!(old.status(), StatusCode::OK);
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
    let legacy = router
        .clone()
        .oneshot(request(
            "POST",
            &format!("/api/reviews/decisions/{}/resolve", decision.decision_id),
            json!(answer),
            "owner@localhost",
        ))
        .await
        .unwrap();
    assert_eq!(
        legacy.status(),
        StatusCode::FORBIDDEN,
        "old code-only resolution cannot answer a combined subject"
    );
    let resolve = format!("{path}/resolve");
    let body = json!({"expected_subject":decision.subject,"answer":answer});
    let mut wrong = body.clone();
    wrong["expected_subject"]["digest"] = json!(Digest::of(b"changed exact subject"));
    let refused = router
        .clone()
        .oneshot(request("POST", &resolve, wrong, "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let mut wrong = body.clone();
    wrong["answer"]["decision_id"] = json!(Uuid::new_v4());
    assert_eq!(
        router
            .clone()
            .oneshot(request("POST", &resolve, wrong, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let resolved = router
        .clone()
        .oneshot(request("POST", &resolve, body.clone(), "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(resolved.status(), StatusCode::OK);
    let receipt = value(resolved).await;
    assert_eq!(receipt["subject"], json!(decision.subject));
    assert_eq!(receipt["finality"]["choice"], "reject");
    let replay = router
        .clone()
        .oneshot(request("POST", &resolve, body.clone(), "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(value(replay).await, receipt);
    let mut changed = body;
    changed["answer"]["choice"] = json!("approve");
    assert_eq!(
        router
            .oneshot(request("POST", &resolve, changed, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    finish(&c);
}

#[tokio::test]
async fn settlement_http_lookup_is_readonly_and_reconcile_reuses_exact_effect() {
    let c = Box::pin(setup()).await;
    let router = app(&c);
    let email = &c.actor.email;
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/settlement/evaluations",
            json!(c.request),
            email,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let evaluation = value(response).await;
    assert_eq!(evaluation["disposition"], "accepted");
    let completion = evaluation["completion_operation_id"].as_str().unwrap();
    let path = format!("/api/settlement/completions/{completion}/reconcile");
    let lookup = format!("/api/settlement/evaluations/{}", c.request.operation_id);
    let read = router
        .clone()
        .oneshot(request("GET", &lookup, Value::Null, email))
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(value(read).await, evaluation);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    let foreign = router
        .clone()
        .oneshot(request("GET", &lookup, Value::Null, "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(
        foreign.status(),
        StatusCode::CONFLICT,
        "a different Owner cannot substitute for the retained evaluation requester"
    );
    let foreign = router
        .clone()
        .oneshot(request("POST", &path, json!({}), "owner@localhost"))
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::CONFLICT);
    let replay = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/settlement/evaluations",
            json!(c.request),
            email,
        ))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(value(replay).await, evaluation);
    let mut changed = json!(c.request);
    changed["subject"]["digest"] = json!(Digest::of(b"changed evaluation"));
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/api/settlement/evaluations",
                changed,
                email
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    for body in [json!([]), Value::Null, json!({"retry":true})] {
        assert_eq!(
            router
                .clone()
                .oneshot(request("POST", &path, body, email))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        router
            .clone()
            .oneshot(request("GET", &path, Value::Null, email))
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
    c.loss.store(true, Ordering::SeqCst);
    let response = router
        .clone()
        .oneshot(request("POST", &path, json!({}), email))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let possible = value(response).await;
    assert_eq!(possible["state"], "possible");
    assert!(possible["receipt"].is_null());
    assert_eq!(possible["recovery"], "replay_identical");
    let response = router
        .clone()
        .oneshot(request("POST", &path, json!({}), email))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let done = value(response).await;
    assert_eq!(done["state"], "confirmed");
    assert_eq!(done["operation_id"], json!(completion));
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    let read = router
        .oneshot(request("GET", &lookup, Value::Null, email))
        .await
        .unwrap();
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
async fn combined_http_optional_mount_owner_origin_and_closed_body_guards() {
    let c = Box::pin(setup()).await;
    let router = app(&c);
    let decision = c.request.decision_id;
    let paths = [
        ("POST", "/api/reviews/combined".into()),
        ("POST", "/api/reviews/combined/prepare".into()),
        ("GET", format!("/api/reviews/combined/{decision}")),
        ("POST", format!("/api/reviews/combined/{decision}/resolve")),
        ("POST", "/api/settlement/evaluations".into()),
        (
            "GET",
            format!("/api/settlement/evaluations/{}", c.request.operation_id),
        ),
        (
            "POST",
            format!("/api/settlement/completions/{}/reconcile", Uuid::new_v4()),
        ),
    ];
    let absent = aq_server::build_admin_router(c.env.f.state.clone());
    let machine = aq_server::build_worker_router(c.env.f.state.clone());
    for (method, path) in &paths {
        assert_eq!(
            machine
                .clone()
                .oneshot(request(method, path, json!({}), "owner@localhost"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            absent
                .clone()
                .oneshot(request(method, path, json!({}), "owner@localhost"))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let at = chrono::DateTime::from_timestamp_millis(NOW as i64).unwrap();
    c.env
        .f
        .state
        .store
        .invite_user("http-viewer@localhost", aq_server::store::Role::Viewer, at)
        .await
        .unwrap();
    c.env
        .f
        .state
        .store
        .bind_invited_user("http-viewer@localhost", "dev", "http-viewer@localhost", at)
        .await
        .unwrap()
        .unwrap();
    for (method, path) in &paths {
        assert_eq!(
            router
                .clone()
                .oneshot(request(method, path, json!({}), "http-viewer@localhost"))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        router
            .clone()
            .oneshot(request("GET", &paths[2].1, Value::Null, ""))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let resolve = &paths[3].1;
    let mut missing = request("POST", resolve, json!({}), "owner@localhost");
    missing.headers_mut().remove("x-aq-request");
    assert_eq!(
        router.clone().oneshot(missing).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut origin = request("POST", resolve, json!({}), "owner@localhost");
    origin
        .headers_mut()
        .insert("origin", "https://foreign.invalid".parse().unwrap());
    assert_eq!(
        router.clone().oneshot(origin).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    for body in [
        json!({"expected_subject":c.request.subject,"answer":{},"unexpected":true}),
        json!({"schema_version":1,"operation_id":Uuid::new_v4(),"decision_id":decision,"choice":"approve"}),
    ] {
        assert_eq!(
            router
                .clone()
                .oneshot(request("POST", resolve, body, "owner@localhost"))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Reject,
        comment: None,
    };
    let positional = json!([c.request.subject, answer]);
    assert_eq!(
        router
            .clone()
            .oneshot(request("POST", resolve, positional, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let positional = json!([
        c.request.schema_version,
        c.request.operation_id,
        c.request.report_id,
        c.request.source_operation_id,
        c.request.decision_id,
        c.request.subject
    ]);
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/api/settlement/evaluations",
                positional,
                &c.actor.email
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let reference = json!(c.request.subject);
    let reference_array = json!([reference["id"], reference["version"], reference["digest"]]);
    let nested = json!({"expected_subject":reference_array,"answer":answer});
    assert_eq!(
        router
            .clone()
            .oneshot(request("POST", resolve, nested, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let answer_array = json!([
        answer.schema_version,
        answer.operation_id,
        answer.decision_id,
        answer.expected_decision_revision,
        answer.choice,
        answer.comment
    ]);
    let nested = json!({"expected_subject":c.request.subject,"answer":answer_array});
    assert_eq!(
        router
            .clone()
            .oneshot(request("POST", resolve, nested, "owner@localhost"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let mut nested = json!(c.request);
    nested["subject"] = reference_array;
    assert_eq!(
        router
            .clone()
            .oneshot(request(
                "POST",
                "/api/settlement/evaluations",
                nested,
                &c.actor.email
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_settlement_evaluations")
        .fetch_one(&c.env.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let duplicate = format!(
        "{{\"expected_subject\":{},\"expected_subject\":{},\"answer\":{}}}",
        json!(c.request.subject),
        json!(c.request.subject),
        json!(answer)
    );
    let mut duplicated = request("POST", resolve, json!({}), "owner@localhost");
    *duplicated.body_mut() = Body::from(duplicate);
    assert_eq!(
        router.clone().oneshot(duplicated).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut large = request("POST", resolve, json!({}), "owner@localhost");
    *large.body_mut() = Body::from(vec![b' '; 16385]);
    assert_eq!(
        router.oneshot(large).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    finish(&c);
}

struct RebindOnRead {
    future: std::pin::Pin<Box<dyn std::future::Future<Output = Bytes> + Send>>,
    done: bool,
}
impl http_body::Body for RebindOnRead {
    type Data = Bytes;
    type Error = std::convert::Infallible;
    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>>
    {
        if self.done {
            return std::task::Poll::Ready(None);
        }
        match self.future.as_mut().poll(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(bytes) => {
                self.done = true;
                std::task::Poll::Ready(Some(Ok(http_body::Frame::data(bytes))))
            }
        }
    }
}
#[tokio::test]
async fn settlement_http_rechecks_full_current_principal_after_body_read() {
    let c = Box::pin(setup()).await;
    let router = app(&c);
    let pool = c.env.f.db.pool().await;
    let actor = c.actor.id;
    let bytes = serde_json::to_vec(&c.request).unwrap();
    let future = Box::pin(async move {
        sqlx::query("UPDATE users SET subject='changed-after-auth-body-read' WHERE id=$1")
            .bind(actor)
            .execute(&pool)
            .await
            .unwrap();
        Bytes::from(bytes)
    });
    let mut request = request(
        "POST",
        "/api/settlement/evaluations",
        Value::Null,
        &c.actor.email,
    );
    *request.body_mut() = Body::new(RebindOnRead {
        future,
        done: false,
    });
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_settlement_evaluations")
        .fetch_one(&c.env.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
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
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(&pool)
        .await
        .unwrap();
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/settlement/evaluations",
            json!(evaluation),
            &c.actor.email,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    for table in [
        "profiled_settlement_evaluations",
        "profiled_task_completions",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_verified_releases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, before);
    assert_eq!(c.writes.load(Ordering::SeqCst), 0);
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
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            &path,
            json!({"expected_subject":decision.subject,"answer":answer}),
            "owner@localhost",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/api/settlement/evaluations",
            json!(evaluation),
            &c.actor.email,
        ))
        .await
        .unwrap();
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
        let response = router
            .clone()
            .oneshot(request("POST", &path, json!({}), &c.actor.email))
            .await
            .unwrap();
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
