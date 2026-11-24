use super::*;
pub(super) async fn initial_batch(e: &Env, task: &Value) -> (Value, Value) {
    for _ in 0..3 {
        assert_eq!(
            send(
                &e.admin,
                "/api/messages/send",
                None,
                Some(message(task.clone()))
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let mut poll = node_request(e);
    poll["limit"] = json!(16);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.token),
            Some(poll.clone())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, batch) = send(
        &e.app,
        "/v2/worker/messages/poll",
        Some(&e.node_token),
        Some(poll.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{batch}");
    assert_eq!(batch["deliveries"].as_array().unwrap().len(), 3);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(poll.clone())
        )
        .await
        .1,
        batch
    );
    (poll, batch)
}
pub(super) async fn record_disposition(e: &Env, delivery_id: &Value, disposition: &str) {
    let mut ack = node_request(e);
    ack["operation_id"] = json!(Uuid::new_v4());
    ack["delivery_id"] = delivery_id.clone();
    ack["disposition"] = json!("received_by_node");
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/ack",
            Some(&e.node_token),
            Some(ack.clone())
        )
        .await
        .1["disposition"],
        "received_by_node"
    );
    ack["operation_id"] = json!(Uuid::new_v4());
    ack["disposition"] = json!(disposition);
    let a = send(
        &e.app,
        "/v2/worker/messages/ack",
        Some(&e.node_token),
        Some(ack.clone()),
    )
    .await;
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(a.1["disposition"], json!(disposition));
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/ack",
            Some(&e.node_token),
            Some(ack.clone())
        )
        .await,
        a
    );
    ack["disposition"] = json!("received_by_node");
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/ack",
            Some(&e.node_token),
            Some(ack.clone())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    ack["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/ack",
            Some(&e.node_token),
            Some(ack)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
pub(super) async fn verify_complete(e: &Env, task: Value, poll: Value) {
    assert!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(poll.clone())
        )
        .await
        .1["deliveries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut wrong = poll.clone();
    wrong["attempt"]["generation"] = json!(2);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(wrong)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        list(e, task, "0").await.1["messages"][1]["disposition"],
        "queued",
        "transport evidence cannot forge an agent read receipt"
    );
}
