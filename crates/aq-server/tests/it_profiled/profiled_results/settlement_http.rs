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
async fn call(router: &Router, request: Request<Body>) -> Response {
    router.clone().oneshot(request).await.unwrap()
}
async fn owner_get(router: &Router, path: &str) -> Response {
    call(router, request("GET", path, Value::Null, "owner@localhost")).await
}
async fn owner_post(router: &Router, path: &str, body: Value) -> Response {
    call(router, request("POST", path, body, "owner@localhost")).await
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
    let prepared = owner_post(
        &router,
        "/api/reviews/combined/prepare",
        json!({"report_id":c.env.report.report_id,"source_operation_id":c.env.operation,
            "external":external(c.env.report.binding.attempt_id)}),
    )
    .await;
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
        owner_post(&router, "/api/reviews/combined", wrong)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    for _ in 0..2 {
        let admitted = owner_post(&router, "/api/reviews/combined", body.clone()).await;
        assert_eq!(admitted.status(), StatusCode::OK);
        assert_eq!(value(admitted).await, json!(decision));
    }
    decision
}
fn finish(c: &Case) {
    c.env.peer.abort();
    c._graph.abort();
}

#[path = "settlement_http/evaluations.rs"]
mod evaluations;
#[path = "settlement_http/guards.rs"]
mod guards;
#[path = "settlement_http/subjects.rs"]
mod subjects;
