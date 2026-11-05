//! Durable AQ-11 mailboxes against disposable PostgreSQL; real routers and authority.
use aq_server::{
    AppState,
    agent_access::{AgentAccessConfig, Runtime},
    config::Config,
    identity::verifier_from_config,
    store::NewNode,
    test_support::TestDb,
    tokens,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
async fn send(
    app: &Router,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("content-type", "application/json")
        .header("X-AQ-Request", "1");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let r = app
        .clone()
        .oneshot(
            b.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
struct Env {
    db: TestDb,
    pool: sqlx::PgPool,
    app: Router,
    admin: Router,
    token: String,
    binding: Value,
    time: Arc<std::sync::atomic::AtomicI64>,
    node_token: String,
}
async fn setup() -> Env {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    let node_token = tokens::new_node_credential();
    store
        .create_enrollment("agent-test", owner.id, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let node = store
        .redeem_enrollment(
            "agent-test",
            NewNode {
                name: "agent".into(),
                credential_hash: tokens::verifier(&node_token),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../protocol/agent-access/v1/fixtures/base.json"
    ))
    .unwrap();
    let mut profile = fixture["context"]["profile"].clone();
    for cap in ["message.send", "message.read", "message.ack"] {
        profile["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!(cap));
    }
    let context = fixture["context"]["context"].clone();
    let mut binding = fixture["context"]["binding"].clone();
    binding["node_id"] = json!(node.id);
    binding["profile_digest"] = json!(tokens::verifier(&profile.to_string()));
    let boot = binding["boot_id"].clone();
    let identity = json!({"node_id":node.id,"boot_id":boot});
    let attempt = json!({"attempt_id":binding["attempt_id"],"session_id":binding["session_id"],"generation":1});
    let spec = json!({"project_id":binding["project_id"],"harness":"deterministic-agent-v1","resolved_spec_digest":binding["resolved_spec_digest"],"fixture_output":"done","resources":{"slots":1,"cpu_millis":100,"memory_bytes":1024,"disk_bytes":1024},"required_capabilities":["deterministic-agent-v1","scoped-agent-access-v1"],"bindings":{"source":{"repository_id":Uuid::new_v4(),"binding_generation":1,"logical_identity":"test","base":{"algorithm":"sha1","hex":"1111111111111111111111111111111111111111"}},"graph":{"workspace_id":binding["workspace_id"],"profile_id":profile["profile_id"],"profile_revision":profile["revision"],"profile_digest":binding["profile_digest"],"context_digest":binding["context_digest"]}}});
    let offer_id = Uuid::new_v4();
    let attempt_id: Uuid = binding["attempt_id"].as_str().unwrap().parse().unwrap();
    let task_id: Uuid = binding["task_id"].as_str().unwrap().parse().unwrap();
    let session_id: Uuid = binding["session_id"].as_str().unwrap().parse().unwrap();
    let execution = json!({"task_id":task_id,"offer":{"offer_id":offer_id,"identity":identity,"attempt":attempt,"spec":spec,"offer_expires_at_ms":2_000_000,"reservation_ttl_ms":10000},"status":"granted","grant":null,"grant_expires_at_ms":1_100_000,"last_observation":null,"report":null,"disposition":null,"cancellation_id":null,"cancellation_reason":null,"stopped":false});
    sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2)")
        .bind(node.id)
        .bind(json!({"identity":identity}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,$6)").bind(attempt_id).bind(task_id).bind(node.id).bind(offer_id).bind(session_id).bind(execution).execute(&pool).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let token_file = directory.path().join("graph-token");
    std::fs::write(&token_file, "ordinary-service-credential-0000000000000000").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config: Config = toml::from_str(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#,
    )
    .unwrap();
    let runtime = Runtime::new(&AgentAccessConfig {
        mutation_actor_id: None,
        graph_url: "http://127.0.0.1:9".into(),
        graph_token_file: token_file,
        credential_ttl_ms: 10000,
    })
    .await
    .unwrap();
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock_time = time.clone();
    let state = AppState {
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store: store.clone(),
        clock: Arc::new(move || {
            Utc.timestamp_millis_opt(clock_time.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap()
        }),
        agent_access: Some(Arc::new(runtime)),
        source_retention: None,
    };
    let app = aq_server::build_worker_router(state.clone());
    let admin = aq_server::build_admin_router(state.clone());
    assert_eq!(
        send(
            &admin,
            "/api/agent-access/provision",
            None,
            Some(json!({"attempt_id":attempt_id,"profile":profile,"context":context}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, issued) = send(
        &app,
        "/v2/worker/agent-session",
        Some(&node_token),
        Some(json!({"identity":identity,"attempt":attempt,"expected_credential_version":0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    Env {
        db,
        pool,
        app,
        admin,
        token: issued["token"].as_str().unwrap().into(),
        binding,
        time,
        node_token,
    }
}
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
    let (_, inbox) = list(&e, r.clone(), "0").await;
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
    assert!(
        list(&e, r.clone(), "1").await.1["messages"]
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
        list(&e, r.clone(), "0").await.1["messages"][0]["disposition"],
        "read"
    );
    let mut wrong = r.clone();
    wrong["project_id"] = json!(Uuid::new_v4());
    assert_eq!(list(&e, wrong.clone(), "0").await.0, StatusCode::NOT_FOUND);
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
    let session = mailbox(&e, true);
    let sm = message(session.clone());
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(sm)).await.0,
        StatusCode::OK
    );
    let mut stale = session.clone();
    stale["generation"] = json!(2);
    assert_eq!(list(&e, stale.clone(), "0").await.0, StatusCode::CONFLICT);
    assert_eq!(
        send(&e.admin, "/api/messages/send", None, Some(message(stale)))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        list(&e, session, "0").await.1["messages"]
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
        list(&e, r, "1").await.1["messages"][0]["sender"]["binding"],
        e.binding
    );
}
#[tokio::test]
async fn bounded_inputs_auth_capabilities_and_online_expiry_cancellation() {
    let e = setup().await;
    let r = mailbox(&e, false);
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
    assert_eq!(list(&e, r.clone(), "0").await.0, StatusCode::UNAUTHORIZED);
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
    let mut old_poll = node_request(&e);
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
    assert_eq!(
        list(&e, task.clone(), "0").await.0,
        StatusCode::UNAUTHORIZED
    );
    e.token = token;
    assert_eq!(
        list(&e, task.clone(), "0").await.1["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(list(&e, old_session, "0").await.0, StatusCode::CONFLICT);
    assert!(
        list(&e, mailbox(&e, true), "0").await.1["messages"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut poll = node_request(&e);
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
    let mut poll = node_request(&e);
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
    for (i, disposition) in ["handed_to_harness", "delivery_unknown", "stale"]
        .iter()
        .enumerate()
    {
        let mut ack = node_request(&e);
        ack["operation_id"] = json!(Uuid::new_v4());
        ack["delivery_id"] = batch["deliveries"][i]["delivery_id"].clone();
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
        list(&e, task, "0").await.1["messages"][1]["disposition"],
        "queued",
        "transport evidence cannot forge an agent read receipt"
    );
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
    assert_eq!(page["messages"][0]["body_omitted"], true);
    assert_eq!(page["next_cursor"], "0");
    assert_eq!(page["messages"][0]["recovery"], "message_body");
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
        let e = setup().await;
        let recipient = mailbox(&e, false);
        let m = message(recipient.clone());
        assert_eq!(
            send(&e.admin, "/api/messages/send", None, Some(m.clone()))
                .await
                .0,
            StatusCode::OK
        );
        let mut tx = e.pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE mailbox_messages IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .unwrap();
        let app = e.app.clone();
        let token = e.token.clone();
        let (path, request) = if body_chunk {
            (
                "/v1/agent/messages/body",
                json!({"recipient":recipient,"message_id":m["message_id"],"offset":"0","max_bytes":512}),
            )
        } else {
            (
                "/v1/agent/messages/inbox",
                json!({"recipient":recipient,"after":"0","limit":1}),
            )
        };
        let pending =
            tokio::spawn(async move { send(&app, path, Some(&token), Some(request)).await });
        tokio::time::timeout(std::time::Duration::from_secs(3),async {loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='relation' AND query LIKE '%mailbox_messages%')").fetch_one(&e.pool).await.unwrap();if blocked {break;}tokio::task::yield_now().await;
        }}).await.expect("authenticated read reached mailbox relation lock");
        e.time.store(1_010_000, std::sync::atomic::Ordering::SeqCst);
        tx.commit().await.unwrap();
        let response = pending.await.unwrap();
        assert_eq!(response.0, StatusCode::UNAUTHORIZED, "{response:?}");
        assert_eq!(response.1["code"], "session_expired");
        assert!(response.1.get("messages").is_none());
        assert!(response.1.get("body").is_none());
    }
}
#[tokio::test]
async fn session_sends_to_current_supervisor_conversation_and_store_reopens() {
    let e = setup().await;
    let current: Uuid = sqlx::query_scalar(
        "SELECT conversation_id FROM global_supervisor_conversation WHERE singleton",
    )
    .fetch_one(&e.pool)
    .await
    .unwrap();
    let to = |c: Uuid| json!({"kind":"global_supervisor","conversation_id":c});
    let addressed = |c: Uuid| {
        let mut m = message(to(c));
        m["thread_id"] = json!(c);
        m
    };
    let path = "/v1/agent/messages/send";
    let other = Uuid::new_v4();
    let (status, body) = send(&e.app, path, Some(&e.token), Some(addressed(other))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = send(&e.app, path, Some(&e.token), Some(message(to(current)))).await;
    assert_eq!(body["code"], "invalid_request", "{status} {body}");
    let request = addressed(current);
    let (status, receipt) = send(&e.app, path, Some(&e.token), Some(request.clone())).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["recipient"], to(current));
    assert_eq!(receipt["thread_id"], json!(current));
    assert_eq!(receipt["disposition"], "queued");
    assert_eq!(
        send(&e.app, path, Some(&e.token), Some(request.clone()))
            .await
            .1,
        receipt
    );
    let message_id: Uuid = request["message_id"].as_str().unwrap().parse().unwrap();
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
    assert_eq!(list(&e, to(current), "0").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &e.admin,
            "/api/messages/inbox",
            None,
            Some(json!({"recipient":to(current),"after":"0","limit":1}))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Startup storage validation accepts the retained agent mail proof.
    drop(e.db.store().await);
}
