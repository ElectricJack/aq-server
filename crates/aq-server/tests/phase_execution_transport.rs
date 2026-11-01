//! Production Prepare transport over actual owners; Graph/Git/private inputs are
//! synthetic in these fixtures. The cross-process physical probe is separate.
mod common;
#[allow(dead_code)]
#[path = "common/phase_terminal.rs"]
mod terminal;
use aq_admission::preparation_v2::{self as wire, Document};
use aq_server::profiled_execution::{phase_execution::transport, policies};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
const PATH: &str = "/v1/worker/profiled-v2/prepare";
fn request(f: &terminal::Prepared, nonce: Uuid) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(PATH)
        .header(
            "authorization",
            format!("Bearer {}", f.c.env.env.credential),
        )
        .header("content-type", "application/json")
        .header("aq-delivery-nonce", nonce.to_string())
        .body(Body::from(
            serde_json::to_vec(&Document::Prepare(Box::new(f.prepared.clone()))).unwrap(),
        ))
        .unwrap()
}
async fn call(app: &Router, q: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(q).await.unwrap();
    let status = response.status();
    if status != StatusCode::NOT_FOUND {
        assert_eq!(response.headers()["cache-control"], "no-store")
    };
    let bytes = to_bytes(response.into_body(), wire::MAX_BYTES)
        .await
        .unwrap();
    (status, bytes.to_vec())
}
async fn unissued(f: &terminal::Prepared) {
    let (grants,installs):(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_phase_execution_grants),(SELECT count(*) FROM admission_material_receipts)").fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!((grants, installs), (0, 0));
}
#[tokio::test]
async fn actual_production_prepare_issues_once_and_replay_keeps_independent_execution_deadline() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let state = f.c.env.env.state.clone();
    let app = transport::routes(state.clone()).with_state(state);
    let nonce = Uuid::new_v4();
    let (status, bytes) = call(&app, request(&f, nonce)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let Document::ExecutionDelivery(first) = wire::decode(&bytes).unwrap() else {
        panic!("expected actual phase execution delivery")
    };
    first
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, nonce)
        .unwrap();
    let second_nonce = Uuid::new_v4();
    let (status, bytes) = call(&app, request(&f, second_nonce)).await;
    assert_eq!(status, StatusCode::OK);
    let Document::ExecutionDelivery(second) = wire::decode(&bytes).unwrap() else {
        panic!("expected retained phase receipt")
    };
    second
        .validate_for(&f.prepared, &f.a, &f.q, &f.p, second_nonce)
        .unwrap();
    assert_eq!(
        first.receipt.digest().unwrap(),
        second.receipt.digest().unwrap()
    );
    assert_eq!(
        first.receipt.grant.expires_at_ms,
        second.receipt.grant.expires_at_ms
    );
    assert_eq!(
        first.receipt.execution_deadline_ms,
        second.receipt.execution_deadline_ms
    );
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM profiled_phase_execution_grants),(SELECT count(*) FROM profiled_phase_terminal_capacity),(SELECT count(*) FROM profiled_execution_receipts WHERE kind='prepare')").fetch_one(&f.c.env.env.pool).await.unwrap();
    assert_eq!(counts, (1, 1, 0));
}
#[tokio::test]
async fn auth_exact_nonce_type_negotiation_and_body_bounds_refuse_before_issuance() {
    let f = Box::pin(terminal::prepared_terminal()).await;
    let state = f.c.env.env.state.clone();
    let app = transport::routes(state.clone()).with_state(state);
    let mut missing = request(&f, Uuid::new_v4());
    missing.headers_mut().remove("authorization");
    assert_eq!(call(&app, missing).await.0, StatusCode::UNAUTHORIZED);
    for value in ["invalid", "00000000-0000-0000-0000-000000000000"] {
        let mut q = request(&f, Uuid::new_v4());
        q.headers_mut()
            .insert("aq-delivery-nonce", value.parse().unwrap());
        assert_eq!(call(&app, q).await.0, StatusCode::BAD_REQUEST);
    }
    let mut duplicate = request(&f, Uuid::new_v4());
    duplicate.headers_mut().append(
        "aq-delivery-nonce",
        Uuid::new_v4().to_string().parse().unwrap(),
    );
    assert_eq!(call(&app, duplicate).await.0, StatusCode::BAD_REQUEST);
    let mut wrong = request(&f, Uuid::new_v4());
    wrong
        .headers_mut()
        .insert("content-type", "text/plain".parse().unwrap());
    assert_eq!(call(&app, wrong).await.0, StatusCode::BAD_REQUEST);
    let mut unsupported = f.prepared.clone();
    unsupported.negotiation.features.clear();
    let mut q = request(&f, Uuid::new_v4());
    *q.body_mut() =
        Body::from(serde_json::to_vec(&Document::Prepare(Box::new(unsupported))).unwrap());
    assert_eq!(call(&app, q).await.0, StatusCode::BAD_REQUEST);
    let mut huge = request(&f, Uuid::new_v4());
    *huge.body_mut() = Body::from(vec![b' '; wire::MAX_BYTES + 1]);
    assert_eq!(call(&app, huge).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    unissued(&f).await;
}
#[tokio::test]
async fn absent_qualified_cleanup_keeps_transport_inactive_without_legacy_fallback() {
    let mut f = Box::pin(terminal::prepared_terminal()).await;
    let mut config = (*f.c.env.env.state.config).clone();
    let mut p = (**config.profiled_execution.as_ref().unwrap()).clone();
    p.revision += 1;
    p.definition_bodies.retain(|d| {
        !d.artifact()
            .id
            .as_str()
            .starts_with("aq.phase-terminal-qualification/")
    });
    config.profiled_execution = Some(Arc::new(p));
    f.c.env.env.state.config = Arc::new(config);
    policies::install(&f.c.env.env.state).await.unwrap();
    let state = f.c.env.env.state.clone();
    let app = transport::routes(state.clone()).with_state(state);
    assert_eq!(
        call(&app, request(&f, Uuid::new_v4())).await.0,
        StatusCode::NOT_FOUND
    );
    unissued(&f).await;
}
#[tokio::test]
async fn genuine_old_schema_cannot_issue_production_grant_without_reserved_cleanup_capacity() {
    let f =
        aq_server::test_support::with_state32_store(Box::pin(terminal::prepared_terminal())).await;
    let state = f.c.env.env.state.clone();
    let app = transport::routes(state.clone()).with_state(state);
    assert_eq!(
        call(&app, request(&f, Uuid::new_v4())).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    unissued(&f).await;
}
