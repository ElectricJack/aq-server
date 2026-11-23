//! Seed the granted execution attempt used by the access integration fixtures.
use aq_server::{
    AppState,
    agent_access::Runtime,
    config::Config,
    identity::verifier_from_config,
    store::{NewNode, Store},
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
pub(crate) async fn send(
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
pub(crate) async fn enroll(store: &Store) -> (Uuid, String) {
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
    (node.id, node_token)
}
pub(crate) fn state(
    store: Store,
    runtime: Runtime,
) -> (AppState, Arc<std::sync::atomic::AtomicI64>) {
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
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock_time = time.clone();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(move || {
            Utc.timestamp_millis_opt(clock_time.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap()
        }),
        agent_access: Some(Arc::new(runtime)),
        source_retention: None,
    };
    (state, time)
}
pub(crate) struct Launch {
    pub(crate) binding: Value,
    profile: Value,
    context: Value,
    identity: Value,
    attempt: Value,
}
impl Launch {
    pub(crate) fn new(
        node: Uuid,
        attempt: Option<Uuid>,
        edit: impl FnOnce(&mut Value, &mut Value),
    ) -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let mut profile = fixture["context"]["profile"].clone();
        let mut context = fixture["context"]["context"].clone();
        let original_context = context.clone();
        edit(&mut profile, &mut context);
        let mut binding = fixture["context"]["binding"].clone();
        if let Some(attempt) = attempt {
            binding["attempt_id"] = json!(attempt);
        }
        binding["node_id"] = json!(node);
        binding["profile_digest"] = json!(tokens::verifier(&profile.to_string()));
        if context != original_context {
            binding["context_digest"] = json!(tokens::verifier(&context.to_string()));
        }
        let boot = binding["boot_id"].clone();
        let identity = json!({"node_id":node,"boot_id":boot});
        let attempt = json!({"attempt_id":binding["attempt_id"],"session_id":binding["session_id"],"generation":1});
        Self {
            binding,
            profile,
            context,
            identity,
            attempt,
        }
    }
    pub(crate) async fn install(&self, pool: &sqlx::PgPool, node: Uuid) {
        let Self {
            binding,
            profile,
            identity,
            attempt,
            ..
        } = self;
        let spec = json!({"project_id":binding["project_id"],"harness":"deterministic-agent-v1","resolved_spec_digest":binding["resolved_spec_digest"],"fixture_output":"done","resources":{"slots":1,"cpu_millis":100,"memory_bytes":1024,"disk_bytes":1024},"required_capabilities":["deterministic-agent-v1","scoped-agent-access-v1"],"bindings":{"source":{"repository_id":Uuid::new_v4(),"binding_generation":1,"logical_identity":"test","base":{"algorithm":"sha1","hex":"1111111111111111111111111111111111111111"}},"graph":{"workspace_id":binding["workspace_id"],"profile_id":profile["profile_id"],"profile_revision":profile["revision"],"profile_digest":binding["profile_digest"],"context_digest":binding["context_digest"]}}});
        let offer_id = Uuid::new_v4();
        let attempt_id: Uuid = binding["attempt_id"].as_str().unwrap().parse().unwrap();
        let task_id: Uuid = binding["task_id"].as_str().unwrap().parse().unwrap();
        let session_id: Uuid = binding["session_id"].as_str().unwrap().parse().unwrap();
        let execution = json!({"task_id":task_id,"offer":{"offer_id":offer_id,"identity":identity,"attempt":attempt,"spec":spec,"offer_expires_at_ms":2_000_000,"reservation_ttl_ms":10000},"status":"granted","grant":null,"grant_expires_at_ms":1_100_000,"last_observation":null,"report":null,"disposition":null,"cancellation_id":null,"cancellation_reason":null,"stopped":false});
        sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2)")
            .bind(node)
            .bind(json!({"identity":identity}))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state) VALUES($1,$2,$3,$4,$5,$6)").bind(attempt_id).bind(task_id).bind(node).bind(offer_id).bind(session_id).bind(execution).execute(pool).await.unwrap();
    }
    pub(crate) async fn provision(&self, admin: &Router, grants: Option<Value>) {
        let attempt_id = &self.binding["attempt_id"];
        let Self {
            profile, context, ..
        } = self;
        assert_eq!(
            send(
                admin,
                "/api/agent-access/provision",
                None,
                Some({
                    let mut body =
                        json!({"attempt_id":attempt_id,"profile":profile,"context":context});
                    if let Some(grants) = grants {
                        body["grants"] = grants;
                    }
                    body
                })
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    pub(crate) async fn issue(&self, app: &Router, node_token: &str) -> String {
        let Self {
            identity, attempt, ..
        } = self;
        let (status, issued) = send(
            app,
            "/v2/worker/agent-session",
            Some(node_token),
            Some(json!({"identity":identity,"attempt":attempt,"expected_credential_version":0})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{issued}");
        issued["token"].as_str().unwrap().into()
    }
}
