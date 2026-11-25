use super::*;
use crate::messaging::Env;
pub(super) async fn edit_and_replay(e: &Env) -> Uuid {
    let mine = Uuid::new_v4();
    let (status, _) = invoke(
        e,
        "task.comment.add",
        json!({"operation_id":mine,"body":"First draft."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let edit = json!({"operation_id":Uuid::new_v4(),"comment_id":mine,"expected_revision":"1","body":"Edited."});
    let (status, body) = invoke(e, "task.comment.edit", edit.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["revision"].clone(), body["comment"]["body"].clone()),
        (json!("2"), json!("Edited."))
    );
    // Replay returns the recorded effect; a stale revision changes nothing.
    let (status, again) = invoke(e, "task.comment.edit", edit.clone()).await;
    assert_eq!(
        (status, again["revision"].clone()),
        (StatusCode::OK, json!("2"))
    );
    let mut stale = edit;
    stale["operation_id"] = json!(Uuid::new_v4());
    let (status, body) = invoke(e, "task.comment.edit", stale).await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::CONFLICT, json!("stale_revision")),
        "{body}"
    );
    mine
}
pub(super) async fn other_authors(e: &Env) {
    let id = |k: &str| Uuid::parse_str(e.binding[k].as_str().unwrap()).unwrap();
    // The owner's comment on the same task is not the agent's to change.
    let theirs = Uuid::new_v4();
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    aq_task_comments::add(
        &e.pool,
        theirs,
        aq_task_comments::Thread {
            project_id: id("project_id"),
            task_id: id("task_id"),
        },
        &aq_task_comments::Author::Owner { user_id: owner },
        "Owner note.",
        chrono::Utc::now(),
    )
    .await
    .unwrap();
    for target in [theirs, Uuid::new_v4()] {
        let (status, body) = invoke(
            e,
            "task.comment.delete",
            json!({"operation_id":Uuid::new_v4(),"comment_id":target,"expected_revision":"1"}),
        )
        .await;
        assert_eq!(
            (status, body["code"].clone()),
            (StatusCode::FORBIDDEN, json!("scope_denied")),
            "{body}"
        );
        assert_eq!(body["grant"], "comment.write");
    }
    // The route's own author check, without the command surface.
    let (status, body) = send(
        &e.app,
        "/v1/agent/comments/edit",
        Some(&e.token),
        Some(json!({"api_version":1,"operation_id":Uuid::new_v4(),"comment_id":theirs,"expected_revision":"1","body":"Mine now."})),
    )
    .await;
    assert_eq!(
        (status, body["code"].clone()),
        (StatusCode::FORBIDDEN, json!("scope_denied")),
        "{body}"
    );
}
pub(super) async fn delete(e: &Env, mine: Uuid) {
    let (status, body) = invoke(
        e,
        "task.comment.delete",
        json!({"operation_id":Uuid::new_v4(),"comment_id":mine,"expected_revision":"2"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["comment"]["body"], Value::Null);
    assert_eq!(
        audit_rows(e, "task.comment.delete").await,
        vec![
            ("deny".into(), Some("scope_denied".into())),
            ("deny".into(), Some("scope_denied".into())),
            ("allow".into(), None)
        ]
    );
}
