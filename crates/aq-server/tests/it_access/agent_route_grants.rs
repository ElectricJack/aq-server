//! Spec 0006 phase 0: every `/v1/agent/*` question, comment and wait route
//! refuses an attempt that does not hold its grant, names the missing grant,
//! and records each decision in `command_audit`.
use crate::messaging::{Env, send, setup_with};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const ROUTES: [(&str, &str, &str); 14] = [
    ("/v1/agent/questions", "question.ask", "question.ask"),
    ("/v1/agent/questions/show", "question.show", "question.read"),
    ("/v1/agent/comments", "task.comment.add", "comment.write"),
    ("/v1/agent/comments/list", "comment.list", "comment.read"),
    (
        "/v1/agent/comments/edit",
        "task.comment.edit",
        "comment.write",
    ),
    (
        "/v1/agent/comments/delete",
        "task.comment.delete",
        "comment.write",
    ),
    ("/v1/agent/waits/register", "wait.register", "wait.register"),
    ("/v1/agent/waits/inspect", "wait.inspect", "wait.read"),
    ("/v1/agent/waits/ack", "wait.ack", "wait.ack"),
    ("/v1/agent/waits/list", "wait.list", "wait.read"),
    ("/v1/agent/waits/show", "wait.show", "wait.read"),
    (
        "/v1/agent/results/list",
        "result.review.list",
        "result.read",
    ),
    (
        "/v1/agent/results/show",
        "result.review.show",
        "result.read",
    ),
    (
        "/v1/agent/results/patch",
        "result.review.patch",
        "result.read",
    ),
];

async fn structured(e: &Env, path: &str) -> (StatusCode, Value) {
    let r = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("X-AQ-Request", "1")
                .header("X-AQ-Refusal-Version", "1")
                .header("authorization", format!("Bearer {}", e.token))
                .body(Body::from(json!({"api_version":1}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn agent_routes_refuse_without_their_grant_and_name_it() {
    // Messaging only: none of the question, comment or wait grants.
    let e = setup_with(Some(json!([
        {"operation":"message.send","scope":"project:self"},
        {"operation":"comment.read","scope":"attempt:self"}
    ])))
    .await;
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/schema.json"
    ))
    .unwrap();
    for (path, command, grant) in ROUTES {
        // Default: the closed agent-access-1 ErrorResponse, naming the grant.
        let (status, body) =
            send(&e.app, path, Some(&e.token), Some(json!({"api_version":1}))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} {body}");
        assert!(
            aq_agent_api::check(&schema, &schema["$defs"]["ErrorResponse"], &body),
            "{path} {body}"
        );
        assert!(body["message"].as_str().unwrap().contains(grant), "{body}");
        // Opt-in: the spec 0006 §6.8 refusal.
        let (status, body) = structured(&e, path).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} {body}");
        assert_eq!(body["command"], command, "{body}");
        assert_eq!(body["grant"], grant, "{body}");
        assert_eq!(body["effect"], "not_applied");
        assert!(body["request_id"].is_string());
        // The commands-1 Refusal (closed; protocol/commands/v1/check.py
        // validates the same shape against the schema).
        let refusal: aq_agent_grants::Refusal = serde_json::from_value(body.clone()).unwrap();
        assert_eq!(refusal.body(), body);
        if path == "/v1/agent/comments/list" {
            // Held, but attempt:self does not reach the task thread.
            assert_eq!(body["code"], "scope_denied", "{body}");
            assert_eq!(
                body["held"],
                json!([{"operation":"comment.read","scope":"attempt:self"}])
            );
            assert!(body["message"].as_str().unwrap().contains("attempt:self"));
        } else {
            assert_eq!(body["code"], "grant_missing", "{body}");
            // Only the caller's own grants of the same family are shown.
            assert!(body["held"].as_array().unwrap().iter().all(|g| {
                g["operation"].as_str().unwrap().split('.').next() == grant.split('.').next()
            }));
        }
    }
    let rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT command,decision,reason_code,grant_operation FROM command_audit WHERE attempt_id=$1 ORDER BY recorded_at_ms,command",
    )
    .bind(Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap())
    .fetch_all(&e.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2 * ROUTES.len());
    assert!(rows.iter().all(|r| r.1 == "deny" && r.2.is_some()));
}

#[tokio::test]
async fn held_grant_admits_the_route_and_audits_the_allow() {
    let e = setup_with(Some(json!([
        {"operation":"comment.write","scope":"task:subtree"},
        {"operation":"comment.read","scope":"project:self"}
    ])))
    .await;
    let op = Uuid::new_v4();
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments",
        Some(&e.token),
        Some(json!({"api_version":1,"operation_id":op,"body":"Granted comment."})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let allowed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM command_audit WHERE attempt_id=$1 AND decision='allow' AND caller_kind='agent'",
    )
    .bind(Uuid::parse_str(e.binding["attempt_id"].as_str().unwrap()).unwrap())
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!(allowed, 2);
    // A binding sealed without grants holds none.
    let e = setup_with(None).await;
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "unsupported_capability");
}

