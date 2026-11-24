use super::*;
pub(super) async fn routes_and_inputs(e: &Env, r: &Value) {
    for token in [None, Some(e.node_token.as_str()), Some("aqa_invalid")] {
        assert_eq!(
            send(
                &e.app,
                "/v1/agent/messages/send",
                token,
                Some(message(r.clone()))
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        send(&e.app, "/api/messages/send", None, Some(message(r.clone())))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &e.admin,
            "/v1/agent/messages/send",
            Some(&e.token),
            Some(message(r.clone()))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for bad in ["", &"é".repeat(8193)] {
        let mut m = message(r.clone());
        m["body"] = json!(bad);
        assert_eq!(
            send(&e.admin, "/api/messages/send", None, Some(m)).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut impersonation = message(r.clone());
    impersonation["sender"] = json!({"kind":"owner"});
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(impersonation))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
pub(super) async fn capabilities(e: &Env, r: &Value) -> Uuid {
    let attempt: Uuid = e.binding["attempt_id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[]') WHERE attempt_id=$1").bind(attempt).execute(&e.pool).await.unwrap();
    for path in ["send", "inbox", "ack"] {
        let request = match path {
            "send" => message(r.clone()),
            "inbox" => json!({"recipient":r,"after":"0","limit":1}),
            _ => {
                json!({"operation_id":Uuid::new_v4(),"recipient":r,"message_id":Uuid::new_v4(),"disposition":"received"})
            }
        };
        assert_eq!(
            send(
                &e.app,
                &format!("/v1/agent/messages/{path}"),
                Some(&e.token),
                Some(request)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    sqlx::query("UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[\"message.send\",\"message.read\",\"message.ack\"]') WHERE attempt_id=$1").bind(attempt).execute(&e.pool).await.unwrap();
    attempt
}
pub(super) async fn expiry_and_cancellation(e: &Env, r: &Value, attempt: Uuid) {
    let m = message(r.clone());
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/send",
            Some(&e.token),
            Some(m.clone())
        )
        .await
        .0,
        StatusCode::OK
    );
    e.time.store(1_010_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/send",
            Some(&e.token),
            Some(m.clone())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    e.time.store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{cancellation_id}',$2) WHERE attempt_id=$1").bind(attempt).bind(json!(Uuid::new_v4())).execute(&e.pool).await.unwrap();
    assert_eq!(list(e, r.clone(), "0").await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        send(&e.app, "/v1/agent/messages/send", Some(&e.token), Some(m))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mailbox_messages")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
}
