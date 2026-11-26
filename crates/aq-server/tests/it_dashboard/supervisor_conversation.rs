//! Actual durable AQ mailbox foundation, without worker/runtime/provider effects.
use aq_server::{
    AppState, agent_access::supervisor_conversation, authz, config::Config,
    identity::verifier_from_config, test_support::TestDb,
};
use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
    middleware::from_fn_with_state,
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

const OWNER: &str = "owner@localhost";
const CONFIG: &str = concat!(
    "format_version=1\n",
    "environment='development'\n",
    "[server]\n",
    "admin_bind='127.0.0.1:0'\n",
    "worker_bind='127.0.0.1:0'\n",
    "public_origin='http://127.0.0.1:8080'\n",
    "[auth]\n",
    "mode='dev'\n",
    "dev_email='owner@localhost'\n",
    "[bootstrap]\n",
    "owner_email='owner@localhost'\n",
    "enabled=true\n",
);

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
        .bootstrap_owner(OWNER, "dev", OWNER, now)
        .await
        .unwrap()
        .unwrap()
        .id;
    let config: Config = toml::from_str(CONFIG).unwrap();
    let state = AppState {
        internal_work: Default::default(),
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

/// Sends a prepared request and returns its status and exact body bytes.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Bytes) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    (
        status,
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}
async fn request(app: &Router, path: &str, body: Option<&str>, email: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(path)
        .header("x-aq-dev-email", email)
        .header("x-aq-request", "1")
        .header("content-type", "application/json")
        .body(Body::from(body.unwrap_or("").to_string()))
        .unwrap();
    let (status, bytes) = send(app, request).await;
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
/// An owner GET.
async fn get(app: &Router, path: &str) -> (StatusCode, Value) {
    request(app, path, None, OWNER).await
}
/// An owner POST of a JSON body.
async fn post(app: &Router, path: &str, body: &Value) -> (StatusCode, Value) {
    request(app, path, Some(&body.to_string()), OWNER).await
}
/// An owner POST without the CSRF header.
async fn post_without_csrf(app: &Router, path: &str, body: &Value) -> StatusCode {
    let request = Request::post(path)
        .header("x-aq-dev-email", OWNER)
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, request).await.0
}

fn messages_path(id: Uuid) -> String {
    format!("/api/human/conversations/{id}/messages")
}
fn operation_path(id: Uuid, operation: &Value) -> String {
    let operation = operation.as_str().unwrap();
    format!("/api/human/conversations/{id}/operations/{operation}")
}
async fn insert_user(pool: &PgPool, email: &str, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())")
        .bind(id).bind(email).bind(role).execute(pool).await.unwrap();
    id
}
async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

fn schema(v: &Value) {
    let s: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/schema.json"
    ))
    .unwrap();
    assert!(aq_agent_api::check(&s, &s, v), "public schema {v}");
}
async fn discovery(f: &Fixture) -> Uuid {
    let (c, v) = get(&f.app, "/api/human/conversations/global").await;
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
    let path = messages_path(id);
    let (_, empty) = get(&f.app, &path).await;
    schema(&empty);
    assert_eq!(empty["through_cursor"], "0");
    assert_eq!(empty["items"], json!([]));
    let first = create("Hello ☃\nPlease inspect the next milestone.");
    first_send_recovers_one_receipt(&f, id, &first).await;
    message_id_collision_is_a_retained_refusal(&f.app, &path, &first).await;
    stale_revision_is_a_retained_refusal(&f.app, id).await;
    operation_of_another_command_is_refused(&f, &pool, id).await;
    let second = create("Second queued message");
    assert_eq!(post(&f.app, &path, &second).await.1["cursor"], "2");
    let history = history_lists_both_messages(&f, id, &first).await;
    another_owner_shares_history_but_not_receipts(&f, &pool, id, &first, &history).await;
    let new_app = supervisor_conversation::routes(&f.state).with_state(f.state.clone());
    assert_eq!(
        get(&new_app, "/api/human/conversations/global").await.1["conversation"]["id"],
        json!(id)
    );
    for table in [
        "execution_attempts",
        "agent_launch_bindings",
        "mailbox_deliveries",
        "mailbox_receipts",
    ] {
        assert_eq!(
            count(&pool, table).await,
            0,
            "no runtime, worker or delivery invented"
        );
    }
    let (n, ops): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM mailbox_messages),(SELECT count(*) FROM mailbox_operations)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((n, ops), (2, 5));
    if optional_browser(&f, id, &first["message_id"], &second["message_id"]).await {
        assert_eq!(
            count(&pool, "mailbox_messages").await,
            3,
            "explicit disposable browser probe queues exactly one new message"
        );
        assert_eq!(
            count(&pool, "mailbox_deliveries").await,
            0,
            "browser enqueue remains logical mail, never worker delivery"
        );
    }
}

