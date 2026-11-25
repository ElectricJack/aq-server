use super::*;
pub(super) async fn deliver_to_predecessor(e: &Env, task: &Value, old_session: &Value) {
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
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/send",
            None,
            Some(message(old_session.clone()))
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut old_poll = node_request(e);
    old_poll["limit"] = json!(16);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(old_poll)
        )
        .await
        .1["deliveries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
pub(super) async fn replace_attempt(e: &mut Env) -> String {
    let old: Uuid = e.binding["attempt_id"].as_str().unwrap().parse().unwrap();
    let new = Uuid::new_v4();
    let session = Uuid::new_v4();
    let offer = Uuid::new_v4();
    let mut execution: Value =
        sqlx::query_scalar("SELECT state FROM execution_attempts WHERE attempt_id=$1")
            .bind(old)
            .fetch_one(&e.pool)
            .await
            .unwrap();
    execution["offer"]["attempt"] = json!({"attempt_id":new,"session_id":session,"generation":2});
    execution["offer"]["offer_id"] = json!(offer);
    sqlx::query("UPDATE execution_attempts SET holds_task=FALSE,state=jsonb_set(state,'{cancellation_id}',$2) WHERE attempt_id=$1").bind(old).bind(json!(Uuid::new_v4())).execute(&e.pool).await.unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) SELECT $2,task_id,node_id,$3,$4,$5 FROM execution_attempts WHERE attempt_id=$1").bind(old).bind(new).bind(offer).bind(session).bind(execution).execute(&e.pool).await.unwrap();
    e.binding["attempt_id"] = json!(new);
    e.binding["session_id"] = json!(session);
    e.binding["generation"] = json!(2);
    let token = format!("aqa_{}", "x".repeat(43));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context,credential_version,credential_verifier,credential_expires_at_ms) SELECT $2,node_id,$3,profile,context,1,$4,credential_expires_at_ms FROM agent_launch_bindings WHERE attempt_id=$1").bind(old).bind(new).bind(&e.binding).bind(tokens::verifier(&token)).execute(&e.pool).await.unwrap();
    token
}
pub(super) async fn read_new_session(e: &mut Env, task: &Value, old_session: Value, token: String) {
    assert_eq!(list(e, task.clone(), "0").await.0, StatusCode::UNAUTHORIZED);
    e.token = token;
    assert_eq!(
        list(e, task.clone(), "0").await.1["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(list(e, old_session, "0").await.0, StatusCode::CONFLICT);
    assert!(
        list(e, mailbox(e, true), "0").await.1["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
pub(super) async fn verify_next_delivery(e: &Env, task: Value) {
    let mut poll = node_request(e);
    poll["limit"] = json!(16);
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
            .is_empty(),
        "unresolved predecessor dispatch cannot become duplicate successor injection"
    );
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(message(task)))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(poll)
        )
        .await
        .1["deliveries"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "fresh queued task mail can target the successor"
    );
}
