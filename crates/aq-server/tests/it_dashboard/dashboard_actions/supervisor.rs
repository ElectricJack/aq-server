use super::*;

fn message_action(id: &Value, payload: Value) -> Value {
    json!({"operation_id":payload["operation_id"],"kind":"supervisor.message.send",
        "subject":{"type":"conversation","id":id,"revision":payload["expected_conversation_revision"]},
        "expected_versions":{"conversation":payload["expected_conversation_revision"]},"payload":payload})
}

#[tokio::test]
async fn supervisor_action_and_legacy_share_one_enqueue_and_retained_operation() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, discovery) = get(&app, "/api/human/conversations/global").await;
    assert_eq!(status, StatusCode::OK, "{discovery}");
    let id = &discovery["conversation"]["id"];
    for legacy_first in [false, true] {
        let payload = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),
            "expected_conversation_revision":"1","body":"Hello ☃","context":[],"attachments":[]});
        let action = message_action(id, payload.clone());
        let legacy = format!("/api/human/conversations/{}/messages", id.as_str().unwrap());
        if legacy_first {
            let (status, receipt) = call(&app, &legacy, Some(&payload), "owner@localhost").await;
            assert_eq!(status, StatusCode::OK, "{receipt}");
        }
        let (status, result) = submit(&app, &action).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["data"]["disposition"], "confirmed");
        assert_eq!(result["data"]["affected_entities"][0]["type"], "message");
        assert_eq!(
            result["data"]["affected_entities"][0]["id"],
            payload["message_id"]
        );
        let (status, receipt) = call(&app, &legacy, Some(&payload), "owner@localhost").await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["state"], "queued");
        assert_eq!(receipt["host_delivery"], "not_attempted");
        assert!(receipt["agent_receipt"].is_null());
        let (_, replay) = submit(&app, &action).await;
        assert_eq!(replay["data"], result["data"]);
        let (_, lookup) = get(
            &app,
            &format!(
                "/api/v1/operations/{}",
                payload["operation_id"].as_str().unwrap()
            ),
        )
        .await;
        assert_eq!(lookup["data"], result["data"]);
        let mut changed = action.clone();
        changed["payload"]["body"] = json!("changed");
        assert_eq!(submit(&app, &changed).await.0, StatusCode::CONFLICT);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_operations WHERE response->>'contract'='human-supervisor-conversation-1'")
        .fetch_one(&f.db.pool().await).await.unwrap();
    assert_eq!(count, 2);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM dashboard_operations")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn supervisor_action_lookup_reconciles_native_commit_without_reenqueue() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, d) = get(&app, "/api/human/conversations/global").await;
    let action = message_action(
        &d["conversation"]["id"],
        json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),
        "expected_conversation_revision":"1","body":"Durable admission only","context":[],"attachments":[]}),
    );
    let pool = f.db.pool().await;
    sqlx::query("CREATE FUNCTION refuse_finish() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test interruption'; END $$").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_finish BEFORE UPDATE ON dashboard_operations FOR EACH ROW EXECUTE FUNCTION refuse_finish()").execute(&pool).await.unwrap();
    let (status, admitted) = submit(&app, &action).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    assert_eq!(admitted["data"]["disposition"], "pending");
    sqlx::query("DROP TRIGGER refuse_finish ON dashboard_operations")
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/v1/operations/{}",
        action["operation_id"].as_str().unwrap()
    );
    let (status, recovered) = get(&app, &path).await;
    assert_eq!(status, StatusCode::OK, "{recovered}");
    assert_eq!(recovered["data"]["disposition"], "confirmed");
    assert_eq!(
        recovered["data"]["request_digest"],
        admitted["data"]["request_digest"]
    );
    let (_, replay) = submit(&app, &action).await;
    assert_eq!(replay["data"], recovered["data"]);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM mailbox_operations WHERE operation_id=$1")
            .bind(
                action["operation_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, &path).await.0, StatusCode::NOT_FOUND);
    let (_, b) = get(&app, "/api/v1/bootstrap").await;
    assert_eq!(b["pending_operations"], json!([]));
    assert!(
        !b["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "dashboard.actions")
            .unwrap()["enabled"]
            .as_bool()
            .unwrap()
    );
}

#[tokio::test]
async fn supervisor_union_bounds_identity_and_concurrent_legacy_admission() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, discovery) = get(&app, "/api/human/conversations/global").await;
    assert_eq!(status, 200, "{discovery}");
    let conversation = &discovery["conversation"]["id"];
    let payload = json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":"é".repeat(8192),"context":[],"attachments":[]});
    let original = message_action(conversation, payload.clone());
    for (section, field, value) in [
        ("payload", "body", json!("é".repeat(8193))),
        ("payload", "context", json!([{}])),
        ("payload", "attachments", json!([{}])),
        ("payload", "message_id", payload["operation_id"].clone()),
        ("payload", "unexpected", json!(true)),
        ("expected_versions", "conversation", json!("2")),
    ] {
        let mut bad = original.clone();
        bad[section][field] = value;
        let (status, body) = submit(&app, &bad).await;
        assert_eq!(status, 422, "{body}");
    }
    let legacy = format!(
        "/api/human/conversations/{}/messages",
        conversation.as_str().unwrap()
    );
    let (union, native) = tokio::join!(
        submit(&app, &original),
        call(&app, &legacy, Some(&payload), "owner@localhost")
    );
    assert!(matches!(union.0.as_u16(), 200 | 202), "{union:?}");
    // A racing legacy handler may arrive before native admission. It returns a
    // retryable native error; neither route can authorize another dispatch.
    assert!(matches!(native.0.as_u16(), 200 | 503), "{native:?}");
    let id = payload["operation_id"].as_str().unwrap();
    let (status, retained) = get(&app, &format!("/api/v1/operations/{id}")).await;
    assert_eq!(status, 200, "{retained}");
    assert_eq!(retained["data"]["disposition"], "confirmed");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&f.db.pool().await).await.unwrap();
    let (_, hidden) = call(
        &app,
        &format!("/api/v1/operations/{id}"),
        None,
        "other@localhost",
    )
    .await;
    assert_eq!(hidden["code"], "not_found");
    assert!(hidden.get("operation_id").is_none());
}
