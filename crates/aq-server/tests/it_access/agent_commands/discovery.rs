use super::*;
use crate::messaging::Env;
pub(super) async fn identity(e: &Env) {
    let (status, me) = send(
        &e.app,
        "/api/v1/commands/whoami",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"], "agent");
    for key in ["attempt_id", "task_id", "project_id"] {
        assert_eq!(me[key], e.binding[key], "{key}");
    }
    assert_eq!(me["grants"].as_array().unwrap().len(), 3);
    assert!(me["grants_digest"].as_str().unwrap().starts_with("sha256:"));
    assert!(me["credential"]["expires_at"].is_string());
    // Enough to address its own session mailbox and the supervisor.
    assert_eq!(me["session_id"], e.binding["session_id"]);
    assert_eq!(me["session_generation"], e.binding["generation"]);
    let supervisor: Option<Uuid> = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_optional(&e.pool)
    .await
    .unwrap();
    assert_eq!(me["supervisor_conversation_id"], json!(supervisor));
}
pub(super) async fn published_grants(e: &Env) {
    let (status, list) = send(&e.app, "/api/v1/commands", Some(&e.token), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let names: Vec<&str> = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for expected in [
        "whoami",
        "operation.show",
        "task.comment.add",
        "comment.list",
        "message.send",
    ] {
        assert!(names.contains(&expected), "{expected} in {names:?}");
    }
    // Published means runnable: no grant, no entry.
    for absent in [
        "question.ask",
        "wait.register",
        "task.show",
        "decision.resolve",
    ] {
        assert!(!names.contains(&absent), "{absent}");
    }
    let add = list["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "task.comment.add")
        .unwrap();
    assert_eq!(add["scope"], "task:subtree");
}
pub(super) async fn unavailable_reasons(e: &Env) {
    let (_, all) = send(&e.app, "/api/v1/commands?all=true", Some(&e.token), None).await;
    let reason = |name: &str| {
        all["unavailable"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["reason"].clone())
    };
    for (name, expected) in [
        ("question.ask", "grant_missing"),
        ("decision.resolve", "not_available"),
        ("task.show", "grant_missing"),
        ("task.list", "not_available"),
    ] {
        assert_eq!(reason(name), Some(json!(expected)), "{name}");
    }
    // No credential.
    let (status, body) = send(&e.app, "/api/v1/commands", None, None).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::UNAUTHORIZED, json!("unauthorized"))
    );
}
pub(super) async fn write_and_read_comment(e: &Env) {
    let op = Uuid::new_v4();
    let (status, added) = send(
        &e.app,
        "/api/v1/commands/task.comment.add",
        Some(&e.token),
        Some(json!({"params":{"operation_id":op,"body":"Through the command surface."}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    assert_eq!(added["comment"]["comment_id"], json!(op));
    assert_eq!(added["comment"]["author"]["kind"], "agent");
    // One audit row for the call, not one per layer.
    assert_eq!(
        audit_rows(e, "task.comment.add").await,
        vec![("allow".into(), None)]
    );
    let (status, page) = send(
        &e.app,
        "/api/v1/commands/comment.list",
        Some(&e.token),
        Some(json!({"params":{"limit":5}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"][0]["comment_id"], json!(op));
}
pub(super) async fn question_audit(e: &Env) {
    // Refusals are the §6.8 shape and audited.
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/question.ask",
        Some(&e.token),
        Some(json!({"params":{"operation_id":Uuid::new_v4(),"question":"May I?"}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let refusal: aq_agent_grants::Refusal = serde_json::from_value(body.clone()).unwrap();
    assert_eq!(refusal.code, aq_agent_grants::RefusalCode::GrantMissing);
    assert_eq!(refusal.grant.as_deref(), Some("question.ask"));
    assert_eq!(
        audit_rows(e, "question.ask").await,
        vec![("deny".into(), Some("grant_missing".into()))]
    );
}
pub(super) async fn command_refusals(e: &Env) {
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/decision.resolve",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("owner_only")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/task.list",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("not_available")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/no.such",
        Some(&e.token),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::NOT_FOUND, json!("unknown_command")),
        "{body}"
    );
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/comment.list",
        Some(&e.token),
        Some(json!({"bogus":1})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_request")),
        "{body}"
    );
}
pub(super) async fn foreign_project(e: &Env) {
    // A foreign project is outside project:self.
    let (status, body) = send(
        &e.app,
        "/api/v1/commands/knowledge.search",
        Some(&e.token),
        Some(json!({"params":{"text":"x","project_id":Uuid::new_v4()}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}