/// The first send queues one message. Dropping or ignoring its reply loses
/// nothing: the operation lookup and even reformatted equivalent JSON recover
/// the one receipt, while a changed body under the operation conflicts.
async fn first_send_recovers_one_receipt(f: &Fixture, id: Uuid, first: &Value) {
    let path = messages_path(id);
    let (c, accepted) = post(&f.app, &path, first).await;
    assert_eq!(c, StatusCode::OK, "{accepted}");
    schema(&accepted);
    assert_eq!(accepted["state"], "queued");
    assert_eq!(accepted["actor_id"], json!(f.owner));
    assert_eq!(accepted["cursor"], "1");
    assert_eq!(accepted["host_delivery"], "not_attempted");
    assert!(accepted["agent_receipt"].is_null());
    let digest = format!("sha256:{}", aq_agent_api::digest(first).unwrap());
    assert_eq!(accepted["request_digest"], digest);
    let lookup = operation_path(id, &first["operation_id"]);
    assert_eq!(get(&f.app, &lookup).await.1, accepted);
    let pretty = serde_json::to_string_pretty(first).unwrap();
    assert_eq!(
        request(&f.app, &path, Some(&pretty), OWNER).await.1,
        accepted
    );
    let mut changed = first.clone();
    changed["body"] = json!("Changed body");
    assert_eq!(post(&f.app, &path, &changed).await.0, StatusCode::CONFLICT);
}

/// A distinct request reusing a message id is refused, and the refusal is
/// replayed; the same operation with a new message id still conflicts.
async fn message_id_collision_is_a_retained_refusal(app: &Router, path: &str, first: &Value) {
    let mut collision = create("A distinct request");
    collision["message_id"] = first["message_id"].clone();
    let (c, refused) = post(app, path, &collision).await;
    assert_eq!(c, StatusCode::CONFLICT, "{refused}");
    schema(&refused);
    assert_eq!(refused["state"], "not_queued");
    assert_eq!(refused["reason"], "message_id_conflict");
    assert_eq!(post(app, path, &collision).await.1, refused);
    collision["message_id"] = json!(Uuid::new_v4());
    assert_eq!(post(app, path, &collision).await.0, StatusCode::CONFLICT);
}

/// A stale conversation revision is a retained refusal: its operation looks
/// up the refusal, and correcting the revision does not reuse the operation.
async fn stale_revision_is_a_retained_refusal(app: &Router, id: Uuid) {
    let path = messages_path(id);
    let mut stale = create("Retained draft after stale definition");
    stale["expected_conversation_revision"] = json!("2");
    let (c, stale_reply) = post(app, &path, &stale).await;
    assert_eq!(c, StatusCode::CONFLICT, "{stale_reply}");
    schema(&stale_reply);
    assert_eq!(stale_reply["reason"], "conversation_revision_changed");
    let lookup = operation_path(id, &stale["operation_id"]);
    assert_eq!(get(app, &lookup).await.1, stale_reply);
    stale["expected_conversation_revision"] = json!("1");
    assert_eq!(post(app, &path, &stale).await.0, StatusCode::CONFLICT);
}

