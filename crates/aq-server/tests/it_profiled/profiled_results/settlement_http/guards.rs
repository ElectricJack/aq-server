use super::*;

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
    check_route_access(&c, &router, &paths).await;
    let resolve = &paths[3].1;
    check_origin_and_unknown_fields(&c, &router, resolve).await;
    let answer = aq_review_protocol::ResolveRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: decision,
        expected_decision_revision: 1,
        choice: aq_review_protocol::Choice::Reject,
        comment: None,
    };
    check_closed_objects(&c, &router, resolve, &answer).await;
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

async fn check_route_access(c: &Case, router: &Router, paths: &[(&str, String)]) {
    let absent = aq_server::build_admin_router(c.env.f.state.clone());
    let machine = aq_server::build_worker_router(c.env.f.state.clone());
    for (method, path) in paths {
        assert_eq!(
            call(
                &machine,
                request(method, path, json!({}), "owner@localhost")
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&absent, request(method, path, json!({}), "owner@localhost"))
                .await
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
    for (method, path) in paths {
        assert_eq!(
            call(
                router,
                request(method, path, json!({}), "http-viewer@localhost")
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        call(router, request("GET", &paths[2].1, Value::Null, ""))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

async fn check_origin_and_unknown_fields(c: &Case, router: &Router, resolve: &str) {
    let decision = c.request.decision_id;
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
            owner_post(router, resolve, body).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
}

async fn check_closed_objects(
    c: &Case,
    router: &Router,
    resolve: &str,
    answer: &aq_review_protocol::ResolveRequest,
) {
    let positional = json!([c.request.subject, answer]);
    assert_eq!(
        owner_post(router, resolve, positional).await.status(),
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
        evaluate(c, router, positional).await.status(),
        StatusCode::BAD_REQUEST
    );
    let reference = json!(c.request.subject);
    let reference_array = json!([reference["id"], reference["version"], reference["digest"]]);
    let nested = json!({"expected_subject":reference_array,"answer":answer});
    assert_eq!(
        owner_post(router, resolve, nested).await.status(),
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
        owner_post(router, resolve, nested).await.status(),
        StatusCode::BAD_REQUEST
    );
    let mut nested = json!(c.request);
    nested["subject"] = reference_array;
    assert_eq!(
        evaluate(c, router, nested).await.status(),
        StatusCode::BAD_REQUEST
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_settlement_evaluations")
        .fetch_one(&c.env.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

async fn evaluate(c: &Case, router: &Router, body: Value) -> Response {
    call(
        router,
        request("POST", "/api/settlement/evaluations", body, &c.actor.email),
    )
    .await
}
