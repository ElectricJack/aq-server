//! Agent task-comment routes (protocol/agent-comments/v1) under the real
//! per-attempt credential: an agent comments on and reads only its own task.
use crate::messaging::{Env, send, setup_with};
use aq_task_comments::{self as comments, Author, Thread};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

fn uuid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn agent_adds_and_lists_only_its_own_task_thread() {
    let e = setup_with(Some(json!([{"operation":"comment.write","scope":"task:subtree"},{"operation":"comment.read","scope":"project:self"}]))).await;
    let (project, task) = (uuid(&e.binding["project_id"]), uuid(&e.binding["task_id"]));
    let op = verify_comment_and_replay(&e, task).await;
    verify_input_limits(&e).await;
    verify_thread_isolation(&e, project, task).await;
    verify_paging(&e, op).await;
    verify_revocation(&e).await;
}
async fn verify_comment_and_replay(e: &Env, task: Uuid) -> Uuid {
    let op = Uuid::new_v4();
    let request = json!({"api_version":1,"operation_id":op,"body":"Finding: the adapter drops `executions`."});
    let (status, added) = send(
        &e.app,
        "/v1/agent/comments",
        Some(&e.token),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let c = &added["comment"];
    assert_eq!(c["comment_id"], json!(op));
    assert_eq!(c["task_id"], json!(task));
    assert_eq!(c["author"]["kind"], "agent");
    assert_eq!(c["author"]["session_id"], e.binding["session_id"]);
    assert_eq!(c["author"]["attempt_id"], e.binding["attempt_id"]);
    assert_eq!(c["revision"], "1");
    assert_eq!(c["can_edit"], false);
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    let comment_schema = &schema["components"]["schemas"]["HumanComment"];
    assert!(aq_agent_api::check(&schema, comment_schema, c), "{c}");

    // Replay returns the same comment; a changed body under the operation conflicts.
    let (status, replay) = send(&e.app, "/v1/agent/comments", Some(&e.token), Some(request)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, added);
    let changed = json!({"api_version":1,"operation_id":op,"body":"Something else"});
    let (status, body) = send(&e.app, "/v1/agent/comments", Some(&e.token), Some(changed)).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("id_conflict")),
        "{body}"
    );
    op
}
async fn verify_input_limits(e: &Env) {
    // Bounds, versions and authentication.
    for (body, expected) in [
        (
            json!({"api_version":1,"operation_id":Uuid::new_v4(),"body":"  "}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"api_version":1,"operation_id":Uuid::new_v4(),"body":"x".repeat(16385)}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"api_version":1,"operation_id":Uuid::new_v4(),"body":"ok","task_id":Uuid::new_v4()}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"api_version":2,"operation_id":Uuid::new_v4(),"body":"ok"}),
            StatusCode::UPGRADE_REQUIRED,
        ),
    ] {
        let (status, reply) = send(&e.app, "/v1/agent/comments", Some(&e.token), Some(body)).await;
        assert_eq!(status, expected, "{reply}");
    }
    let (status, _) = send(
        &e.app,
        "/v1/agent/comments",
        None,
        Some(json!({"api_version":1,"operation_id":Uuid::new_v4(),"body":"ok"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
async fn verify_thread_isolation(e: &Env, project: Uuid, task: Uuid) {
    // The owner's comment on this task is visible; other tasks' threads are not.
    let now = chrono::Utc::now();
    let owner = Author::Owner {
        user_id: Uuid::new_v4(),
    };
    let mine = Thread {
        project_id: project,
        task_id: task,
    };
    comments::add(&e.pool, Uuid::new_v4(), mine, &owner, "Owner reply", now)
        .await
        .unwrap();
    for other in [
        Thread {
            project_id: project,
            task_id: Uuid::new_v4(),
        },
        Thread {
            project_id: Uuid::new_v4(),
            task_id: task,
        },
    ] {
        comments::add(&e.pool, Uuid::new_v4(), other, &owner, "Not yours", now)
            .await
            .unwrap();
    }
    let (status, page) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["task_id"], json!(task));
    let bodies: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(
        bodies,
        ["Owner reply", "Finding: the adapter drops `executions`."]
    );
    assert_eq!(page["items"][0]["author"]["kind"], "owner");
    assert_eq!(page["next_cursor"], Value::Null);
}
async fn verify_paging(e: &Env, op: Uuid) {
    // Paging by the opaque position.
    let (_, first) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1,"limit":1})),
    )
    .await;
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (_, second) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1,"limit":1,"cursor":cursor})),
    )
    .await;
    assert_eq!(second["items"][0]["comment_id"], json!(op));
    assert_eq!(second["next_cursor"], Value::Null);
    for body in [
        json!({"api_version":1,"limit":101}),
        json!({"api_version":1,"cursor":"forged"}),
    ] {
        let (status, _) = send(
            &e.app,
            "/v1/agent/comments/list",
            Some(&e.token),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
async fn verify_revocation(e: &Env) {
    // A revoked credential can neither read nor write.
    sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE")
        .execute(&e.pool)
        .await
        .unwrap();
    let (status, _) = send(
        &e.app,
        "/v1/agent/comments/list",
        Some(&e.token),
        Some(json!({"api_version":1})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