/// Global enqueue cannot reuse an operation already owned by another AQ-11 command.
async fn operation_of_another_command_is_refused(f: &Fixture, pool: &PgPool, id: Uuid) {
    let other_domain = create("Cross-domain operation collision");
    let sender_key = aq_agent_api::digest(&json!({"kind":"owner","user_id":f.owner})).unwrap();
    let other_op: Uuid = other_domain["operation_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    sqlx::query("INSERT INTO mailbox_operations(sender_key,operation_id,payload_digest,response) VALUES($1,$2,$3,'{\"api_version\":1,\"disposition\":\"queued\"}')")
        .bind(sender_key).bind(other_op).bind("a".repeat(64)).execute(pool).await.unwrap();
    assert_eq!(
        post(&f.app, &messages_path(id), &other_domain).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        get(&f.app, &operation_path(id, &other_domain["operation_id"]))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

async fn history_lists_both_messages(f: &Fixture, id: Uuid, first: &Value) -> Value {
    let (_, history) = get(&f.app, &messages_path(id)).await;
    schema(&history);
    assert_eq!(history["items"].as_array().unwrap().len(), 2);
    let oldest = &history["items"][0];
    assert_eq!(oldest["body"], first["body"]);
    assert_eq!(oldest["message_id"], first["message_id"]);
    assert_eq!(oldest["thread_id"], json!(id));
    assert_eq!(oldest["author"]["kind"], "human");
    assert_eq!(oldest["author"]["user_id"], json!(f.owner));
    history
}

/// Another owner reads the one global mailbox, but not this owner's receipts.
async fn another_owner_shares_history_but_not_receipts(
    f: &Fixture,
    pool: &PgPool,
    id: Uuid,
    first: &Value,
    history: &Value,
) {
    insert_user(pool, "other@localhost", "owner").await;
    let other = |path: String| async move { request(&f.app, &path, None, "other@localhost").await };
    assert_eq!(&other(messages_path(id)).await.1, history);
    assert_eq!(
        other(operation_path(id, &first["operation_id"])).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn max_body_pages_trim_to_byte_budget_and_preserve_both_cursor_directions() {
    let f = fixture().await;
    let id = discovery(&f).await;
    let path = messages_path(id);
    let body = format!("{}x", "\u{1}".repeat(16383));
    for _ in 0..5 {
        let (c, v) = post(&f.app, &path, &create(&body)).await;
        assert_eq!(c, StatusCode::OK, "{v}");
    }
    let app = &f.app;
    let page = |query: &str| {
        let uri = format!("{path}{query}");
        async move { get(app, &uri).await }
    };
    let (_, newest) = page("").await;
    schema(&newest);
    assert!(serde_json::to_vec(&newest).unwrap().len() <= 262144);
    assert_eq!(newest["items"].as_array().unwrap().len(), 2);
    assert_eq!(newest["items"][0]["cursor"], "4");
    assert_eq!(newest["items"][1]["cursor"], "5");
    assert_eq!(newest["next_cursor"], "4");
    assert_eq!(newest["has_more"], true);
    let (_, older) = page("?before=4").await;
    assert_eq!(older["items"][0]["cursor"], "2");
    assert_eq!(older["items"][1]["cursor"], "3");
    assert_eq!(older["next_cursor"], "2");
    let (_, oldest) = page("?before=2").await;
    assert_eq!(oldest["items"][0]["cursor"], "1");
    assert_eq!(oldest["has_more"], false);
    let (_, forward) = page("?after=0").await;
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
        assert_eq!(page(&format!("?{query}")).await.0, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn owner_current_identity_and_input_bounds_refuse_without_mail_or_worker_fallback() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = messages_path(id);
    let viewer = insert_user(&pool, "viewer@localhost", "viewer").await;
    grant_session_inspection(&pool, viewer).await;
    viewer_with_session_access_is_forbidden(&f.app, id).await;
    unsupported_inputs_are_bad_requests(&f.app, &path, viewer).await;
    assert_eq!(
        post_without_csrf(&f.app, &path, &create("No CSRF")).await,
        StatusCode::FORBIDDEN
    );
    role_revoked_during_lock_wait_is_forbidden(&f, &pool, &path).await;
    assert_eq!(count(&pool, "mailbox_messages").await, 0);
}

/// Session inspection rights do not stand in for the owner role.
async fn grant_session_inspection(pool: &PgPool, viewer: Uuid) {
    let session = aq_session_protocol::SessionIdentity::Remote {
        node_id: Uuid::new_v4(),
        boot_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        session_id: Uuid::new_v4(),
        assignment_generation: 1,
    };
    let key = aq_server::session_access::session_key(&session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,2000000)").bind(key).bind(json!(session)).bind(viewer).execute(pool).await.unwrap();
}

async fn viewer_with_session_access_is_forbidden(app: &Router, id: Uuid) {
    let path = messages_path(id);
    for uri in [
        "/api/human/conversations/global".to_string(),
        path.clone(),
        operation_path(id, &json!(Uuid::new_v4())),
    ] {
        assert_eq!(
            request(app, &uri, None, "viewer@localhost").await.0,
            StatusCode::FORBIDDEN
        );
    }
    let denied = create("Denied").to_string();
    assert_eq!(
        request(app, &path, Some(&denied), "viewer@localhost")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

async fn unsupported_inputs_are_bad_requests(app: &Router, path: &str, viewer: Uuid) {
    let mut refused = Vec::new();
    for field in ["context", "attachments"] {
        let mut bad = create("No unsupported references");
        bad[field] = json!([{"id":Uuid::new_v4()}]);
        refused.push(bad);
    }
    for body in [" ", "\0", &"x".repeat(16385)] {
        refused.push(create(body));
    }
    let mut equal_ids = create("Operation and message are separate identities");
    equal_ids["message_id"] = equal_ids["operation_id"].clone();
    refused.push(equal_ids);
    let mut caller_author = create("No caller author");
    caller_author["author"] = json!(viewer);
    refused.push(caller_author);
    for bad in &refused {
        assert_eq!(
            post(app, path, bad).await.0,
            StatusCode::BAD_REQUEST,
            "{bad}"
        );
    }
}

/// The request resolves its Owner before waiting on this actual role-row lock.
async fn role_revoked_during_lock_wait_is_forbidden(f: &Fixture, pool: &PgPool, path: &str) {
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(f.owner)
        .execute(&mut *held)
        .await
        .unwrap();
    let app = f.app.clone();
    let path = path.to_owned();
    let pending =
        tokio::spawn(async move { post(&app, &path, &create("Revoked while waiting")).await });
    assert!(
        conversation_waits_on_lock(pool).await,
        "observe actual authorization lock wait"
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.owner)
        .execute(&mut *held)
        .await
        .unwrap();
    held.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().0, StatusCode::FORBIDDEN);
}

/// Polls (bounded) until a conversation request waits on a row lock.
async fn conversation_waits_on_lock(pool: &PgPool) -> bool {
    for _ in 0..100 {
        let waiting = sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-supervisor-conversation' AND wait_event_type='Lock')").fetch_one(pool).await.unwrap();
        if waiting {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}

#[tokio::test]
async fn finite_retention_preserves_original_replay_and_lookup_at_capacity() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = messages_path(id);
    let first = create("Retained at capacity");
    let (_, old) = post(&f.app, &path, &first).await;
    sqlx::query("UPDATE global_supervisor_conversation SET retained_operations=4096,retained_bytes=67108864 WHERE singleton").execute(&pool).await.unwrap();
    assert_eq!(
        post(&f.app, &path, &create("New refused at capacity"))
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(post(&f.app, &path, &first).await.1, old);
    let lookup = operation_path(id, &first["operation_id"]);
    assert_eq!(get(&f.app, &lookup).await.1, old);
    let (_, page) = get(&f.app, &path).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn dropped_http_reply_recovers_byte_identical_receipt_and_one_committed_message() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let id = discovery(&f).await;
    let path = messages_path(id);
    let submission = create("The caller loses this reply after the mailbox commits.");
    let raw = submission.to_string();
    let response = f
        .app
        .clone()
        .oneshot(
            Request::post(&path)
                .header("x-aq-dev-email", OWNER)
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
    let lookup = Request::get(operation_path(id, &submission["operation_id"]))
        .header("x-aq-dev-email", OWNER)
        .body(Body::empty())
        .unwrap();
    let (_, get_bytes) = send(&f.app, lookup).await;
    let replay = Request::post(path)
        .header("x-aq-dev-email", OWNER)
        .header("x-aq-request", "1")
        .body(Body::from(raw))
        .unwrap();
    let (_, replay_bytes) = send(&f.app, replay).await;
    assert_eq!(
        get_bytes, replay_bytes,
        "receipt recovery must preserve exact wire bytes"
    );
    let record: Value = serde_json::from_slice(&get_bytes).unwrap();
    schema(&record);
    let sender = json!({"kind":"owner","user_id":f.owner});
    let private_digest = aq_agent_api::digest(&json!({"kind":"global_supervisor_send","conversation_id":id,"request":submission,"sender":sender})).unwrap();
    let message_id: Uuid = submission["message_id"].as_str().unwrap().parse().unwrap();
    let stored_digest: String =
        sqlx::query_scalar("SELECT payload_digest FROM mailbox_messages WHERE message_id=$1")
            .bind(message_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_digest, private_digest);
    assert_eq!(count(&pool, "mailbox_messages").await, 1);
}

#[tokio::test]
async fn actual_mount_has_owner_caps_and_csrf_while_standalone_and_viewer_do_not() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    insert_user(&pool, "mounted-viewer@localhost", "viewer").await;
    let mounted = aq_server::build_admin_router(f.state.clone());
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    for (app, email, expected) in [
        (&mounted, OWNER, true),
        (&mounted, "mounted-viewer@localhost", false),
        (&standalone, OWNER, false),
    ] {
        bootstrap_reports_supervisor(app, email, expected).await;
    }
    let (status, descriptor) = get(&mounted, "/api/human/conversations/global").await;
    assert_eq!(status, StatusCode::OK, "{descriptor}");
    let id: Uuid = descriptor["conversation"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        post_without_csrf(&mounted, &messages_path(id), &create("CSRF blocked")).await,
        StatusCode::FORBIDDEN
    );
}

/// The bootstrap enables the supervisor capabilities only when `enabled`.
async fn bootstrap_reports_supervisor(app: &Router, email: &str, enabled: bool) {
    let (status, bootstrap) = request(app, "/api/v1/bootstrap", None, email).await;
    assert_eq!(status, StatusCode::OK, "{bootstrap}");
    let caps = bootstrap["data"]["capabilities"].as_array().unwrap();
    let cap = |name: &str| caps.iter().find(|c| c["name"] == name).unwrap();
    for name in ["supervisor.history.read", "supervisor.message.enqueue"] {
        assert_eq!(cap(name)["version"], "1");
        assert_eq!(cap(name)["enabled"], enabled);
    }
    assert_eq!(cap("supervisor.conversation")["enabled"], false);
    // X4: a mounted owner sees the provisioned conversation, not yet live.
    let global = &bootstrap["data"]["global_supervisor"];
    let status = if enabled {
        "unavailable"
    } else {
        "unconfigured"
    };
    assert_eq!(global["status"], status, "{global}");
    assert_eq!(global["conversation_id"].is_string(), enabled, "{global}");
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
