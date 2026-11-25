use super::*;
fn recipient(conversation: Uuid) -> Value {
    json!({"kind":"global_supervisor","conversation_id":conversation})
}
fn addressed(conversation: Uuid) -> Value {
    let mut m = message(recipient(conversation));
    m["thread_id"] = json!(conversation);
    m
}
pub(super) async fn send_to_current(e: &Env) -> (Uuid, Uuid) {
    let current: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(&e.pool)
    .await
    .unwrap();
    let path = "/v1/agent/messages/send";
    let other = Uuid::new_v4();
    let (status, body) = send(&e.app, path, Some(&e.token), Some(addressed(other))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send(
        &e.app,
        path,
        Some(&e.token),
        Some(message(recipient(current))),
    )
    .await;
    assert_eq!(body["code"], "invalid_request", "{status} {body}");
    let request = addressed(current);
    let (status, receipt) = send(&e.app, path, Some(&e.token), Some(request.clone())).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["recipient"], recipient(current));
    assert_eq!(
        (receipt["thread_id"].clone(), receipt["disposition"].clone()),
        (json!(current), json!("queued"))
    );
    assert_eq!(
        send(&e.app, path, Some(&e.token), Some(request.clone()))
            .await
            .1,
        receipt
    );
    let message_id: Uuid = request["message_id"].as_str().unwrap().parse().unwrap();
    (current, message_id)
}
pub(super) async fn verify_record(e: &Env, message_id: Uuid) {
    let row: (Value, bool, bool) = sqlx::query_as("SELECT m.sender,m.mailbox_key=central_global_mailbox_key(),central_global_mail_source_valid(m.message_id) FROM mailbox_messages m WHERE m.message_id=$1")
        .bind(message_id).fetch_one(&e.pool).await.unwrap();
    assert_eq!(row.0, json!({"kind":"session","binding":e.binding}));
    assert!(
        row.1 && row.2,
        "agent mail lands in the proven supervisor mailbox"
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT retained_operations FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(&e.pool)
    .await
    .unwrap();
    assert_eq!(retained, 1);
}
pub(super) async fn verify_mailbox_access(e: &Env, current: Uuid) {
    // Owners use the conversation API; neither inbox can read the supervisor mailbox.
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/send",
            None,
            Some(addressed(current))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        list(e, recipient(current), "0").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/inbox",
            None,
            Some(json!({"recipient":recipient(current),"after":"0","limit":1}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
