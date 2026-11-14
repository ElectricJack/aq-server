//! AQ-07 real PostgreSQL intent/outbox and bounded public HTTP graph adapter.
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
    fake: Arc<FakeGraph>,
    state: AppState,
    graph_origin: String,
    _graph_task: tokio::task::JoinHandle<()>,
}
async fn setup() -> Env {
    setup_scoped(false).await
}
async fn setup_scoped(structural: bool) -> Env {
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
        "../../../../protocol/agent-access/v1/fixtures/base.json"
    ))
    .unwrap();
    let mut profile = fixture["context"]["profile"].clone();
    profile["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("write.task_content"));
    if structural {
        for edge in ["blocks", "parent-child"] {
            profile["write_scope"]["relationship_types"]
                .as_array_mut()
                .unwrap()
                .push(json!(edge));
        }
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
    let fake = Arc::new(FakeGraph::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!("http://{}", listener.local_addr().unwrap());
    let service = Router::new().fallback(graph_http).with_state(fake.clone());
    let graph_task = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
    let runtime = Runtime::new(&AgentAccessConfig {
        mutation_actor_id: Some("api01-writer".into()),
        graph_url: local.clone(),
        graph_token_file: std::env::var_os("AQ_MUTATION_GRAPH_TOKEN_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or(token_file),
        credential_ttl_ms: 10000,
    })
    .await
    .unwrap();
    let time = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let clock_time = time.clone();
    let state = AppState {
        internal_work: Default::default(),
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
        fake,
        state,
        graph_origin: local,
        _graph_task: graph_task,
    }
}
use axum::{
    extract::{Request as AxumRequest, State},
    response::{IntoResponse, Response},
};
use std::collections::BTreeMap;
struct FakeData {
    nodes: BTreeMap<String, Value>,
    edges: BTreeMap<String, Value>,
    receipts: BTreeMap<String, (Value, Value)>,
    posts: Vec<Value>,
    applied: usize,
}
struct FakeGraph {
    upstream: Option<String>,
    data: tokio::sync::Mutex<FakeData>,
    lose_once: std::sync::atomic::AtomicBool,
    race_after_get: std::sync::atomic::AtomicBool,
    hold: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl FakeGraph {
    fn new() -> Self {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
        ))
        .unwrap();
        Self {
            upstream: std::env::var("AQ_MUTATION_GRAPH_URL").ok(),
            data: tokio::sync::Mutex::new(FakeData {
                nodes: serde_json::from_value(fixture["nodes"].clone()).unwrap(),
                edges: serde_json::from_value(fixture["edges"].clone()).unwrap(),
                receipts: BTreeMap::new(),
                posts: Vec::new(),
                applied: 0,
            }),
            lose_once: false.into(),
            race_after_get: false.into(),
            hold: false.into(),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}
fn graph_response(code: StatusCode, body: Value) -> Response {
    (code, [("X-AQ-Graph-API-Version", "1")], axum::Json(body)).into_response()
}
async fn graph_http(State(fake): State<Arc<FakeGraph>>, request: AxumRequest) -> Response {
    if let Some(origin) = &fake.upstream {
        return qualified_proxy(&fake, origin, request).await;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() < 5 || parts[1] != "v1" || parts[2] != "workspaces" {
        return graph_response(StatusCode::NOT_FOUND, json!({"error":{"code":"not_found"}}));
    }
    let mut data = fake.data.lock().await;
    if method == axum::http::Method::GET {
        if parts.len() == 6 && parts[4] == "nodes" {
            let observed = data.nodes.get(parts[5]).cloned();
            if parts[5] == uid(2)
                && fake
                    .race_after_get
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                let n = data.nodes.get_mut(parts[5]).unwrap();
                n["version"] = json!(n["version"].as_i64().unwrap() + 1);
                n["verification"] = json!("verified");
            }
            return match observed {
                Some(n) => graph_response(StatusCode::OK, n),
                None => {
                    graph_response(StatusCode::NOT_FOUND, json!({"error":{"code":"not_found"}}))
                }
            };
        }
        if parts.len() == 7 && parts[6] == "context" {
            let fixture: Value = serde_json::from_str(include_str!(
                "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
            ))
            .unwrap();
            let mut c = fixture["context"].clone();
            c["node"] = data.nodes.get(parts[5]).cloned().unwrap_or(Value::Null);
            c["edges"] = json!(data.edges.values().cloned().collect::<Vec<_>>());
            return graph_response(StatusCode::OK, c);
        }
    }
    if method != axum::http::Method::POST {
        return graph_response(
            StatusCode::BAD_REQUEST,
            json!({"error":{"code":"invalid_request"}}),
        );
    }
    let raw = axum::body::to_bytes(request.into_body(), 1048576)
        .await
        .unwrap();
    let payload: Value = serde_json::from_slice(&raw).unwrap();
    data.posts.push(payload.clone());
    let key = payload["idempotency_key"].as_str().unwrap().to_string();
    if let Some((old, response)) = data.receipts.get(&key) {
        return if old == &payload {
            graph_response(StatusCode::OK, response.clone())
        } else {
            graph_response(StatusCode::CONFLICT, json!({"error":{"code":"conflict"}}))
        };
    }
    drop(data);
    if fake.hold.load(std::sync::atomic::Ordering::SeqCst) {
        fake.arrived.notify_one();
        fake.release.notified().await;
    }
    let mut data = fake.data.lock().await;
    let mut nodes = data.nodes.clone();
    let mut edges = data.edges.clone();
    let mut results = Vec::new();
    for op in payload["operations"].as_array().unwrap() {
        let id = op["id"].as_str().unwrap().to_string();
        match op["op"].as_str().unwrap() {
            "update_node" => {
                let Some(node) = nodes.get_mut(&id) else {
                    return graph_response(
                        StatusCode::NOT_FOUND,
                        json!({"error":{"code":"not_found"}}),
                    );
                };
                if node["retired"] == true
                    || node["verification"] != "unverified"
                    || (node["kind"] == "knowledge" && node["created_by"] != "api01-writer")
                {
                    return graph_response(
                        StatusCode::FORBIDDEN,
                        json!({"error":{"code":"forbidden"}}),
                    );
                }
                if node["version"] != op["expected_version"] {
                    return graph_response(
                        StatusCode::CONFLICT,
                        json!({"error":{"code":"conflict"}}),
                    );
                }
                if !op["patch"].as_object().unwrap().is_empty() {
                    for (k, v) in op["patch"].as_object().unwrap() {
                        node[k] = v.clone();
                    }
                    node["version"] = json!(node["version"].as_i64().unwrap() + 1);
                    node["content_revision"] =
                        json!(node["content_revision"].as_i64().unwrap() + 1);
                }
                results.push(node.clone());
            }
            "create_node" => {
                if nodes.contains_key(&id) {
                    return graph_response(
                        StatusCode::CONFLICT,
                        json!({"error":{"code":"conflict"}}),
                    );
                }
                let prototype = if op["kind"] == "task" { uid(1) } else { uid(2) };
                let mut n = nodes[&prototype].clone();
                n["id"] = json!(id);
                n["version"] = json!(1);
                n["content_revision"] = json!(1);
                n["created_by"] = json!("api01-writer");
                for field in [
                    "title",
                    "body",
                    "labels",
                    "content_type",
                    "acceptance_criteria",
                    "task_state",
                    "provenance",
                ] {
                    if let Some(v) = op.get(field) {
                        n[field] = v.clone();
                    }
                }
                nodes.insert(id, n.clone());
                results.push(n);
            }
            "add_edge" => {
                if edges.contains_key(&id) {
                    return graph_response(
                        StatusCode::CONFLICT,
                        json!({"error":{"code":"conflict"}}),
                    );
                }
                let mut e = edges.values().next().unwrap().clone();
                e["id"] = json!(id);
                for field in [
                    "source",
                    "target",
                    "edge_type",
                    "description",
                    "source_revision",
                    "target_revision",
                ] {
                    e[field] = op.get(field).cloned().unwrap_or(Value::Null);
                }
                e["version"] = json!(1);
                e["deleted"] = json!(false);
                edges.insert(id, e.clone());
                if let Some(n) = nodes.get_mut(op["source"].as_str().unwrap()) {
                    n["version"] = json!(n["version"].as_i64().unwrap() + 1);
                    n["content_revision"] = json!(n["content_revision"].as_i64().unwrap() + 1);
                }
                results.push(e);
            }
            "remove_edge" => {
                let Some(e) = edges.get_mut(&id) else {
                    return graph_response(
                        StatusCode::NOT_FOUND,
                        json!({"error":{"code":"not_found"}}),
                    );
                };
                if e["version"] != op["expected_version"] {
                    return graph_response(
                        StatusCode::CONFLICT,
                        json!({"error":{"code":"conflict"}}),
                    );
                }
                e["deleted"] = json!(true);
                e["version"] = json!(e["version"].as_i64().unwrap() + 1);
                results.push(e.clone());
            }
            _ => {
                return graph_response(
                    StatusCode::BAD_REQUEST,
                    json!({"error":{"code":"invalid_request"}}),
                );
            }
        }
    }
    let response = if parts[4] == "changesets" {
        json!({"id":Uuid::new_v4(),"workspace_id":parts[3],"actor_id":"api01-writer","name":payload["name"],"reason":payload["reason"],"operations":payload["operations"],"status":"draft","created_at":"2026-10-30T00:00:00Z","applied_at":null,"result":null})
    } else {
        data.nodes = nodes;
        data.edges = edges;
        json!({"change_set_id":Uuid::new_v4(),"results":results})
    };
    data.applied += 1;
    data.receipts.insert(key, (payload, response.clone()));
    if fake
        .lose_once
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return graph_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":{"code":"internal"}}),
        );
    }
    graph_response(StatusCode::OK, response)
}
fn uid(n: u128) -> String {
    Uuid::from_u128(n).to_string()
}
fn command(e: &Env, commands: Value) -> Value {
    json!({"api_version":1,"operation_id":Uuid::new_v4(),"binding":e.binding,"reason":"Test authorized immutable intent","commands":commands})
}
fn correction(e: &Env) -> Value {
    command(
        e,
        json!([{"kind":"correct_record","id":uid(2),"expected_version":"2","patch":{"title":"Reviewed correction"}}]),
    )
}
fn recovery(e: &Env, op: &Value) -> Value {
    json!({"api_version":1,"request_id":Uuid::new_v4(),"node_id":e.binding["node_id"],"attempt":{"attempt_id":e.binding["attempt_id"],"session_id":e.binding["session_id"],"generation":e.binding["generation"]},"mode":"lookup","operation_ids":[op]})
}
async fn submit(e: &Env, r: Value) -> (StatusCode, Value) {
    send(&e.app, "/v1/agent/graph/commands", Some(&e.token), Some(r)).await
}
#[tokio::test]
async fn confirmed_outbox_replay_byte_conflict_and_truthful_version_conflict() {
    let e = setup().await;
    let r = correction(&e);
    let (status, receipt) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["versions"][0]["version"], "3");
    assert_eq!(receipt["receipt"]["task_completion"], false);
    assert_eq!(submit(&e, r.clone()).await.1, receipt);
    assert_eq!(e.fake.data.lock().await.applied, 1);
    let mut changed = r.clone();
    changed["commands"][0]["patch"]["title"] = json!("different");
    assert_eq!(submit(&e, changed).await.0, StatusCode::CONFLICT);
    let (status, conflict) = submit(&e, correction(&e)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["code"], "id_conflict");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
    let reopened = e.db.store().await;
    drop(reopened);
    assert_eq!(
        send(
            &e.app,
            &format!(
                "/v1/agent/operations/{}",
                r["operation_id"].as_str().unwrap()
            ),
            Some(&e.token),
            None
        )
        .await
        .1,
        receipt
    );
}
#[tokio::test]
async fn response_loss_recovery_after_expiry_and_revocation_replays_exact_intent() {
    let e = setup().await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let r = correction(&e);
    let (status, unknown) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{unknown}");
    assert_eq!(unknown["state"], "unknown");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(submit(&e, r.clone()).await.0, StatusCode::UNAUTHORIZED);
    let lookup = recovery(&e, &r["operation_id"]);
    let (status, result) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["operations"][0]["state"], "unknown");
    let (status, replayed) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["operations"][0]["state"], "confirmed");
    let data = e.fake.data.lock().await;
    assert_eq!(data.applied, 1);
    assert_eq!(data.posts.len(), 2);
    assert_eq!(data.posts[0], data.posts[1]);
    drop(data);
    let mut bad = lookup;
    bad["operation_ids"] = json!([Uuid::new_v4()]);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/agent-operations/retry",
            Some(&e.node_token),
            Some(bad)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
}
#[tokio::test]
async fn permissions_protected_authors_and_capabilities_refuse_before_admission() {
    let e = setup().await;
    let mut r = correction(&e);
    r["commands"][0]["id"] = json!(uid(4));
    assert_eq!(submit(&e, r).await.0, StatusCode::NOT_FOUND);
    {
        let mut data = e.fake.data.lock().await;
        data.nodes.get_mut(&uid(2)).unwrap()["created_by"] = json!("foreign");
    }
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    {
        let mut data = e.fake.data.lock().await;
        data.nodes.get_mut(&uid(2)).unwrap()["created_by"] = json!("api01-writer");
        data.nodes.get_mut(&uid(2)).unwrap()["verification"] = json!("verified");
    }
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    sqlx::query("UPDATE agent_launch_bindings SET profile=jsonb_set(profile,'{capabilities}','[\"read.get\"]')").execute(&e.pool).await.unwrap();
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(e.fake.data.lock().await.posts.len(), 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn relationship_guard_same_batch_and_findings_created_scope() {
    let e = setup().await;
    let edge = Uuid::new_v4();
    let r = command(
        &e,
        json!([{"kind":"add_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"2","edge_type":"references","source_revision":null,"target_revision":null,"description":"bounded link"}]),
    );
    let (status, body) = submit(&e, r.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = e.fake.data.lock().await;
    assert_eq!(
        data.posts[0]["operations"][0],
        json!({"op":"update_node","id":uid(2),"expected_version":2,"patch":{}})
    );
    assert_eq!(data.posts[0]["operations"][1]["op"], "add_edge");
    drop(data);
    let remove = command(
        &e,
        json!([{"kind":"remove_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"3","expected_version":"1"}]),
    );
    let (status, removed) = submit(&e, remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["receipt"]["versions"][0]["kind"], "edge");
    let finding = Uuid::new_v4();
    let r = command(
        &e,
        json!([{"kind":"create_finding","id":finding,"title":"Finding","body":"Attributed knowledge","labels":[],"content_type":"fact"}]),
    );
    let (status, body) = submit(&e, r).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let read = json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":e.binding,"query":{"kind":"get","node_id":finding}});
    let (status, body) = send(&e.app, "/v1/agent/graph/read", Some(&e.token), Some(read)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["node"]["id"], json!(finding));
}
#[tokio::test]
async fn future_guard_versions_and_concurrent_protection_changes_cannot_authorize_a_write() {
    let e = setup().await;
    let mut future = correction(&e);
    future["commands"][0]["expected_version"] = json!("3");
    assert_eq!(submit(&e, future).await.0, StatusCode::CONFLICT);
    assert!(e.fake.data.lock().await.posts.is_empty());
    let source_future = command(
        &e,
        json!([{"kind":"add_relationship","id":Uuid::new_v4(),"source":uid(2),"target":uid(3),"expected_source_version":"3","edge_type":"references","source_revision":null,"target_revision":null,"description":null}]),
    );
    assert_eq!(submit(&e, source_future).await.0, StatusCode::CONFLICT);
    let mut parent_future = proposal(&e);
    parent_future["commands"][0]["expected_parent_version"] = json!("4");
    assert_eq!(submit(&e, parent_future).await.0, StatusCode::CONFLICT);
    assert!(e.fake.data.lock().await.posts.is_empty());
    e.fake
        .race_after_get
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = submit(&e, correction(&e)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["state"], "rejected");
    let d = e.fake.data.lock().await;
    assert_eq!(d.applied, 0);
    assert_eq!(d.nodes[&uid(2)]["verification"], "verified");
    assert_eq!(d.posts[0]["operations"][0]["expected_version"], 2);
}
fn proposal(e: &Env) -> Value {
    command(
        e,
        json!([{"kind":"propose_decomposition","name":"Bounded draft","parent_id":uid(1),"expected_parent_version":"3","tasks":[{"id":Uuid::new_v4(),"title":"Proposed task","body":"Draft only","acceptance_criteria":"No direct apply"}],"relationships":[],"guards":[{"id":uid(3),"expected_version":"2"}]}]),
    )
}
#[tokio::test]
async fn structural_proposals_need_explicit_edge_scopes_and_never_apply() {
    for granted in [false, true] {
        let e = setup_scoped(granted).await;
        let mut r = proposal(&e);
        let task = r["commands"][0]["tasks"][0]["id"].clone();
        r["commands"][0]["relationships"] =
            json!([{"id":Uuid::new_v4(),"source":uid(1),"target":task,"edge_type":"parent-child"}]);
        let (status, body) = submit(&e, r.clone()).await;
        if granted {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["receipt"]["kind"], "graph_draft");
            assert_eq!(body["receipt"]["applied"], false);
            assert_eq!(e.fake.data.lock().await.nodes.len(), 6);
            assert_eq!(
                send(
                    &e.app,
                    "/v1/agent/graph/changesets/arbitrary/apply",
                    Some(&e.token),
                    Some(json!({}))
                )
                .await
                .0,
                StatusCode::NOT_FOUND
            );
            let mut future = r.clone();
            future["operation_id"] = json!(Uuid::new_v4());
            future["commands"][0]["guards"][0]["expected_version"] = json!("3");
            assert_eq!(submit(&e, future).await.0, StatusCode::CONFLICT);
        } else {
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(e.fake.data.lock().await.posts.is_empty());
        }
    }
}
#[tokio::test]
async fn admitted_dispatch_does_not_hold_authority_locks_and_cannot_be_cancelled_as_unsent() {
    let e = setup().await;
    e.fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(&e);
    let app = e.app.clone();
    let token = e.token.clone();
    let r = request.clone();
    let inflight =
        tokio::spawn(
            async move { send(&app, "/v1/agent/graph/commands", Some(&token), Some(r)).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(3), e.fake.arrived.notified())
        .await
        .unwrap();
    let (_, cancel) = send(
        &e.admin,
        &format!(
            "/api/agent-operations/{}/cancel",
            request["operation_id"].as_str().unwrap()
        ),
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(cancel["effect"], "possible");
    let revoke_path = format!(
        "/api/agent-access/{}/revoke",
        e.binding["attempt_id"].as_str().unwrap()
    );
    let revoke = send(&e.admin, &revoke_path, None, Some(json!({})));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), revoke)
            .await
            .expect("HTTP dispatch must not hold authority locks")
            .0,
        StatusCode::OK
    );
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::UNAUTHORIZED);
    e.fake.release.notify_one();
    let (status, result) = inflight.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["state"], "confirmed");
    assert_eq!(
        send(
            &e.admin,
            &format!(
                "/api/agent-operations/{}/retry",
                request["operation_id"].as_str().unwrap()
            ),
            None,
            Some(json!({}))
        )
        .await
        .1,
        result
    );
}

async fn qualified_proxy(fake: &FakeGraph, origin: &str, request: AxumRequest) -> Response {
    let method = request.method().clone();
    let path = request.uri().path_and_query().unwrap().as_str().to_string();
    let credential = request.headers().get("authorization").cloned().unwrap();
    let bytes = axum::body::to_bytes(request.into_body(), 1048576)
        .await
        .unwrap();
    if method == axum::http::Method::POST {
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        fake.data.lock().await.posts.push(value);
    }
    let response = reqwest::Client::new()
        .request(method.clone(), format!("{origin}{path}"))
        .header("authorization", credential)
        .header("X-AQ-Graph-API-Version", "1")
        .header("content-type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(response.headers()["X-AQ-Graph-API-Version"], "1");
    let body: Value = response.json().await.unwrap();
    if method == axum::http::Method::POST
        && status.is_success()
        && fake
            .lose_once
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return graph_response(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error":{"code":"internal"}}),
        );
    }
    graph_response(status, body)
}
#[tokio::test]
#[ignore = "requires scripts/test-agent-mutations.py isolated pinned graph image and ordinary credential"]
async fn actual_qualified_graph_delivery_recovers_lost_receipt_without_duplicate_history() {
    let origin = std::env::var("AQ_MUTATION_GRAPH_URL")
        .expect("isolated qualification wrapper supplies graph");
    let path =
        std::env::var("AQ_MUTATION_GRAPH_TOKEN_FILE").expect("isolated ordinary writer token file");
    let token = std::fs::read_to_string(path).unwrap();
    let mut e = setup_scoped(true).await;
    async fn history(origin: &str, token: &str) -> Value {
        reqwest::Client::new()
            .get(format!(
                "{origin}/v1/workspaces/{}/nodes/{}/history?limit=64",
                uid(105),
                uid(2)
            ))
            .bearer_auth(token.trim())
            .header("X-AQ-Graph-API-Version", "1")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    let before = history(&origin, &token).await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(&e);
    let (status, body) = submit(&e, request.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["state"], "unknown");
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    let retry = recovery(&e, &request["operation_id"]);
    let (status, body) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(retry),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["operations"][0]["state"], "confirmed");
    let after = history(&origin, &token).await;
    assert_eq!(
        after["items"].as_array().unwrap().len(),
        before["items"].as_array().unwrap().len() + 1
    );
    let data = e.fake.data.lock().await;
    assert_eq!(data.posts.len(), 2);
    assert_eq!(data.posts[0], data.posts[1]);
    drop(data);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::UNAUTHORIZED);
    let issue = json!({"identity":{"node_id":e.binding["node_id"],"boot_id":e.binding["boot_id"]},"attempt":{"attempt_id":e.binding["attempt_id"],"session_id":e.binding["session_id"],"generation":e.binding["generation"]},"expected_credential_version":1});
    let (status, issued) = send(
        &e.app,
        "/v2/worker/agent-session",
        Some(&e.node_token),
        Some(issue),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    e.token = issued["token"].as_str().unwrap().into();
    assert_eq!(submit(&e, correction(&e)).await.0, StatusCode::CONFLICT);
    assert_eq!(
        history(&origin, &token).await,
        after,
        "stale operation never becomes blind overwrite"
    );
    let finding = command(
        &e,
        json!([{"kind":"create_finding","id":Uuid::new_v4(),"title":"AQ07 retained provenance","body":"Synthetic isolated qualification finding","labels":[],"content_type":"fact"}]),
    );
    let (status, result) = submit(&e, finding).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let edge = Uuid::new_v4();
    let relation = command(
        &e,
        json!([{"kind":"add_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"3","edge_type":"references","source_revision":null,"target_revision":null,"description":"AQ07 isolated qualification"}]),
    );
    let (status, added) = submit(&e, relation).await;
    assert_eq!(status, StatusCode::OK, "{added}");
    let remove = command(
        &e,
        json!([{"kind":"remove_relationship","id":edge,"source":uid(2),"target":uid(3),"expected_source_version":"4","expected_version":"1"}]),
    );
    let (status, removed) = submit(&e, remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    let mut draft = proposal(&e);
    let proposed = draft["commands"][0]["tasks"][0]["id"].clone();
    draft["commands"][0]["relationships"] =
        json!([{"id":Uuid::new_v4(),"source":uid(1),"target":proposed,"edge_type":"parent-child"}]);
    let proposed_id = draft["commands"][0]["tasks"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, receipt) = submit(&e, draft).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["applied"], false);
    let unavailable = reqwest::Client::new()
        .get(format!(
            "{origin}/v1/workspaces/{}/nodes/{proposed_id}",
            uid(105)
        ))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(
        unavailable.status(),
        StatusCode::NOT_FOUND,
        "draft task must remain unapplied"
    );
}
async fn admit_only(e: &Env, request: &Value) -> (StatusCode, Value) {
    let response = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/agent/graph/commands")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", e.token))
                .header("Prefer", "respond-async")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn queued_admission_cancellation_wins_before_first_dispatch() {
    let e = setup().await;
    let request = correction(&e);
    let (status, queued) = admit_only(&e, &request).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["effect"], "not_dispatched");
    assert!(e.fake.data.lock().await.posts.is_empty());
    let node: Uuid = e.binding["node_id"].as_str().unwrap().parse().unwrap();
    let mut tx = e.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await
        .unwrap();
    let app = e.admin.clone();
    let retry_path = format!(
        "/api/agent-operations/{}/retry",
        request["operation_id"].as_str().unwrap()
    );
    let dispatch =
        tokio::spawn(async move { send(&app, &retry_path, None, Some(json!({}))).await });
    tokio::task::yield_now().await;
    let cancel_path = format!(
        "/api/agent-operations/{}/cancel",
        request["operation_id"].as_str().unwrap()
    );
    let (status, cancel) = send(&e.admin, &cancel_path, None, Some(json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(cancel["effect"], "not_applied");
    tx.commit().await.unwrap();
    assert_eq!(dispatch.await.unwrap().1, cancel);
    assert!(e.fake.data.lock().await.posts.is_empty());
}
#[tokio::test]
async fn retained_budget_refuses_new_intents_but_preserves_exact_replay() {
    let e = setup().await;
    let request = correction(&e);
    assert_eq!(admit_only(&e, &request).await.0, StatusCode::ACCEPTED);
    // A conservative retained reservation can exhaust the byte budget without a
    // large payload fixture. Admission must count it, not recalculate/prune it.
    sqlx::query("UPDATE agent_graph_operations SET retained_bytes=33554432")
        .execute(&e.pool)
        .await
        .unwrap();
    assert_eq!(admit_only(&e, &request).await.0, StatusCode::ACCEPTED);
    let (status, body) = admit_only(&e, &correction(&e)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "overloaded");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_operations")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
    assert!(e.fake.data.lock().await.posts.is_empty());
}
#[tokio::test]
async fn recovery_cursor_freezes_membership_and_serializes_its_finite_quota() {
    let e = setup().await;
    let first = correction(&e);
    let second = correction(&e);
    assert_eq!(admit_only(&e, &first).await.0, StatusCode::ACCEPTED);
    assert_eq!(admit_only(&e, &second).await.0, StatusCode::ACCEPTED);
    let mut request = recovery(&e, &first["operation_id"]);
    request["mode"] = json!("list");
    request.as_object_mut().unwrap().remove("operation_ids");
    request["limit"] = json!(1);
    request["cursor"] = Value::Null;
    let (status, page) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["operations"][0]["operation_id"], first["operation_id"]);
    let opaque = page["next_cursor"].as_str().unwrap();
    assert!(opaque.starts_with("aqc_") && !opaque.starts_with("aqa_"));
    // A later UUID sorts below previous IDs but is beyond the frozen membership.
    let mut concurrent = correction(&e);
    concurrent["operation_id"] = json!(uid(1));
    assert_eq!(admit_only(&e, &concurrent).await.0, StatusCode::ACCEPTED);
    let mut next = request.clone();
    next["cursor"] = page["next_cursor"].clone();
    let (status, tail) = send(
        &e.app,
        "/v2/worker/agent-operations/query",
        Some(&e.node_token),
        Some(next.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tail}");
    assert_eq!(tail["operations"].as_array().unwrap().len(), 1);
    assert_eq!(
        tail["operations"][0]["operation_id"],
        second["operation_id"]
    );
    assert_eq!(tail["coverage"]["status"], "complete");
    next["limit"] = json!(2);
    assert_eq!(
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(next)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    // Reserve 127 valid opaque cursor rows, then race two allocations. Node locking
    // must let exactly one reach the 128 ceiling; no thousands-of-operation fixture.
    sqlx::query("INSERT INTO agent_operation_cursors(verifier,node_id,attempt_id,query_digest,after_sequence,upper_sequence,expires_at_ms) SELECT 'fixture-'||n,$1,$2,'fixture',0,0,2000000 FROM generate_series(1,126) n").bind(e.binding["node_id"].as_str().unwrap().parse::<Uuid>().unwrap()).bind(e.binding["attempt_id"].as_str().unwrap().parse::<Uuid>().unwrap()).execute(&e.pool).await.unwrap();
    let (a, b) = tokio::join!(
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(request.clone())
        ),
        send(
            &e.app,
            "/v2/worker/agent-operations/query",
            Some(&e.node_token),
            Some(request)
        )
    );
    let statuses = [a.0, b.0];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::SERVICE_UNAVAILABLE));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_operation_cursors")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        128
    );
}
#[tokio::test]
async fn retry_budget_and_changed_service_mapping_hold_unknown_without_new_dispatch() {
    let e = setup().await;
    e.fake
        .lose_once
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let request = correction(&e);
    assert_eq!(submit(&e, request.clone()).await.1["state"], "unknown");
    let temp = tempfile::tempdir().unwrap();
    let token_file = temp.path().join("replacement-service");
    std::fs::write(
        &token_file,
        "replacement-ordinary-credential-0000000000000000",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut state = e.state.clone();
    state.agent_access = Some(Arc::new(
        Runtime::new(&AgentAccessConfig {
            graph_url: e.graph_origin.clone(),
            graph_token_file: token_file,
            mutation_actor_id: Some("api01-writer".into()),
            credential_ttl_ms: 10000,
        })
        .await
        .unwrap(),
    ));
    let replacement = aq_server::build_worker_router(state);
    e.time.store(1_002_000, std::sync::atomic::Ordering::SeqCst);
    let lookup = recovery(&e, &request["operation_id"]);
    let (_, held) = send(
        &replacement,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup.clone()),
    )
    .await;
    assert_eq!(held["operations"][0]["state"], "unknown");
    assert_eq!(
        held["operations"][0]["recovery"]["action"],
        "manual_reconciliation"
    );
    assert_eq!(e.fake.data.lock().await.posts.len(), 1);
    e.time.store(1_031_000, std::sync::atomic::Ordering::SeqCst);
    let (_, exhausted) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup),
    )
    .await;
    assert_eq!(
        exhausted["operations"][0]["recovery"]["remaining_elapsed_ms"],
        0
    );
    assert_eq!(
        exhausted["operations"][0]["recovery"]["action"],
        "manual_reconciliation"
    );
    assert_eq!(e.fake.data.lock().await.posts.len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn local_receipt_commit_failure_keeps_unknown_and_recovery_publishes_once() {
    let e = setup().await;
    sqlx::raw_sql("CREATE FUNCTION aq07_fail_outbox() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic local receipt fault'; END $$; CREATE TRIGGER aq07_fail_outbox BEFORE INSERT ON agent_graph_outbox FOR EACH ROW EXECUTE FUNCTION aq07_fail_outbox();").execute(&e.pool).await.unwrap();
    let request = correction(&e);
    let (status, unknown) = submit(&e, request.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{unknown}");
    assert_eq!(unknown["state"], "unknown");
    assert_eq!(unknown["effect"], "possible");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        0
    );
    let state: Value = sqlx::query_scalar("SELECT status FROM agent_graph_operations")
        .fetch_one(&e.pool)
        .await
        .unwrap();
    assert_eq!(
        state["state"], "unknown",
        "failed local transaction cannot publish a confirmed status without its outbox"
    );
    sqlx::query("DROP TRIGGER aq07_fail_outbox ON agent_graph_outbox")
        .execute(&e.pool)
        .await
        .unwrap();
    e.time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    let lookup = recovery(&e, &request["operation_id"]);
    let (_, body) = send(
        &e.app,
        "/v2/worker/agent-operations/retry",
        Some(&e.node_token),
        Some(lookup),
    )
    .await;
    assert_eq!(body["operations"][0]["state"], "confirmed");
    assert_eq!(e.fake.data.lock().await.applied, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_graph_outbox")
            .fetch_one(&e.pool)
            .await
            .unwrap(),
        1
    );
}
