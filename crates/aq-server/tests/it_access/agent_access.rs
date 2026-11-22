//! Real PostgreSQL authority tests; no graph or credential authority is mocked.
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
#[cfg(all(feature = "project_environments", feature = "delivery_playbook"))]
mod environment_try;
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
#[tokio::test]
async fn issuance_rotation_revocation_and_locked_authority_are_online() {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
    let owner = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap()
        .unwrap();
    let mut node_token = tokens::new_node_credential();
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
    let profile = fixture["context"]["profile"].clone();
    let context = fixture["context"]["context"].clone();
    let mut binding = fixture["context"]["binding"].clone();
    binding["node_id"] = json!(node.id);
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
    let fake = Arc::new(FakeGraph {
        data: serde_json::from_str(include_str!(
            "../../../../protocol/agent-access/v1/fixtures/qualified-graph.json"
        ))
        .unwrap(),
        hold: std::sync::atomic::AtomicBool::new(false),
        arrived: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fake_url = format!("http://{}", listener.local_addr().unwrap());
    let fake_app = Router::new().fallback(fake_graph).with_state(fake.clone());
    let fake_server = tokio::spawn(async move { axum::serve(listener, fake_app).await.unwrap() });
    let runtime = Runtime::new(&AgentAccessConfig {
        mutation_actor_id: None,
        graph_url: std::env::var("AQ_AGENT_GRAPH_TEST_URL").unwrap_or(fake_url),
        graph_token_file: std::env::var_os("AQ_AGENT_GRAPH_TEST_TOKEN_FILE")
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
    let app = aq_server::agent_access::admin_routes(state.clone())
        .merge(aq_server::agent_access::machine_routes(state.clone()))
        .with_state(state.clone());
    let mut small_profile = profile.clone();
    small_profile["max_response_bytes"] = json!(context.to_string().len().max(1024) + 10);
    let small_digest = tokens::verifier(&small_profile.to_string());
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer,spec,bindings,graph,profile_digest}',$2) WHERE attempt_id=$1").bind(attempt_id).bind(json!(small_digest)).execute(&pool).await.unwrap();
    let (status, _) = send(
        &app,
        "/api/agent-access/provision",
        None,
        Some(json!({"attempt_id":attempt_id,"profile":small_profile,"context":context})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "snapshot fits but full context does not"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_launch_bindings WHERE attempt_id=$1")
            .bind(attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "oversized provision cannot create authority");
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer,spec,bindings,graph,profile_digest}',$2) WHERE attempt_id=$1").bind(attempt_id).bind(binding["profile_digest"].clone()).execute(&pool).await.unwrap();
    let (status, _) = send(
        &app,
        "/api/agent-access/provision",
        None,
        Some(json!({"attempt_id":attempt_id,"profile":profile,"context":context})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let issue = json!({"identity":identity,"attempt":attempt,"expected_credential_version":0});
    for (field, value, status) in [
        ("node_id", json!(Uuid::new_v4()), StatusCode::UNAUTHORIZED),
        ("boot_id", json!(Uuid::new_v4()), StatusCode::CONFLICT),
    ] {
        let mut wrong = issue.clone();
        wrong["identity"][field] = value;
        assert_eq!(
            send(
                &app,
                "/v2/worker/agent-session",
                Some(&node_token),
                Some(wrong)
            )
            .await
            .0,
            status
        );
    }
    let mut wrong_generation = issue.clone();
    wrong_generation["attempt"]["generation"] = json!(2);
    assert_eq!(
        send(
            &app,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(wrong_generation)
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE agent_launch_bindings SET profile=$2 WHERE attempt_id=$1")
        .bind(attempt_id)
        .bind(&small_profile)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(
            &app,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(issue.clone())
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let (version,verifier):(i64,Option<String>)=sqlx::query_as("SELECT credential_version,credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1").bind(attempt_id).fetch_one(&pool).await.unwrap();
    assert_eq!(version, 0);
    assert!(
        verifier.is_none(),
        "response refusal cannot commit an unreachable credential"
    );
    sqlx::query("UPDATE agent_launch_bindings SET profile=$2 WHERE attempt_id=$1")
        .bind(attempt_id)
        .bind(&profile)
        .execute(&pool)
        .await
        .unwrap();
    // A credential authenticated before a real node-lock wait cannot issue an
    // agent credential after that machine credential rotates under the lock.
    let mut locked = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.id)
        .execute(&mut *locked)
        .await
        .unwrap();
    let xid: String = sqlx::query_scalar("SELECT txid_current()::text")
        .fetch_one(&mut *locked)
        .await
        .unwrap();
    let pending_app = app.clone();
    let old_token = node_token.clone();
    let pending_issue = issue.clone();
    let pending = tokio::spawn(async move {
        send(
            &pending_app,
            "/v2/worker/agent-session",
            Some(&old_token),
            Some(pending_issue),
        )
        .await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE transactionid::text=$1 AND NOT granted)",
        )
        .bind(&xid)
        .fetch_one(&pool)
        .await
        .unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "issuance did not reach the observed node lock wait"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let rotated_node_token = tokens::new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(node.id)
        .bind(tokens::verifier(&rotated_node_token))
        .execute(&mut *locked)
        .await
        .unwrap();
    locked.commit().await.unwrap();
    assert_eq!(
        pending.await.unwrap().0,
        StatusCode::UNAUTHORIZED,
        "the old credential cannot mint agent authority after rotation"
    );
    let (version, verifier): (i64, Option<String>) = sqlx::query_as(
        "SELECT credential_version,credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1")
        .bind(attempt_id).fetch_one(&pool).await.unwrap();
    assert_eq!(version, 0);
    assert!(
        verifier.is_none(),
        "refused stale machine credential changed issuance state"
    );
    assert_eq!(
        send(
            &app,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(issue.clone())
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // The following positive issuance and all other cases use the new credential.
    node_token = rotated_node_token;
    let (status, issued) = send(
        &app,
        "/v2/worker/agent-session",
        Some(&node_token),
        Some(issue.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{issued}");
    let agent = issued["token"].as_str().unwrap();
    assert_eq!(
        send(&app, "/v1/agent/context", Some(&node_token), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&app, "/v1/agent/context", Some(agent), None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(issue.clone())
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(&app, "/v1/agent/context", Some(agent), None).await.0,
        StatusCode::OK,
        "retry cannot invalidate credential"
    );
    let persisted: Option<String> = sqlx::query_scalar(
        "SELECT credential_verifier FROM agent_launch_bindings WHERE attempt_id=$1",
    )
    .bind(attempt_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(persisted.as_deref(), Some(tokens::verifier(agent).as_str()));
    let mut rotate = issue;
    rotate["expected_credential_version"] = json!(1);
    let (status, rotated) = send(
        &app,
        "/v2/worker/agent-session",
        Some(&node_token),
        Some(rotate),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        send(&app, "/v1/agent/context", Some(agent), None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let new_agent = rotated["token"].as_str().unwrap();
    {
        let queries = vec![
            json!({"kind":"get","node_id":binding["task_id"]}),
            json!({"kind":"revision","node_id":binding["task_id"],"revision":"1"}),
            json!({"kind":"context","node_id":binding["task_id"],"depth":1,"direction":"both","max_nodes":64,"max_edges":256,"max_bytes":1048576}),
            json!({"kind":"relationships","node_id":binding["task_id"],"direction":"both","edge_types":["references","parent-child"],"limit":64,"cursor":null}),
            json!({"kind":"history","node_id":binding["task_id"],"limit":64,"cursor":null}),
            json!({"kind":"search","text":"graph","kind_filter":null,"limit":64,"cursor":null}),
        ];
        for query in queries {
            let (status,result)=send(&app,"/v1/agent/graph/read",Some(new_agent),Some(json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":binding,"query":query}))).await;
            assert_eq!(status, StatusCode::OK, "{query}: {result}");
            let text = result.to_string();
            assert!(!text.contains("UNAUTHORIZED-CANARY"));
            assert!(!text.contains("metadata"));
            assert!(!text.contains("created_by"));
        }
        let denied = json!({"kind":"get","node_id":"00000000-0000-0000-0000-000000000004"});
        assert_eq!(send(&app,"/v1/agent/graph/read",Some(new_agent),Some(json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":binding,"query":denied}))).await.0,StatusCode::NOT_FOUND);
    }

    let vectors: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../protocol/agent-access/v1/fixtures/scope-cases.json"
    ))
    .unwrap();
    for vector in vectors.iter().filter(|v| v["action"] == "read") {
        let mut request = vector["request"].clone();
        request["binding"] = binding.clone();
        let token = match vector["principal_kind"].as_str().unwrap() {
            "agent_session" => new_agent,
            "node" => &node_token,
            _ => "ordinary-service-credential-0000000000000000",
        };
        let (status, body) = send(&app, "/v1/agent/graph/read", Some(token), Some(request)).await;
        let expected = match vector["expected"].as_str().unwrap() {
            "allowed" => StatusCode::OK,
            "not_found" => StatusCode::NOT_FOUND,
            "unauthorized" => StatusCode::UNAUTHORIZED,
            _ => panic!("unhandled scope vector"),
        };
        assert_eq!(status, expected, "{}: {body}", vector["name"]);
        assert!(!body.to_string().contains("UNAUTHORIZED-CANARY"));
    }
    if std::env::var_os("AQ_AGENT_GRAPH_TEST_URL").is_none() {
        let paged =
            json!({"kind":"search","text":"paged","kind_filter":null,"limit":1,"cursor":null});
        let request = |q: Value| json!({"api_version":1,"request_id":Uuid::new_v4(),"binding":binding,"query":q});
        let (status, first) = send(
            &app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(request(paged.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["result"]["nodes"].as_array().unwrap().len(), 1);
        assert_eq!(first["result"]["nodes"][0]["id"], binding["task_id"]);
        let mut hidden = paged.clone();
        hidden["text"] = json!("hidden");
        let (status, partial) = send(
            &app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(request(hidden)),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(partial["result"]["nodes"].as_array().unwrap().is_empty());
        assert_eq!(partial["result"]["coverage"]["status"], "partial");
        assert!(!partial.to_string().contains("UNAUTHORIZED-CANARY"));
        let budget = json!({"kind":"context","node_id":binding["task_id"],"depth":1,"direction":"both","max_nodes":64,"max_edges":256,"max_bytes":1024});
        assert_eq!(
            send(
                &app,
                "/v1/agent/graph/read",
                Some(new_agent),
                Some(request(budget))
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let budget = json!({"kind":"context","node_id":binding["task_id"],"depth":1,"direction":"both","max_nodes":1,"max_edges":1,"max_bytes":1048576});
        let (_, partial) = send(
            &app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(request(budget)),
        )
        .await;
        assert_eq!(partial["result"]["coverage"]["status"], "partial");
        let cursor = first["result"]["next_cursor"].clone();
        // aq-worker's leak guard refuses agent output holding an `aqa_` credential
        // shape, so a cursor an agent prints must never look like one.
        let opaque = cursor.as_str().unwrap();
        assert!(opaque.starts_with("aqc_") && !opaque.starts_with("aqa_"));
        assert_eq!(opaque.len(), 47);
        assert_ne!(cursor, "visible-two");
        let mut next = paged.clone();
        next["cursor"] = cursor.clone();
        next["text"] = json!("changed");
        assert_eq!(
            send(
                &app,
                "/v1/agent/graph/read",
                Some(new_agent),
                Some(request(next.clone()))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        next["text"] = json!("paged");
        let (status, second) = send(
            &app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(request(next.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(second["result"]["next_cursor"].is_null());
        // A cursor issued before the `aqc_` prefix (`aqa_`) is still honoured: the
        // server looks cursors up by the verifier of the whole opaque string.
        let legacy = format!("aqa_{}", &opaque[4..]);
        sqlx::query("UPDATE agent_read_cursors SET verifier=$1 WHERE verifier=$2")
            .bind(tokens::verifier(&legacy))
            .bind(tokens::verifier(opaque))
            .execute(&pool)
            .await
            .unwrap();
        next["cursor"] = json!(legacy);
        let (status, legacy_page) = send(
            &app,
            "/v1/agent/graph/read",
            Some(new_agent),
            Some(request(next.clone())),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{legacy_page}");
        assert_eq!(legacy_page["result"], second["result"]);
        // An in-flight graph read releases central locks. Cancellation/expiry must
        // be observed after the graph barrier, before projecting any response.
        for expire in [true, false] {
            fake.hold.store(true, std::sync::atomic::Ordering::SeqCst);
            let read_app = app.clone();
            let read_token = new_agent.to_owned();
            let read_request = request(json!({"kind":"get","node_id":binding["task_id"]}));
            let pending = tokio::spawn(async move {
                send(
                    &read_app,
                    "/v1/agent/graph/read",
                    Some(&read_token),
                    Some(read_request),
                )
                .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), fake.arrived.notified())
                .await
                .unwrap();
            if expire {
                time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
            } else {
                sqlx::query("UPDATE agent_launch_bindings SET revoked=TRUE WHERE attempt_id=$1")
                    .bind(attempt_id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            fake.release.notify_one();
            let (status, body) = pending.await.unwrap();
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(body.get("result").is_none());
            time.store(1_000_000, std::sync::atomic::Ordering::SeqCst);
            sqlx::query("UPDATE agent_launch_bindings SET revoked=FALSE WHERE attempt_id=$1")
                .bind(attempt_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        // The same opaque cursor ceases to authorize after credential rotation.
        let rotate = json!({"identity":identity,"attempt":attempt,"expected_credential_version":2});
        let (_, third) = send(
            &app,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(rotate),
        )
        .await;
        let third_token = third["token"].as_str().unwrap();
        assert_eq!(
            send(
                &app,
                "/v1/agent/graph/read",
                Some(third_token),
                Some(request(next))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        // Return to version 2 only in this disposable fixture so the remaining
        // independent queued-expiry/cancellation checks use the known token.
        sqlx::query("UPDATE agent_launch_bindings SET credential_version=2,credential_verifier=$2 WHERE attempt_id=$1").bind(attempt_id).bind(tokens::verifier(new_agent)).execute(&pool).await.unwrap();
    }
    let mut locked = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.id)
        .execute(&mut *locked)
        .await
        .unwrap();
    let pending_app = app.clone();
    let pending_token = node_token.clone();
    let queued = json!({"identity":identity,"attempt":attempt,"expected_credential_version":2});
    let pending = tokio::spawn(async move {
        send(
            &pending_app,
            "/v2/worker/agent-session",
            Some(&pending_token),
            Some(queued),
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!pending.is_finished());
    time.store(1_200_000, std::sync::atomic::Ordering::SeqCst);
    locked.commit().await.unwrap();
    assert_eq!(pending.await.unwrap().0, StatusCode::UNAUTHORIZED);
    time.store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    let mut locked = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node.id)
        .execute(&mut *locked)
        .await
        .unwrap();
    let pending_app = app.clone();
    let pending_token = new_agent.to_owned();
    let pending = tokio::spawn(async move {
        send(
            &pending_app,
            "/v1/agent/context",
            Some(&pending_token),
            None,
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!pending.is_finished(), "authority waits for node lock");
    time.store(1_020_000, std::sync::atomic::Ordering::SeqCst);
    locked.commit().await.unwrap();
    assert_eq!(
        pending.await.unwrap().0,
        StatusCode::UNAUTHORIZED,
        "expiry sampled after lock"
    );
    time.store(1_000_000, std::sync::atomic::Ordering::SeqCst);
    let spool = tempfile::tempdir().unwrap();
    std::fs::create_dir(spool.path().join("status")).unwrap();
    std::fs::write(spool.path().join("status/maintenance"), "").unwrap();
    let mut maintenance_state = state.clone();
    let mut maintenance_config = state.config.as_ref().clone();
    maintenance_config.updates = Some(aq_server::config::UpdatesConfig {
        spool_dir: spool.path().to_path_buf(),
    });
    maintenance_state.config = Arc::new(maintenance_config);
    let admin = aq_server::build_admin_router(maintenance_state.clone());
    let worker = aq_server::build_worker_router(maintenance_state);
    let rotate = json!({"identity":identity,"attempt":attempt,"expected_credential_version":2});
    assert_eq!(
        send(
            &worker,
            "/v2/worker/agent-session",
            Some(&node_token),
            Some(rotate)
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        send(&worker, "/v1/agent/context", Some(new_agent), None)
            .await
            .0,
        StatusCode::OK,
        "existing read authority survives maintenance"
    );
    assert_eq!(
        send(
            &admin,
            &format!("/api/agent-access/{attempt_id}/revoke"),
            None,
            Some(json!({}))
        )
        .await
        .0,
        StatusCode::OK,
        "maintenance permits owner revocation"
    );
    assert_eq!(
        send(&worker, "/v1/agent/context", Some(new_agent), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    // Restore fixture authority solely to isolate the following cancellation check.
    sqlx::query(
        "UPDATE agent_launch_bindings SET revoked=FALSE,credential_verifier=$2 WHERE attempt_id=$1",
    )
    .bind(attempt_id)
    .bind(tokens::verifier(new_agent))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"cancelling\"') WHERE attempt_id=$1").bind(attempt_id).execute(&pool).await.unwrap();
    assert_eq!(
        send(&app, "/v1/agent/context", Some(new_agent), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    fake_server.abort();
}

struct FakeGraph {
    data: Value,
    hold: std::sync::atomic::AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
async fn fake_graph(
    axum::extract::State(f): axum::extract::State<Arc<FakeGraph>>,
    uri: axum::http::Uri,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> ([(&'static str, &'static str); 1], axum::Json<Value>) {
    if f.hold.swap(false, std::sync::atomic::Ordering::SeqCst) {
        f.arrived.notify_one();
        f.release.notified().await;
    }
    let path = uri.path();
    let data = if path.ends_with("/search") {
        if query.get("q").is_some_and(|q| q == "hidden") {
            json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000004"]],"next_cursor":"hidden-next"})
        } else if query.get("q").is_some_and(|q| q == "paged") {
            match query.get("cursor").map(String::as_str) {
                None => {
                    json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000004"]],"next_cursor":"visible-one"})
                }
                Some("visible-one") => {
                    json!({"items":[f.data["context"]["node"]],"next_cursor":"visible-two"})
                }
                Some("visible-two") => {
                    json!({"items":[f.data["nodes"]["00000000-0000-0000-0000-000000000002"]],"next_cursor":null})
                }
                _ => panic!("unexpected cursor"),
            }
        } else {
            json!({"items":f.data["nodes"].as_object().unwrap().values().filter(|n|query.get("kind").is_none_or(|kind|n["kind"].as_str()==Some(kind.as_str()))).cloned().collect::<Vec<_>>(),"next_cursor":null})
        }
    } else if path.ends_with("/context") {
        f.data["context"].clone()
    } else if path.ends_with("/history") {
        f.data["history"].clone()
    } else if path.contains("/revisions/") {
        f.data["revision"].clone()
    } else {
        f.data["nodes"][path.rsplit('/').next().unwrap()].clone()
    };
    ([("X-AQ-Graph-API-Version", "1")], axum::Json(data))
}
