//! AQ24 registered migration against isolated PostgreSQL and fake HTTP peers.
use aq_lifecycle_api::Scope;
use aq_notifications::{RetryPolicy, Route};
use aq_server::{
    notifications::{
        Error,
        config::Loaded,
        persistence::{RouteRequest, SendRequest, Service},
    },
    test_support::TestDb,
};
use chrono::{TimeZone, Utc};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use uuid::Uuid;
struct Env {
    _db: TestDb,
    service: Service,
    owner: Uuid,
    recipient: Uuid,
    scope: Scope,
    time: Arc<AtomicI64>,
}
impl Env {
    async fn new(configured: bool) -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
        let owner = store
            .bootstrap_owner(
                "notify-owner@localhost",
                "dev",
                "notify-owner@localhost",
                Utc.timestamp_millis_opt(1000).unwrap(),
            )
            .await
            .unwrap()
            .unwrap()
            .id;
        let recipient = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,email,role,created_at) VALUES($1,'recipient@localhost','viewer',now())").bind(recipient).execute(&pool).await.unwrap();
        let scope = Scope {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
        };
        sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')")
            .bind(scope.project_id).bind(json!({"enabled":true,"graph_workspace_id":scope.workspace_id})).execute(&pool).await.unwrap();
        let time = Arc::new(AtomicI64::new(1000));
        let t = time.clone();
        let service = Service {
            pool,
            configuration: configured.then(|| {
                Arc::new(Loaded::fixture(
                    1,
                    "discord-test",
                    1,
                    "http://127.0.0.1:1/api/v10/webhooks/42/fixture_token_1234567890",
                ))
            }),
            public_origin: "https://aq.example".into(),
            clock: Arc::new(move || Utc.timestamp_millis_opt(t.load(Ordering::SeqCst)).unwrap()),
        };
        service.install(Uuid::new_v4()).await.unwrap();
        Self {
            _db: db,
            service,
            owner,
            recipient,
            scope,
            time,
        }
    }
    fn route(&self) -> RouteRequest {
        RouteRequest {
            operation_id: Uuid::new_v4(),
            expected_revision: 0,
            route: Route {
                route_id: Uuid::new_v4(),
                revision: 1,
                scope: self.scope.clone(),
                recipient_user_id: self.recipient,
                endpoint_key: "discord-test".into(),
                endpoint_revision: 1,
                enabled: true,
                retry: RetryPolicy {
                    max_attempts: 3,
                    max_elapsed_ms: 30000,
                    initial_delay_ms: 250,
                    max_delay_ms: 2000,
                    allow_duplicate_risk: false,
                },
            },
        }
    }
    fn message(&self) -> SendRequest {
        SendRequest {
            operation_id: Uuid::new_v4(),
            message_id: Uuid::new_v4(),
            revision: 1,
            scope: self.scope.clone(),
            recipient_user_id: self.recipient,
            summary: "Review the bounded summary".into(),
            ttl_ms: 60_000,
        }
    }
    async fn counts(&self) -> (i64, i64, i64) {
        sqlx::query_as("SELECT message_count,delivery_count,operation_count FROM notification_budget WHERE singleton").fetch_one(&self.service.pool).await.unwrap()
    }
}
#[tokio::test]
async fn absent_route_preserves_dashboard_message_and_identical_logical_replay_never_sends_later() {
    let e = Env::new(false).await;
    let request = e.message();
    let result = e.service.enqueue(e.owner, request.clone()).await.unwrap();
    assert_eq!(result["external_state"], "no_external_route");
    assert_eq!(e.counts().await, (1, 0, 1));
    e.time.store(2000, Ordering::SeqCst);
    assert_eq!(
        e.service.enqueue(e.owner, request.clone()).await.unwrap(),
        result
    );
    assert_eq!(e.counts().await, (1, 0, 1));
    let mut replay = request.clone();
    replay.operation_id = Uuid::new_v4();
    assert_eq!(e.service.enqueue(e.owner, replay).await.unwrap(), result);
    assert_eq!(e.counts().await, (1, 0, 2));
    let inbox = e
        .service
        .inbox(e.recipient, e.recipient, (0, 1))
        .await
        .unwrap();
    assert_eq!(inbox.items.len(), 1);
    assert_eq!(inbox.items[0]["message"]["summary"], request.summary);
    let mut changed = request;
    changed.summary.push('!');
    assert_eq!(
        e.service.enqueue(e.owner, changed).await,
        Err(Error::Conflict)
    );
}
#[tokio::test]
async fn atomic_enqueue_route_cas_and_scope_rechecks_preserve_delivery_identity() {
    let e = Env::new(true).await;
    let route = e.route();
    let original = e.service.set_route(e.owner, route.clone()).await.unwrap();
    assert_eq!(
        e.service.set_route(e.owner, route.clone()).await.unwrap(),
        original
    );
    let request = e.message();
    let result = e.service.enqueue(e.owner, request.clone()).await.unwrap();
    assert_eq!(result["delivery_ids"].as_array().unwrap().len(), 1);
    assert_eq!(e.counts().await, (1, 1, 2));
    let view = e
        .service
        .inspect(e.recipient, request.message_id)
        .await
        .unwrap();
    assert_eq!(view["deliveries"][0]["status"]["kind"], "queued");
    assert_eq!(
        view["deliveries"][0]["human_acknowledgment"],
        "unacknowledged"
    );
    assert!(!view.to_string().contains("fixture_token"));
    let mut stale = route.clone();
    stale.operation_id = Uuid::new_v4();
    assert_eq!(
        e.service.set_route(e.owner, stale).await,
        Err(Error::Conflict)
    );
    let mut changed = route;
    changed.operation_id = Uuid::new_v4();
    changed.expected_revision = 1;
    changed.route.revision = 2;
    changed.route.enabled = false;
    e.service.set_route(e.owner, changed).await.unwrap();
    assert_eq!(e.service.enqueue(e.owner, request).await.unwrap(), result);
    assert_eq!(
        e.service.enqueue(e.owner, e.message()).await.unwrap()["external_state"],
        "no_external_route"
    );
    assert_eq!(
        e.service.set_route(e.recipient, e.route()).await,
        Err(Error::Forbidden)
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(e.owner)
        .execute(&e.service.pool)
        .await
        .unwrap();
    assert_eq!(
        e.service.enqueue(e.owner, e.message()).await,
        Err(Error::Forbidden)
    );
}
#[tokio::test]
async fn quota_refusal_rolls_back_all_message_intent_and_receipt_rows() {
    let e = Env::new(true).await;
    e.service.set_route(e.owner, e.route()).await.unwrap();
    sqlx::query("UPDATE notification_budget SET reserved_bytes=134217728")
        .execute(&e.service.pool)
        .await
        .unwrap();
    let request = e.message();
    assert_eq!(
        e.service.enqueue(e.owner, request.clone()).await,
        Err(Error::Overloaded)
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_messages WHERE message_id=$1")
            .bind(request.message_id)
            .fetch_one(&e.service.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert_eq!(e.counts().await, (0, 0, 1));
}
#[tokio::test]
async fn config_rotation_is_monotonic_and_missing_destinations_do_not_break_inspection() {
    let e = Env::new(true).await;
    e.service.set_route(e.owner, e.route()).await.unwrap();
    let request = e.message();
    e.service.enqueue(e.owner, request.clone()).await.unwrap();
    let mut changed = e.service.clone();
    changed.configuration = Some(Arc::new(Loaded::fixture(
        1,
        "discord-test",
        1,
        "http://127.0.0.1:1/api/v10/webhooks/42/changed_token_1234567890",
    )));
    assert_eq!(changed.install(Uuid::new_v4()).await, Err(Error::Conflict));
    changed.configuration = None;
    changed.install(Uuid::new_v4()).await.unwrap();
    assert!(
        changed
            .inspect(e.recipient, request.message_id)
            .await
            .is_ok()
    );
    assert_eq!(
        changed.enqueue(e.owner, e.message()).await.unwrap()["external_state"],
        "no_external_route"
    );
}
#[tokio::test]
async fn simultaneous_duplicate_enqueue_and_project_revocation_are_bounded() {
    let e = Env::new(true).await;
    e.service.set_route(e.owner, e.route()).await.unwrap();
    let request = e.message();
    let (a, b) = tokio::join!(
        e.service.enqueue(e.owner, request.clone()),
        e.service.enqueue(e.owner, request.clone())
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(e.counts().await, (1, 1, 2));
    sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false') WHERE project_id=$1").bind(e.scope.project_id).execute(&e.service.pool).await.unwrap();
    assert_eq!(
        e.service.inspect(e.recipient, request.message_id).await,
        Err(Error::NotFound)
    );
    assert!(
        e.service
            .inbox(e.recipient, e.recipient, (0, 64))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        e.service.enqueue(e.owner, request).await,
        Err(Error::NotFound)
    );
}

struct Peer {
    url: String,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    seen: Arc<tokio::sync::Notify>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Peer {
    async fn new(responses: Vec<(u16, &'static str)>) -> Self {
        use axum::{Router, response::IntoResponse, routing::post};
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::new(tokio::sync::Notify::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let c = calls.clone();
        let n = seen.clone();
        let responses = Arc::new(responses);
        let app = Router::new().route(
            "/api/v10/webhooks/{id}/{token}",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let c = c.clone();
                let n = n.clone();
                let responses = responses.clone();
                async move {
                    assert_eq!(body["allowed_mentions"]["parse"], json!([]));
                    assert_eq!(body["flags"], 4);
                    let i = c.fetch_add(1, Ordering::SeqCst);
                    n.notify_one();
                    let (code, body) = responses[i.min(responses.len() - 1)];
                    if code == 0 {
                        std::future::pending::<()>().await;
                    }
                    (axum::http::StatusCode::from_u16(code).unwrap(), body).into_response()
                }
            }),
        );
        let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://127.0.0.1:{port}/api/v10/webhooks/42/fixture_token_1234567890"),
            calls,
            seen,
            handle,
        }
    }
    async fn configure(&self, e: &mut Env) -> Uuid {
        e.service.configuration = Some(Arc::new(Loaded::fixture(2, "discord-test", 2, &self.url)));
        let id = Uuid::new_v4();
        e.service.install(id).await.unwrap();
        id
    }
}
async fn routed(e: &Env, duplicate_risk: bool) -> Uuid {
    let mut r = e.route();
    r.route.endpoint_revision = 2;
    r.route.retry.allow_duplicate_risk = duplicate_risk;
    e.service.set_route(e.owner, r).await.unwrap();
    let m = e.message();
    let id = m.message_id;
    e.service.enqueue(e.owner, m).await.unwrap();
    id
}
#[tokio::test]
async fn transport_acceptance_is_neither_human_ack_nor_decision_and_no_tick_replays_it() {
    let peer = Peer::new(vec![(200, r#"{"id":"99","webhook_id":"42"}"#)]).await;
    let mut e = Env::new(false).await;
    let id = peer.configure(&mut e).await;
    let message = routed(&e, false).await;
    let client = aq_notifications::discord::Discord::new().unwrap();
    assert!(e.service.tick(id, &client).await.unwrap());
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    let d = &view["deliveries"][0];
    assert_eq!(d["status"]["kind"], "accepted_by_transport");
    assert_eq!(d["human_acknowledgment"], "unacknowledged");
    assert_eq!(d["decision"], "not_applicable");
    for _ in 0..3 {
        assert!(!e.service.tick(id, &client).await.unwrap());
    }
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn interrupted_dispatch_is_unknown_after_takeover_and_explicit_retry_is_idempotent() {
    let peer = Peer::new(vec![(0, ""), (200, r#"{"id":"100","webhook_id":"42"}"#)]).await;
    let mut e = Env::new(false).await;
    let first = peer.configure(&mut e).await;
    let message = routed(&e, true).await;
    let service = e.service.clone();
    let task = tokio::spawn(async move {
        service
            .tick(first, &aq_notifications::discord::Discord::new().unwrap())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), peer.seen.notified())
        .await
        .unwrap();
    task.abort();
    let _ = task.await;
    let second = Uuid::new_v4();
    e.service.install(second).await.unwrap();
    let client = aq_notifications::discord::Discord::new().unwrap();
    assert_eq!(
        e.service.tick(first, &client).await,
        Err(Error::DispatcherFenced)
    );
    assert!(!e.service.tick(second, &client).await.unwrap());
    e.time.store(21001, Ordering::SeqCst);
    assert!(!e.service.tick(second, &client).await.unwrap());
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    let d = &view["deliveries"][0];
    assert_eq!(d["status"]["kind"], "delivery_unknown");
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    let request = aq_server::notifications::persistence::RetryRequest {
        operation_id: Uuid::new_v4(),
        delivery_id: serde_json::from_value(d["delivery_id"].clone()).unwrap(),
        expected_revision: d["record_revision"].as_u64().unwrap(),
        accept_duplicate_risk: true,
    };
    let response = e
        .service
        .retry_unknown(e.owner, request.clone())
        .await
        .unwrap();
    assert_eq!(
        e.service.retry_unknown(e.owner, request).await.unwrap(),
        response
    );
    assert!(e.service.tick(second, &client).await.unwrap());
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    assert_eq!(view["deliveries"][0]["attempts"], 2);
    assert_eq!(view["deliveries"][0]["duplicate_risk_retries"], 1);
    assert_eq!(peer.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn global_rate_limit_is_durable_across_dispatcher_restart() {
    let peer = Peer::new(vec![
        (429, r#"{"retry_after":2.5,"global":true}"#),
        (200, r#"{"id":"101","webhook_id":"42"}"#),
    ])
    .await;
    let mut e = Env::new(false).await;
    let first = peer.configure(&mut e).await;
    let message = routed(&e, false).await;
    let client = aq_notifications::discord::Discord::new().unwrap();
    assert!(e.service.tick(first, &client).await.unwrap());
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    assert_eq!(view["deliveries"][0]["status"]["next_attempt_ms"], 3500);
    let second = Uuid::new_v4();
    e.service.install(second).await.unwrap();
    e.time.store(3499, Ordering::SeqCst);
    assert!(!e.service.tick(second, &client).await.unwrap());
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    e.time.store(3500, Ordering::SeqCst);
    for _ in 0..2 {
        e.service.tick(second, &client).await.unwrap();
    }
    assert_eq!(peer.calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn unavailable_destination_stops_future_sends_and_route_revocation_suppresses_queued_work() {
    let peer = Peer::new(vec![(404, "{}")]).await;
    let mut e = Env::new(false).await;
    let id = peer.configure(&mut e).await;
    let first = routed(&e, false).await;
    let second = e.message();
    e.service.enqueue(e.owner, second.clone()).await.unwrap();
    let client = aq_notifications::discord::Discord::new().unwrap();
    e.service.tick(id, &client).await.unwrap();
    e.service.tick(id, &client).await.unwrap();
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
    for m in [first, second.message_id] {
        assert_eq!(
            e.service.inspect(e.recipient, m).await.unwrap()["deliveries"][0]["status"]["reason"],
            "destination_unavailable"
        );
    }
    assert_eq!(
        e.service.enqueue(e.owner, e.message()).await.unwrap()["external_state"],
        "no_external_route"
    );
    // A separate destination revision clears the disabled marker explicitly.
    e.service.configuration = Some(Arc::new(Loaded::fixture(3, "discord-test", 3, &peer.url)));
    let id = Uuid::new_v4();
    e.service.install(id).await.unwrap();
    let mut r = e.route();
    r.route.endpoint_revision = 3;
    e.service.set_route(e.owner, r.clone()).await.unwrap();
    let m = e.message();
    e.service.enqueue(e.owner, m.clone()).await.unwrap();
    r.operation_id = Uuid::new_v4();
    r.expected_revision = 1;
    r.route.revision = 2;
    r.route.enabled = false;
    e.service.set_route(e.owner, r).await.unwrap();
    for _ in 0..2 {
        e.service.tick(id, &client).await.unwrap();
    }
    assert_eq!(
        e.service.inspect(e.recipient, m.message_id).await.unwrap()["deliveries"][0]["status"]["reason"],
        "route_changed"
    );
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
}

impl Env {
    async fn state(&self) -> aq_server::AppState {
        let config: aq_server::config::Config = toml::from_str(
            r#"
format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:8080"
worker_bind="127.0.0.1:8081"
public_origin="https://aq.example"
[auth]
mode="dev"
dev_email="notify-owner@localhost"
[bootstrap]
enabled=false
"#,
        )
        .unwrap();
        aq_server::AppState {
            internal_work: Default::default(),
            agent_access: None,
            source_retention: None,
            store: self._db.store().await,
            config: Arc::new(config),
            identity: Arc::new(aq_server::identity::DevIdentity::new(
                "notify-owner@localhost".into(),
            )),
            clock: self.service.clock.clone(),
        }
    }
}
#[tokio::test]
async fn actual_human_routes_enforce_csrf_current_roles_scope_and_machine_separation() {
    let e = Env::new(false).await;
    let state = e.state().await;
    let app = aq_server::build_admin_router(state.clone());
    let request = e.message();
    http::writes(&app, &request).await;
    http::reads(app, state, e.owner, request.message_id).await;
    assert_eq!(
        aq_server::admission::classify("/api/notifications/retry"),
        aq_server::admission::Class::Notification
    );
}
#[tokio::test]
async fn notification_connections_do_not_consume_execution_control_pool() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let e = Env::new(false).await;
    let state = e.state().await;
    let service = aq_server::notifications::api::service(&state);
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(service.pool.acquire().await.unwrap());
    }
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        aq_server::build_worker_router(state).oneshot(
            Request::post("/v1/worker/probe")
                .header("content-type", "application/json")
                .header("authorization", "Bearer aqn_notification_isolation_fixture")
                .body(Body::from(r#"{"nonce":"notification-pool-check"}"#))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    drop(held);
}

#[tokio::test]
async fn destination_backoff_does_not_block_an_independent_destination() {
    let slow = Peer::new(vec![(429, r#"{"retry_after":20,"global":false}"#)]).await;
    let fast = Peer::new(vec![(200, r#"{"id":"1234","webhook_id":"42"}"#)]).await;
    let mut e = Env::new(false).await;
    let mut config = Loaded::fixture(1, "discord-test", 1, &slow.url);
    config
        .endpoints
        .extend(Loaded::fixture(1, "discord-fast", 1, &fast.url).endpoints);
    e.service.configuration = Some(Arc::new(config));
    let id = Uuid::new_v4();
    e.service.install(id).await.unwrap();
    let slow_route = e.route();
    let mut fast_route = e.route();
    fast_route.route.endpoint_key = "discord-fast".into();
    e.service.set_route(e.owner, slow_route).await.unwrap();
    e.service.set_route(e.owner, fast_route).await.unwrap();
    let m = e.message();
    e.service.enqueue(e.owner, m.clone()).await.unwrap();
    let client = aq_notifications::discord::Discord::new().unwrap();
    for _ in 0..4 {
        e.service.tick(id, &client).await.unwrap();
    }
    assert_eq!(slow.calls.load(Ordering::SeqCst), 1);
    assert_eq!(fast.calls.load(Ordering::SeqCst), 1);
    let view = e.service.inspect(e.recipient, m.message_id).await.unwrap();
    let kinds: Vec<_> = view["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["status"]["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"retry_scheduled"));
    assert!(kinds.contains(&"accepted_by_transport"));
}
#[tokio::test]
async fn missing_routes_work_with_development_http_origin() {
    let mut e = Env::new(false).await;
    e.service.public_origin = "http://127.0.0.1:8080".into();
    assert_eq!(
        e.service.enqueue(e.owner, e.message()).await.unwrap()["external_state"],
        "no_external_route"
    );
}

#[tokio::test]
async fn actual_server_startup_dispatches_and_shutdown_leaves_no_repeating_sender() {
    let peer = Peer::new(vec![(200, r#"{"id":"222","webhook_id":"42"}"#)]).await;
    let mut e = Env::new(false).await;
    peer.configure(&mut e).await;
    let message = routed(&e, false).await;
    let mut state = e.state().await;
    let mut config = (*state.config).clone();
    config.notifications = e.service.configuration.clone();
    state.config = Arc::new(config);
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (stop, wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(aq_server::run_with_listeners(state, admin, worker, async {
        let _ = wait.await;
    }));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let view = e.service.inspect(e.recipient, message).await.unwrap();
            if view["deliveries"][0]["status"]["kind"] == "accepted_by_transport" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(peer.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn late_provider_acceptance_cannot_be_written_by_a_fenced_dispatcher() {
    use axum::{Router, routing::post};
    let seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let a = seen.clone();
    let b = release.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/v10/webhooks/42/fixture_token_1234567890",
        listener.local_addr().unwrap()
    );
    let app = Router::new().route(
        "/api/v10/webhooks/{id}/{token}",
        post(move || {
            let a = a.clone();
            let b = b.clone();
            async move {
                a.notify_one();
                b.notified().await;
                axum::Json(json!({"id":"333","webhook_id":"42"}))
            }
        }),
    );
    let peer = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut e = Env::new(false).await;
    e.service.configuration = Some(Arc::new(Loaded::fixture(2, "discord-test", 2, &url)));
    let first = Uuid::new_v4();
    e.service.install(first).await.unwrap();
    let message = routed(&e, false).await;
    let service = e.service.clone();
    let task = tokio::spawn(async move {
        service
            .tick(first, &aq_notifications::discord::Discord::new().unwrap())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), seen.notified())
        .await
        .unwrap();
    let second = Uuid::new_v4();
    e.service.install(second).await.unwrap();
    release.notify_one();
    assert_eq!(task.await.unwrap(), Err(Error::DispatcherFenced));
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    assert_eq!(view["deliveries"][0]["status"]["kind"], "dispatching");
    e.time.store(21001, Ordering::SeqCst);
    e.service
        .tick(second, &aq_notifications::discord::Discord::new().unwrap())
        .await
        .unwrap();
    let view = e.service.inspect(e.recipient, message).await.unwrap();
    assert_eq!(view["deliveries"][0]["status"]["kind"], "delivery_unknown");
    peer.abort();
}

mod http;
