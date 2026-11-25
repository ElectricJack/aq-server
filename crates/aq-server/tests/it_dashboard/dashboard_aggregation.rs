//! Dashboard aggregate HTTP behavior with disposable owner fixtures.
use crate::common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

async fn read(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn capacity_fixture(value: &Value) {
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocol/dashboard-read/v1/fixtures/capacity.json");
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}

/// A routing classification about `kind` `id`, already failed (no candidate).
fn internal_request(
    kind: &str,
    id: &str,
    project: Option<uuid::Uuid>,
) -> aq_internal_work::Request {
    use aq_internal_work::{Affected, AnswerShape, Purpose};
    aq_internal_work::Request {
        purpose: Purpose::RoutingClassification,
        instructions: "Compile the routing policy.".into(),
        input: "Policy text".into(),
        answer: AnswerShape::Choice {
            choices: vec!["ok".into()],
            rationale_max_bytes: 512,
        },
        blocking: false,
        affected: Affected {
            kind: kind.into(),
            id: id.into(),
            project_id: project,
        },
        deadline_ms: 60_000,
        max_output_bytes: 4_096,
        escalate: true,
        profile_override: None,
        agent: None,
    }
}

mod attention;
mod capacity;
mod overview;
/// Every owner run is an item: one about an item the dashboard cannot open
/// by id opens the run and names the item; an unknown project is null.
// Spec 0011 §4 W6: a task routing could not route, once with the owner, is
// an attention item that opens the task and says why in words.
mod quota;
mod workers;