#[test]
fn route_contracts_publish_the_grant_each_route_checks() {
    let mut published = std::collections::BTreeMap::new();
    for raw in [
        include_str!("../../../../protocol/agent-questions/v1/routes.json"),
        include_str!("../../../../protocol/agent-comments/v1/routes.json"),
        include_str!("../../../../protocol/agent-waits/v1/routes.json"),
        include_str!("../../../../protocol/agent-results/v1/routes.json"),
    ] {
        let routes: Value = serde_json::from_str(raw).unwrap();
        for r in routes["routes"].as_array().unwrap() {
            published.insert(
                r["path"].as_str().unwrap().to_owned(),
                r["grant"].as_str().unwrap().to_owned(),
            );
        }
    }
    let served: std::collections::BTreeMap<_, _> = aq_agent_grants::AGENT_ROUTE_GRANTS
        .iter()
        .map(|(path, _, grant)| (path.to_string(), grant.to_string()))
        .collect();
    assert_eq!(published, served);
    assert_eq!(
        ROUTES.map(|(p, c, g)| (p, c, g)).to_vec(),
        aq_agent_grants::AGENT_ROUTE_GRANTS.to_vec()
    );
}

/// `result.read` reaches a project, never a task or the attempt alone, and a
/// project grant for another project is not this project's.
#[tokio::test]
async fn result_review_routes_need_project_scope_and_stay_in_the_attempts_project() {
    reject_narrow_result_scopes().await;
    let e = setup_with(Some(
        json!([{"operation":"result.read","scope":"project:self"}]),
    ))
    .await;
    let list = |body: Value| send(&e.app, "/v1/agent/results/list", Some(&e.token), Some(body));
    let (status, page) = list(json!({"api_version":1})).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["project_id"], e.binding["project_id"]);
    assert_eq!(page["complete"], true);
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["unavailable"], json!([]));
    // The project is the binding's: a body cannot name another.
    let (status, body) = list(json!({"api_version":1,"project_id":Uuid::new_v4()})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = list(json!({"api_version":1,"limit":26})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = list(json!({"api_version":2})).await;
    assert_eq!(status, StatusCode::UPGRADE_REQUIRED);
    // An unknown report, a malformed digest and a report of no project are
    // all "not found" or invalid; none says another project holds it.
    let show = |body: Value| send(&e.app, "/v1/agent/results/show", Some(&e.token), Some(body));
    let (status, body) =
        show(json!({"api_version":1,"report_id":Uuid::new_v4(),"revision":"a".repeat(64)})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) =
        show(json!({"api_version":1,"report_id":Uuid::new_v4(),"revision":"short"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &e.app,
        "/v1/agent/results/show",
        None,
        Some(json!({"api_version":1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The patch route reads the same exact revisions: an unknown report is
    // not found, the byte bound is 1..=4 MiB, and the project is the binding's.
    let patch = |body: Value| {
        send(
            &e.app,
            "/v1/agent/results/patch",
            Some(&e.token),
            Some(body),
        )
    };
    let query = json!({"api_version":1,"report_id":Uuid::new_v4(),"revision":"a".repeat(64)});
    let (status, body) = patch(query.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    for max_bytes in [0, 4_194_305] {
        let mut bounded = query.clone();
        bounded["max_bytes"] = json!(max_bytes);
        let (status, body) = patch(bounded).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{max_bytes} {body}");
    }
    let mut other_project = query.clone();
    other_project["project_id"] = json!(Uuid::new_v4());
    assert_eq!(patch(other_project).await.0, StatusCode::BAD_REQUEST);
    let mut short = query.clone();
    short["revision"] = json!("short");
    assert_eq!(patch(short).await.0, StatusCode::BAD_REQUEST);
    let mut future = query.clone();
    future["api_version"] = json!(2);
    assert_eq!(patch(future).await.0, StatusCode::UPGRADE_REQUIRED);
    let (status, _) = send(&e.app, "/v1/agent/results/patch", None, Some(query)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
async fn reject_narrow_result_scopes() {
    for scope in ["attempt:self", "task:self", "task:subtree"] {
        let e = setup_with(Some(json!([{"operation":"result.read","scope":scope}]))).await;
        for path in [
            "/v1/agent/results/list",
            "/v1/agent/results/show",
            "/v1/agent/results/patch",
        ] {
            let (status, body) = structured(&e, path).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{scope} {path} {body}");
            assert_eq!(body["code"], "scope_denied", "{scope} {path} {body}");
            assert_eq!(body["grant"], "result.read");
        }
    }
    let other = format!("project:{}", Uuid::new_v4());
    let e = setup_with(Some(json!([{"operation":"result.read","scope":other}]))).await;
    let (status, body) = structured(&e, "/v1/agent/results/list").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "scope_denied", "{body}");
}
