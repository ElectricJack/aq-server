use super::*;

async fn bind_session(
    f: &common::profiled::Fixture,
    pool: &sqlx::PgPool,
    attempt: Uuid,
    session: &Value,
) {
    let l = &f.selection.lineage;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
        .bind(attempt).bind(f.node.node_id)
        .bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id,"attempt_id":attempt,"session_id":session,"generation":1}))
        .execute(pool).await.unwrap();
}

#[tokio::test]
async fn session_message_send_enqueues_to_bound_session_mailbox() {
    let f = fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, list) = get(&app, "/api/v1/sessions").await;
    let session = list["data"]["items"][0]["id"].clone();
    let path = format!("/api/v1/sessions/{}", session.as_str().unwrap());
    let (_, detail) = get(&app, &path).await;
    assert_eq!(detail["data"]["permissions"], json!(["session.inspect"]));
    bind_session(&f, &pool, attempt, &session).await;
    let (_, detail) = get(&app, &path).await;
    assert_eq!(
        detail["data"]["permissions"],
        json!(["session.inspect", "session.message.send", "session.nudge"])
    );
    let (_, b) = get(&app, "/api/v1/bootstrap").await;
    assert!(
        b["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "session.message.enqueue" && c["enabled"] == true)
    );
    let action = |generation: &str, body: &str, op: Uuid, message: Uuid| {
        json!({"operation_id":op,"kind":"session.message.send",
            "subject":{"type":"session","id":session,"revision":generation},
            "expected_versions":{"session":generation},
            "payload":{"operation_id":op,"project_id":f.selection.lineage.project_id,"message_id":message,"expected_generation":generation,"body":body}})
    };
    let (op, message) = (Uuid::new_v4(), Uuid::new_v4());
    let request = action("1", "Please rebase", op, message);
    let (status, result) = submit(&app, &request).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["data"]["disposition"], "confirmed");
    assert_eq!(result["data"]["affected_entities"][0]["type"], "message");
    assert_eq!(result["data"]["affected_entities"][0]["id"], json!(message));
    let (_, replay) = submit(&app, &request).await;
    assert_eq!(replay["data"], result["data"]);
    let (_, lookup) = get(&app, &format!("/api/v1/operations/{op}")).await;
    assert_eq!(lookup["data"], result["data"]);
    let changed = action("1", "changed", op, message);
    assert_eq!(submit(&app, &changed).await.0, StatusCode::CONFLICT);
    let queued: Value =
        sqlx::query_scalar("SELECT recipient FROM mailbox_messages WHERE message_id=$1")
            .bind(message)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(queued["kind"], "session");
    assert_eq!(queued["session_id"], session);
    let stale = action("2", "stale", Uuid::new_v4(), Uuid::new_v4());
    let (_, result) = submit(&app, &stale).await;
    assert_eq!(result["data"]["disposition"], "conflict", "{result}");
    assert_eq!(result["data"]["current_versions"], json!({"session":"1"}));
    let unknown = action("1", "nobody", Uuid::new_v4(), Uuid::new_v4());
    let mut unknown = unknown;
    unknown["subject"]["id"] = json!(Uuid::new_v4());
    assert_eq!(submit(&app, &unknown).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn session_nudge_action_uses_the_stall_nudge_path() {
    let f = fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, list) = get(&app, "/api/v1/sessions").await;
    let session = list["data"]["items"][0]["id"].clone();
    bind_session(&f, &pool, attempt, &session).await;
    let op = Uuid::new_v4();
    let request = json!({"operation_id":op,"kind":"session.nudge",
        "subject":{"type":"session","id":session,"revision":"1"},
        "expected_versions":{"session":"1"},
        "payload":{"operation_id":op,"project_id":f.selection.lineage.project_id,"expected_generation":"1","body":"Are you stuck?"}});
    let (status, result) = submit(&app, &request).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["data"]["disposition"], "confirmed");
    let message = Uuid::new_v5(&op, b"stall-nudge-message");
    assert_eq!(result["data"]["affected_entities"][0]["id"], json!(message));
    let (_, replay) = submit(&app, &request).await;
    assert_eq!(replay["data"], result["data"]);
    let (inbox_only, sender): (bool, Value) = sqlx::query_as("SELECT n.inbox_only,m.sender FROM stall_nudges n JOIN mailbox_messages m USING(message_id) WHERE n.message_id=$1")
        .bind(message).fetch_one(&pool).await.unwrap();
    assert!(!inbox_only, "no terminal: input is ready");
    assert_eq!(sender["kind"], "owner");
}
