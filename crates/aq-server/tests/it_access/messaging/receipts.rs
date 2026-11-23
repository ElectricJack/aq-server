use super::*;
pub(super) async fn verify_replay(e: &Env, m: &Value) {
    let (status, receipt) = send(&e.admin, "/api/messages/send", None, Some(m.clone())).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["cursor"], "1");
    assert_eq!(receipt["harness_delivery"], "not_attempted");
    let (_, replayed) = send(&e.admin, "/api/messages/send", None, Some(m.clone())).await;
    assert_eq!(receipt, replayed);
    for field in ["body", "thread_id", "message_id"] {
        let mut changed = m.clone();
        changed[field] = if field == "body" {
            json!("changed")
        } else {
            json!(Uuid::new_v4())
        };
        assert_eq!(
            send(&e.admin, "/api/messages/send", None, Some(changed))
                .await
                .0,
            StatusCode::CONFLICT
        );
    }
    let mut reused = m.clone();
    reused["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(reused))
            .await
            .0,
        StatusCode::CONFLICT
    );
}
pub(super) async fn acknowledge(e: &Env, r: &Value, m: &Value) {
    let (_, inbox) = list(e, r.clone(), "0").await;
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["messages"][0]["sender"]["kind"], "owner");
    let ack = json!({"operation_id":Uuid::new_v4(),"recipient":r,"message_id":m["message_id"],"disposition":"read"});
    let (status, a) = send(
        &e.app,
        "/v1/agent/messages/ack",
        Some(&e.token),
        Some(ack.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{a}");
    assert_eq!(a["disposition"], "read");
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/ack",
            Some(&e.token),
            Some(ack.clone())
        )
        .await
        .1,
        a
    );
    let mut changed = ack.clone();
    changed["disposition"] = json!("received");
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/ack",
            Some(&e.token),
            Some(changed.clone())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    changed["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/ack",
            Some(&e.token),
            Some(changed)
        )
        .await
        .1["disposition"],
        "read"
    );
}
pub(super) async fn reopen(e: &Env, r: &Value) {
    assert!(
        list(e, r.clone(), "1").await.1["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let reconnected = e.db.store().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    drop(reconnected);
    assert_eq!(
        list(e, r.clone(), "0").await.1["messages"][0]["disposition"],
        "read"
    );
}
pub(super) async fn verify_scope(e: &Env, r: Value) {
    let mut wrong = r.clone();
    wrong["project_id"] = json!(Uuid::new_v4());
    assert_eq!(list(e, wrong.clone(), "0").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/send",
            Some(&e.token),
            Some(message(wrong))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let session = mailbox(e, true);
    let sm = message(session.clone());
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(sm)).await.0,
        StatusCode::OK
    );
    let mut stale = session.clone();
    stale["generation"] = json!(2);
    assert_eq!(list(e, stale.clone(), "0").await.0, StatusCode::CONFLICT);
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(message(stale)))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        list(e, session, "0").await.1["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (_, sent) = send(
        &e.app,
        "/v1/agent/messages/send",
        Some(&e.token),
        Some(message(r.clone())),
    )
    .await;
    assert_eq!(sent["cursor"], "2");
    assert_eq!(
        list(e, r, "1").await.1["messages"][0]["sender"]["binding"],
        e.binding
    );
}
