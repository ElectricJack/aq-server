//! Durable AQ-11 mailboxes against disposable PostgreSQL; real routers and authority.
use aq_server::{
    agent_access::{AgentAccessConfig, Runtime},
    test_support::TestDb,
    tokens,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
mod delivery;
mod fixture;
mod limits;
mod read_lock;
mod receipts;
mod successor;
mod supervisor;
pub(crate) use crate::agent_launch::send;
pub(crate) use fixture::{
    Env, GraphEndpoint, setup, setup_graph, setup_graph_with, setup_graph_with_attempt, setup_with,
};
fn mailbox(e: &Env, session: bool) -> Value {
    let mut r = json!({"kind":"task","workspace_id":e.binding["workspace_id"],"project_id":e.binding["project_id"],"task_id":e.binding["task_id"]});
    if session {
        r["kind"] = json!("session");
        for field in ["session_id", "attempt_id", "generation"] {
            r[field] = e.binding[field].clone();
        }
    }
    r
}
fn message(r: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"thread_id":Uuid::new_v4(),"recipient":r,"body":"Please inspect the retained source."})
}
async fn list(e: &Env, r: Value, after: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        "/v1/agent/messages/inbox",
        Some(&e.token),
        Some(json!({"recipient":r,"after":after,"limit":32})),
    )
    .await
}
#[tokio::test]
async fn durable_identity_scope_cursors_receipts_and_restart() {
    let e = setup().await;
    let r = mailbox(&e, false);
    let m = message(r.clone());
    receipts::verify_replay(&e, &m).await;
    receipts::acknowledge(&e, &r, &m).await;
    receipts::reopen(&e, &r).await;
    receipts::verify_scope(&e, r).await;
}
#[tokio::test]
async fn bounded_inputs_auth_capabilities_and_online_expiry_cancellation() {
    let e = setup().await;
    let r = mailbox(&e, false);
    limits::routes_and_inputs(&e, &r).await;
    let attempt = limits::capabilities(&e, &r).await;
    limits::expiry_and_cancellation(&e, &r, attempt).await;
}
#[tokio::test]
async fn concurrent_sends_have_gapless_durable_mailbox_order() {
    let e = setup().await;
    let r = mailbox(&e, false);
    let m = message(r.clone());
    let (a, b) = tokio::join!(
        send(&e.admin, "/api/messages/send", None, Some(m.clone())),
        send(&e.admin, "/api/messages/send", None, Some(m.clone()))
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(a, b);
    let (a, b) = tokio::join!(
        send(
            &e.admin,
            "/api/messages/send",
            None,
            Some(message(r.clone()))
        ),
        send(
            &e.app,
            "/v1/agent/messages/send",
            Some(&e.token),
            Some(message(r.clone()))
        )
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    let (_, items) = list(&e, r, "0").await;
    assert_eq!(
        items["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["cursor"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["1", "2", "3"]
    );
}
#[tokio::test]
async fn successor_reads_task_mail_but_cannot_consume_predecessor_session_mail() {
    let mut e = setup().await;
    let task = mailbox(&e, false);
    let old_session = mailbox(&e, true);
    successor::deliver_to_predecessor(&e, &task, &old_session).await;
    let token = successor::replace_attempt(&mut e).await;
    successor::read_new_session(&mut e, &task, old_session, token).await;
    successor::verify_next_delivery(&e, task).await;
}
#[tokio::test]
async fn cancellation_and_revocation_serialize_before_new_admission() {
    for revoke in [false, true] {
        let e = setup().await;
        let task = mailbox(&e, false);
        let attempt: Uuid = e.binding["attempt_id"].as_str().unwrap().parse().unwrap();
        let node: Uuid = e.binding["node_id"].as_str().unwrap().parse().unwrap();
        let mut tx = e.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(node)
            .execute(&mut *tx)
            .await
            .unwrap();
        let app = e.app.clone();
        let token = e.token.clone();
        let request = message(task);
        let pending = tokio::spawn(async move {
            send(&app, "/v1/agent/messages/send", Some(&token), Some(request)).await
        });
        // The new request must wait on the same node/attempt authority order.
        tokio::task::yield_now().await;
        if revoke {
            sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE WHERE attempt_id=$1")
                .bind(attempt)
                .execute(&mut *tx)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{cancellation_id}',$2) WHERE attempt_id=$1").bind(attempt).bind(json!(Uuid::new_v4())).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        assert_eq!(pending.await.unwrap().0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mailbox_messages")
                .fetch_one(&e.pool)
                .await
                .unwrap(),
            0
        );
    }
}
fn node_request(e: &Env) -> Value {
    json!({"identity":{"node_id":e.binding["node_id"],"boot_id":e.binding["boot_id"]},"attempt":{"attempt_id":e.binding["attempt_id"],"session_id":e.binding["session_id"],"generation":e.binding["generation"]}})
}
#[tokio::test]
async fn node_delivery_replay_unknown_stale_and_harness_handoff_are_distinct() {
    let e = setup().await;
    let task = mailbox(&e, false);
    let (poll, batch) = delivery::initial_batch(&e, &task).await;
    for (i, disposition) in ["handed_to_harness", "delivery_unknown", "stale"]
        .iter()
        .enumerate()
    {
        delivery::record_disposition(&e, &batch["deliveries"][i]["delivery_id"], disposition).await;
    }
    delivery::verify_complete(&e, task, poll).await;
}
#[tokio::test]
async fn small_profile_reads_non_ascii_body_in_bounded_utf8_chunks() {
    let e = setup().await;
    let recipient = mailbox(&e, false);
    let mut m = message(recipient.clone());
    let text = "é界🙂".repeat(1000);
    m["body"] = json!(text);
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(m.clone()))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{max_response_bytes}','1024')",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    let (status, page) = list(&e, recipient.clone(), "0").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page.to_string().len() <= 1024);
    let summary = &page["messages"][0];
    assert_eq!(
        (
            summary["body_omitted"].clone(),
            summary["recovery"].clone(),
            page["next_cursor"].clone()
        ),
        (json!(true), json!("message_body"), json!("0"))
    );
    assert_eq!(
        page["messages"][0]["body_request"]["message_id"],
        m["message_id"]
    );
    let mut offset = "0".to_string();
    let mut recovered = String::new();
    loop {
        let request = json!({"recipient":recipient,"message_id":m["message_id"],"offset":offset,"max_bytes":16384});
        let (status, chunk) = send(
            &e.app,
            "/v1/agent/messages/body",
            Some(&e.token),
            Some(request),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{chunk}");
        assert!(chunk.to_string().len() <= 1024);
        recovered.push_str(chunk["body"].as_str().unwrap());
        offset = chunk["next_offset"].as_str().unwrap().into();
        if chunk["complete"] == true {
            break;
        }
    }
    assert_eq!(recovered, text);
    let invalid =
        json!({"recipient":recipient,"message_id":m["message_id"],"offset":"1","max_bytes":100});
    assert_eq!(
        send(
            &e.app,
            "/v1/agent/messages/body",
            Some(&e.token),
            Some(invalid)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn credential_expiring_while_waiting_for_operation_lock_admits_nothing() {
    let e = setup().await;
    let task = mailbox(&e, false);
    let actor = tokens::verifier(&json!({"kind":"session","binding":e.binding}).to_string());
    let mut tx = e.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(actor)
        .execute(&mut *tx)
        .await
        .unwrap();
    let app = e.app.clone();
    let token = e.token.clone();
    let request = message(task);
    let pending = tokio::spawn(async move {
        send(&app, "/v1/agent/messages/send", Some(&token), Some(request)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3),async {loop {
        let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory')").fetch_one(&e.pool).await.unwrap();if blocked {break;}tokio::task::yield_now().await;
    }}).await.expect("request reached operation lock");
    e.time.store(1_010_000, std::sync::atomic::Ordering::SeqCst);
    tx.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mailbox_messages")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn owner_route_checks_role_csrf_and_known_task_identity() {
    let e = setup().await;
    let task = mailbox(&e, false);
    let mut unknown = task.clone();
    unknown["task_id"] = json!(Uuid::new_v4());
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(message(unknown)))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let request = Request::builder()
        .method("POST")
        .uri("/api/messages/send")
        .header("content-type", "application/json")
        .body(Body::from(message(task.clone()).to_string()))
        .unwrap();
    assert_eq!(
        e.admin.clone().oneshot(request).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET role='viewer'")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/send",
            None,
            Some(message(task.clone()))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/inbox",
            None,
            Some(json!({"recipient":task,"after":"0","limit":1}))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn node_poll_requires_read_capability_but_known_delivery_ack_can_finish() {
    let e = setup().await;
    let task = mailbox(&e, false);
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(message(task)))
            .await
            .0,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[]')",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    let mut poll = node_request(&e);
    poll["limit"] = json!(16);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/poll",
            Some(&e.node_token),
            Some(poll.clone())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM mailbox_deliveries")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[\"message.read\"]')").execute(&e.pool).await.unwrap();
    let (_, batch) = send(
        &e.app,
        "/v2/worker/messages/poll",
        Some(&e.node_token),
        Some(poll),
    )
    .await;
    sqlx::query(
        "UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[]')",
    )
    .execute(&e.pool)
    .await
    .unwrap();
    let mut ack = node_request(&e);
    ack["delivery_id"] = batch["deliveries"][0]["delivery_id"].clone();
    ack["operation_id"] = json!(Uuid::new_v4());
    ack["disposition"] = json!("delivery_unknown");
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/messages/ack",
            Some(&e.node_token),
            Some(ack)
        )
        .await
        .0,
        StatusCode::OK,
        "known admitted delivery may report its exact disposition without granting a new read"
    );
}
#[tokio::test]
async fn inbox_and_body_refuse_expiry_during_database_read_lock_wait() {
    for body_chunk in [false, true] {
        read_lock::verify_expiry(body_chunk).await;
    }
}
#[tokio::test]
async fn session_sends_to_current_supervisor_conversation_and_store_reopens() {
    let e = setup().await;
    let (current, message_id) = supervisor::send_to_current(&e).await;
    supervisor::verify_record(&e, message_id).await;
    supervisor::verify_mailbox_access(&e, current).await;
    // Startup storage validation accepts the retained agent mail proof.
    drop(e.db.store().await);
}
