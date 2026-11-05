//! Actual durable AQ mailbox foundation, without worker/runtime/provider effects.
use aq_server::{
    AppState, agent_access::supervisor_conversation, authz, config::Config,
    identity::verifier_from_config, test_support::TestDb,
};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
struct Fixture {
    db: TestDb,
    state: AppState,
    owner: Uuid,
    app: Router,
}
async fn fixture() -> Fixture {
    let db = TestDb::create().await;
    let store = db.store().await;
    let now = Utc.timestamp_millis_opt(1000000).unwrap();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap()
        .id;
    let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
    let state = AppState {
        store,
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        clock: Arc::new(move || now),
        agent_access: None,
        source_retention: None,
    };
    let app = supervisor_conversation::routes(&state)
        .layer(from_fn_with_state(state.clone(), authz::csrf_guard))
        .with_state(state.clone());
    Fixture {
        db,
        state,
        owner,
        app,
    }
}
async fn request(app: &Router, path: &str, body: Option<&str>, email: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(path)
                .header("x-aq-dev-email", email)
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(body.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
fn schema(v: &Value) {
    let s: Value = serde_json::from_str(include_str!(
        "../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}
async fn discovery(f: &Fixture) -> Uuid {
    let (c, v) = request(
        &f.app,
        "/api/human/conversations/global",
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(c, StatusCode::OK, "{v}");
    schema(&v);
    assert_eq!(v["runtime"]["state"], "unavailable");
    assert!(v["runtime"]["binding"].is_null());
    v["conversation"]["id"].as_str().unwrap().parse().unwrap()
}
fn create(body: &str) -> Value {
    json!({"operation_id":Uuid::new_v4(),"message_id":Uuid::new_v4(),"expected_conversation_revision":"1","body":body,"context":[],"attachments":[]})
}
#[tokio::test]
async fn history_replay_retained_refusals_and_shared_owner_scope_use_one_mailbox() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    assert_eq!(id, discovery(&f).await);
    let path = format!("/api/human/conversations/{id}/messages");
    let (_, empty) = request(&f.app, &path, None, "owner@localhost").await;
    schema(&empty);
    assert_eq!(empty["through_cursor"], "0");
    assert_eq!(empty["items"], json!([]));
    let first = create("Hello ☃\nPlease inspect the next milestone.");
    let raw = first.to_string();
    let (c, accepted) = request(&f.app, &path, Some(&raw), "owner@localhost").await;
    assert_eq!(c, StatusCode::OK, "{accepted}");
    schema(&accepted);
    assert_eq!(accepted["state"], "queued");
    assert_eq!(accepted["actor_id"], json!(f.owner));
    assert_eq!(accepted["cursor"], "1");
    assert_eq!(accepted["host_delivery"], "not_attempted");
    assert!(accepted["agent_receipt"].is_null());
    let digest = format!("sha256:{}", aq_agent_api::digest(&first).unwrap());
    assert_eq!(accepted["request_digest"], digest);
    // Drop/ignore the first reply: original operation and even reformatted equivalent JSON recover one receipt.
    let operation = first["operation_id"].as_str().unwrap();
    let lookup = format!("/api/human/conversations/{id}/operations/{operation}");
    assert_eq!(
        request(&f.app, &lookup, None, "owner@localhost").await.1,
        accepted
    );
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&serde_json::to_string_pretty(&first).unwrap()),
            "owner@localhost"
        )
        .await
        .1,
        accepted
    );
    let mut changed = first.clone();
    changed["body"] = json!("Changed body");
    assert_eq!(
        request(&f.app, &path, Some(&changed.to_string()), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut collision = create("A distinct request");
    collision["message_id"] = first["message_id"].clone();
    let (c, refused) = request(
        &f.app,
        &path,
        Some(&collision.to_string()),
        "owner@localhost",
    )
    .await;
    assert_eq!(c, StatusCode::CONFLICT, "{refused}");
    schema(&refused);
    assert_eq!(refused["state"], "not_queued");
    assert_eq!(refused["reason"], "message_id_conflict");
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&collision.to_string()),
            "owner@localhost"
        )
        .await
        .1,
        refused
    );
    collision["message_id"] = json!(Uuid::new_v4());
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&collision.to_string()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let mut stale = create("Retained draft after stale definition");
    stale["expected_conversation_revision"] = json!("2");
    let (c, stale_reply) =
        request(&f.app, &path, Some(&stale.to_string()), "owner@localhost").await;
    assert_eq!(c, StatusCode::CONFLICT, "{stale_reply}");
    schema(&stale_reply);
    assert_eq!(stale_reply["reason"], "conversation_revision_changed");
    let op = stale["operation_id"].as_str().unwrap();
    assert_eq!(
        request(
            &f.app,
            &format!("/api/human/conversations/{id}/operations/{op}"),
            None,
            "owner@localhost"
        )
        .await
        .1,
        stale_reply
    );
    stale["expected_conversation_revision"] = json!("1");
    assert_eq!(
        request(&f.app, &path, Some(&stale.to_string()), "owner@localhost")
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Global enqueue cannot reuse an operation already owned by another AQ-11 command.
    let other_domain = create("Cross-domain operation collision");
    let sender_key = aq_agent_api::digest(&json!({"kind":"owner","user_id":f.owner})).unwrap();
    let other_op: Uuid = other_domain["operation_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) VALUES($1,$2,$3,'{\"api_version\":1,\"disposition\":\"queued\"}')")
        .bind(sender_key).bind(other_op).bind("a".repeat(64)).execute(&pool).await.unwrap();
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&other_domain.to_string()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            &f.app,
            &format!("/api/human/conversations/{id}/operations/{other_op}"),
            None,
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let second = create("Second queued message");
    assert_eq!(
        request(&f.app, &path, Some(&second.to_string()), "owner@localhost")
            .await
            .1["cursor"],
        "2"
    );
    let (_, history) = request(&f.app, &path, None, "owner@localhost").await;
    schema(&history);
    assert_eq!(history["items"].as_array().unwrap().len(), 2);
    assert_eq!(history["items"][0]["body"], first["body"]);
    assert_eq!(history["items"][0]["message_id"], first["message_id"]);
    assert_eq!(history["items"][0]["thread_id"], json!(id));
    assert_eq!(history["items"][0]["author"]["kind"], "human");
    assert_eq!(history["items"][0]["author"]["user_id"], json!(f.owner));
    let other = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'other@localhost','owner','dev','other@localhost',now())").bind(other).execute(&pool).await.unwrap();
    assert_eq!(
        request(&f.app, &path, None, "other@localhost").await.1,
        history
    );
    assert_eq!(
        request(&f.app, &lookup, None, "other@localhost").await.0,
        StatusCode::NOT_FOUND
    );
    let new_app = supervisor_conversation::routes(&f.state).with_state(f.state.clone());
    assert_eq!(
        request(
            &new_app,
            "/api/human/conversations/global",
            None,
            "owner@localhost"
        )
        .await
        .1["conversation"]["id"],
        json!(id)
    );
    for table in [
        "execution_attempts",
        "agent_launch_bindings",
        "mailbox_deliveries",
        "mailbox_receipts",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "no runtime, worker or delivery invented");
    }
    let (n, ops): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM mailbox_messages),(SELECT count(*) FROM mailbox_operations)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((n, ops), (2, 5));
    if optional_browser(&f, id, &first["message_id"], &second["message_id"]).await {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            n, 3,
            "explicit disposable browser probe queues exactly one new message"
        );
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_deliveries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            n, 0,
            "browser enqueue remains logical mail, never worker delivery"
        );
    }
}
#[tokio::test]
async fn max_body_pages_trim_to_byte_budget_and_preserve_both_cursor_directions() {
    let f = fixture().await;
    let id = discovery(&f).await;
    let path = format!("/api/human/conversations/{id}/messages");
    let body = format!("{}x", "\u{1}".repeat(16383));
    for _ in 0..5 {
        let (c, v) = request(
            &f.app,
            &path,
            Some(&create(&body).to_string()),
            "owner@localhost",
        )
        .await;
        assert_eq!(c, StatusCode::OK, "{v}");
    }
    let (_, newest) = request(&f.app, &path, None, "owner@localhost").await;
    schema(&newest);
    assert!(serde_json::to_vec(&newest).unwrap().len() <= 262144);
    assert_eq!(newest["items"].as_array().unwrap().len(), 2);
    assert_eq!(newest["items"][0]["cursor"], "4");
    assert_eq!(newest["items"][1]["cursor"], "5");
    assert_eq!(newest["next_cursor"], "4");
    assert_eq!(newest["has_more"], true);
    let (_, older) = request(&f.app, &format!("{path}?before=4"), None, "owner@localhost").await;
    assert_eq!(older["items"][0]["cursor"], "2");
    assert_eq!(older["items"][1]["cursor"], "3");
    assert_eq!(older["next_cursor"], "2");
    let (_, oldest) = request(&f.app, &format!("{path}?before=2"), None, "owner@localhost").await;
    assert_eq!(oldest["items"][0]["cursor"], "1");
    assert_eq!(oldest["has_more"], false);
    let (_, forward) = request(&f.app, &format!("{path}?after=0"), None, "owner@localhost").await;
    assert_eq!(forward["direction"], "forward");
    assert_eq!(forward["items"][0]["cursor"], "1");
    assert_eq!(forward["items"][1]["cursor"], "2");
    assert_eq!(forward["next_cursor"], "2");
    for query in [
        "before=0",
        "after=01",
        "limit=0",
        "limit=17",
        "before=1&after=0",
        "unknown=1",
    ] {
        assert_eq!(
            request(&f.app, &format!("{path}?{query}"), None, "owner@localhost")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}
#[tokio::test]
async fn owner_current_identity_and_input_bounds_refuse_without_mail_or_worker_fallback() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = format!("/api/human/conversations/{id}/messages");
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'viewer@localhost','viewer','dev','viewer@localhost',now())").bind(viewer).execute(&pool).await.unwrap();
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: Uuid::new_v4(),
        boot_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        assignment_generation: 1,
    };
    let key = aq_server::session_access::session_key(&session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,2000000)").bind(key).bind(json!(session)).bind(viewer).execute(&pool).await.unwrap();
    for uri in [
        "/api/human/conversations/global".to_string(),
        path.clone(),
        format!(
            "/api/human/conversations/{id}/operations/{}",
            Uuid::new_v4()
        ),
    ] {
        assert_eq!(
            request(&f.app, &uri, None, "viewer@localhost").await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&create("Denied").to_string()),
            "viewer@localhost"
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for field in ["context", "attachments"] {
        let mut bad = create("No unsupported references");
        bad[field] = json!([{"id":Uuid::new_v4()}]);
        assert_eq!(
            request(&f.app, &path, Some(&bad.to_string()), "owner@localhost")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for body in [" ", "\0", &"x".repeat(16385)] {
        assert_eq!(
            request(
                &f.app,
                &path,
                Some(&create(body).to_string()),
                "owner@localhost"
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut equal_ids = create("Operation and message are separate identities");
    equal_ids["message_id"] = equal_ids["operation_id"].clone();
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&equal_ids.to_string()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut bad = create("No caller author");
    bad["author"] = json!(viewer);
    assert_eq!(
        request(&f.app, &path, Some(&bad.to_string()), "owner@localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let response = f
        .app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::from(create("No CSRF").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // The request resolves its Owner before waiting on this actual role-row lock.
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(f.owner)
        .execute(&mut *held)
        .await
        .unwrap();
    let app = f.app.clone();
    let path2 = path.clone();
    let pending = tokio::spawn(async move {
        request(
            &app,
            &path2,
            Some(&create("Revoked while waiting").to_string()),
            "owner@localhost",
        )
        .await
    });
    let mut waiting = false;
    for _ in 0..100 {
        waiting=sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-supervisor-conversation' AND wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(waiting, "observe actual authorization lock wait");
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.owner)
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
#[tokio::test]
async fn finite_retention_preserves_original_replay_and_lookup_at_capacity() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = format!("/api/human/conversations/{id}/messages");
    let first = create("Retained at capacity");
    let (_, old) = request(&f.app, &path, Some(&first.to_string()), "owner@localhost").await;
    sqlx::query("UPDATE global_supervisor_conversation SET retained_operations=4096,retained_bytes=67108864 WHERE singleton").execute(&pool).await.unwrap();
    assert_eq!(
        request(
            &f.app,
            &path,
            Some(&create("New refused at capacity").to_string()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        request(&f.app, &path, Some(&first.to_string()), "owner@localhost")
            .await
            .1,
        old
    );
    let op = first["operation_id"].as_str().unwrap();
    assert_eq!(
        request(
            &f.app,
            &format!("/api/human/conversations/{id}/operations/{op}"),
            None,
            "owner@localhost"
        )
        .await
        .1,
        old
    );
    let (_, page) = request(&f.app, &path, None, "owner@localhost").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn dropped_http_reply_recovers_byte_identical_receipt_and_one_committed_message() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = format!("/api/human/conversations/{id}/messages");
    let submission = create("The caller loses this reply after the mailbox commits.");
    let raw = submission.to_string();
    let response = f
        .app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(raw.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Discard the actual response without reading its receipt/body.
    drop(response);
    let op = submission["operation_id"].as_str().unwrap();
    let lookup = format!("/api/human/conversations/{id}/operations/{op}");
    let get = f
        .app
        .clone()
        .oneshot(
            Request::get(lookup)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_bytes = get.into_body().collect().await.unwrap().to_bytes();
    let replay = f
        .app
        .clone()
        .oneshot(
            Request::post(path)
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .body(Body::from(raw))
                .unwrap(),
        )
        .await
        .unwrap();
    let replay_bytes = replay.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        get_bytes, replay_bytes,
        "receipt recovery must preserve exact wire bytes"
    );
    let record: Value = serde_json::from_slice(&get_bytes).unwrap();
    schema(&record);
    let sender = json!({"kind":"owner","user_id":f.owner});
    let private_digest = aq_agent_api::digest(&json!({"kind":"global_supervisor_send","conversation_id":id,"request":submission,"sender":sender})).unwrap();
    let stored_digest: String =
        sqlx::query_scalar("SELECT payload_digest FROM mailbox_messages WHERE message_id=$1")
            .bind(
                submission["message_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_digest, private_digest);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn actual_mount_has_owner_caps_and_csrf_while_standalone_and_viewer_do_not() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'mounted-viewer@localhost','viewer','dev','mounted-viewer@localhost',now())")
        .bind(viewer).execute(&pool).await.unwrap();
    let mounted = aq_server::build_admin_router(f.state.clone());
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    for (app, email, expected) in [
        (&mounted, "owner@localhost", true),
        (&mounted, "mounted-viewer@localhost", false),
        (&standalone, "owner@localhost", false),
    ] {
        let (status, bootstrap) = request(app, "/api/v1/bootstrap", None, email).await;
        assert_eq!(status, StatusCode::OK, "{bootstrap}");
        let caps = bootstrap["data"]["capabilities"].as_array().unwrap();
        for name in ["supervisor.history.read", "supervisor.message.enqueue"] {
            let cap = caps.iter().find(|c| c["name"] == name).unwrap();
            assert_eq!(cap["version"], "1");
            assert_eq!(cap["enabled"], expected);
        }
        assert_eq!(
            caps.iter()
                .find(|c| c["name"] == "supervisor.conversation")
                .unwrap()["enabled"],
            false
        );
        // X4: a mounted owner sees the provisioned conversation, not yet live.
        let global = &bootstrap["data"]["global_supervisor"];
        assert_eq!(
            global["status"],
            if expected {
                "unavailable"
            } else {
                "unconfigured"
            },
            "{global}"
        );
        assert_eq!(global["conversation_id"].is_string(), expected, "{global}");
    }
    let (status, descriptor) = request(
        &mounted,
        "/api/human/conversations/global",
        None,
        "owner@localhost",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{descriptor}");
    let id = descriptor["conversation"]["id"].as_str().unwrap();
    let path = format!("/api/human/conversations/{id}/messages");
    let denied = mounted
        .oneshot(
            Request::post(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::from(create("CSRF blocked").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

/// Serve the actual disposable owner and explicitly supplied compiled bundle.
/// The only browser mutation allowed here is one logical global mailbox enqueue.
async fn optional_browser(f: &Fixture, id: Uuid, first: &Value, second: &Value) -> bool {
    let Some(script) = std::env::var_os("AQ_TEST_SUPERVISOR_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_SUPERVISOR_DASHBOARD_DIR").is_none());
        return false;
    };
    let bundle = std::env::var_os("AQ_TEST_SUPERVISOR_DASHBOARD_DIR")
        .expect("explicit supervisor probe requires compiled dashboard bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let app = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        None,
        None,
        Arc::new(assets),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut probe = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_CONVERSATION_ID", id.to_string())
        .env("AQ_LIVE_ACTOR_ID", f.owner.to_string())
        .env(
            "AQ_LIVE_SUPERVISOR_SEED_IDS",
            json!([first, second]).to_string(),
        )
        .env("AQ_LIVE_ALLOW_SUPERVISOR_ENQUEUE", "1")
        .env_remove("AQ_LIVE_PROJECT_ID")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(35), probe.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded actual supervisor history/enqueue browser probe")
            .unwrap()
            .success()
    );
    true
}
