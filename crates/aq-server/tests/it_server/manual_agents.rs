//! Real SQL and production auth/router boundaries; no model invocation.
use crate::common;
use aq_server::{build_admin_router, build_worker_router};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn owner_can_manage_machine_and_node_names_limits_and_removal() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let node = f.node.node_id;
    let sibling = Uuid::new_v4();
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')")
        .bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE nodes SET machine_key='managed-pc',slot=1 WHERE id=$1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,machine_key,slot) VALUES($1,'second','test-sibling','0.5.1',1,now(),'managed-pc',2)")
        .bind(sibling).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO manual_worker_reports(node_id,max_slots,repositories,observed_at_ms) VALUES($1,1,'[]',$2)")
        .bind(node).bind(chrono::Utc::now().timestamp_millis()).execute(&pool).await.unwrap();
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let node_path = format!("/api/worker-management/node/{node}");
    let node_change = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"label":"Build worker","max_agents":3});
    assert_eq!(
        call(&admin, &node_path, Some(node_change.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, receipt) = call(&admin, &node_path, Some(node_change.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["revision"], 1);
    assert_eq!(
        call(&admin, &node_path, Some(node_change), None, true)
            .await
            .1,
        receipt
    );
    let name: String = sqlx::query_scalar("SELECT name FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "Build worker");
    let stale =
        json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"label":"stale","max_agents":1});
    assert_eq!(
        call(&admin, &node_path, Some(stale), None, true).await.0,
        StatusCode::CONFLICT
    );
    let fleet = call(&admin, "/api/manual-agents", None, None, false)
        .await
        .1;
    assert_eq!(fleet["workers"][0]["max_slots"], 3);
    assert_eq!(
        call(&worker, "/api/worker-management", None, None, false)
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let machine_path = "/api/worker-management/machine/managed-pc";
    let machine_change = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"label":"Office PC","max_agents":2});
    let (status, receipt) = call(&admin, machine_path, Some(machine_change), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let (status, tree) = call(&admin, "/api/v1/swarm", None, None, false).await;
    assert_eq!(status, StatusCode::OK, "{tree}");
    let machine = tree["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "managed-pc")
        .unwrap();
    assert_eq!(machine["label"], "Office PC");
    for n in [node, sibling] {
        sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,'{}','running','Running',1,1)")
            .bind(Uuid::new_v4()).bind(f.actor).bind(n).bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    }
    let mut conn = pool.acquire().await.unwrap();
    let limits = aq_machine_control::management::capacity(&mut conn, node)
        .await
        .unwrap();
    assert_eq!(limits.node_used, 1);
    assert_eq!(limits.machine_used, 2);
    assert!(limits.full());
    assert!(
        aq_machine_control::offer_hold(&mut conn, node)
            .await
            .unwrap()
            .is_some()
    );
    drop(conn);
    let zero = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"label":"Office PC","max_agents":0});
    assert_eq!(
        call(&admin, machine_path, Some(zero), None, true).await.0,
        StatusCode::OK
    );
    let live: i64 =
        sqlx::query_scalar("SELECT count(*) FROM manual_agent_sessions WHERE desired_running")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(live, 2, "Lowering a limit must keep the running agents");
    let remove = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"label":"Build worker","max_agents":3,"remove":true});
    let (status, receipt) = call(&admin, &node_path, Some(remove.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert!(receipt["removed"].as_bool().unwrap());
    assert_eq!(
        call(&admin, &node_path, Some(remove), None, true).await.1,
        receipt
    );
    let revoked: bool = sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !revoked,
        "The machine controller must keep serving sibling workers"
    );
    let running: bool =
        sqlx::query_scalar("SELECT desired_running FROM manual_agent_sessions WHERE node_id=$1")
            .bind(sibling)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(running);
    let fleet = call(&admin, "/api/manual-agents", None, None, false)
        .await
        .1;
    assert!(fleet["workers"].as_array().unwrap().is_empty());
    let remove = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"label":"Office PC","max_agents":0,"remove":true});
    let (status, receipt) = call(&admin, machine_path, Some(remove), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM nodes WHERE machine_key='managed-pc' AND revoked_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
}
async fn call(
    app: &Router,
    path: &str,
    body: Option<Value>,
    bearer: Option<&str>,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json");
    if csrf {
        req = req.header("x-aq-request", "1");
    }
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let res = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn persistent_owner_session_has_scoped_idempotent_lifecycle_slots_and_terminal_input() {
    let mut f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    let repo = f.profiled.source.repository_id;
    let node = f.node.node_id;
    let digest = "a".repeat(64);
    let catalog = json!({"catalog":{"intelligence":[{"id":"manual-model","name":"Standard","version":1,"candidates":[{"id":"claude","provider":"anthropic","model":"fixture-model","harnesses":["claude"],"reasoning_configuration_reference":"fixture-medium@1"},{"id":"codex","provider":"openai","model":"gpt-test","harnesses":["codex"],"reasoning_configuration_reference":"fixture-high@1"}]}]}});
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,1)").bind(&digest).bind(serde_json::to_vec(&catalog).unwrap()).bind(f.actor).execute(&pool).await.unwrap();
    let binding = json!({"enabled":true,"name":"manual fixture","repository_id":repo,"catalog_digest":digest,"allowed_intelligence_profiles":["manual-model@1"],"allowed_node_ids":[],"allowed_worker_groups":[]});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')").bind(project).bind(binding).execute(&pool).await.unwrap();
    sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
        .bind(&digest)
        .execute(&pool)
        .await
        .unwrap();
    let preferences = tempfile::tempdir().unwrap();
    let preferences_path = preferences.path().join("models.json");
    std::fs::write(&preferences_path,serde_json::to_vec(&json!([
        {"provider":"anthropic","model":"fixture-model","reasoning_configuration_reference":"fixture-medium@1","reasoning":"medium","harness":"claude"},
        {"provider":"openai","model":"gpt-test","reasoning_configuration_reference":"fixture-high@1","reasoning":"high","harness":"codex","options":{"service_tier":"fast"}}
    ])).unwrap()).unwrap();
    let mut config = (*f.state.config).clone();
    config.intelligence_preferences_file = Some(preferences_path);
    config.profiled_execution=Some(serde_json::from_value(json!({"revision":1,"qualified_runtimes":[],"projects":[{"project_id":project,"enabled":true,"priority":0,"source":{"kind":"immutable","object":{"format":"sha1","hex":"a".repeat(40)}},"retention_handle":Uuid::new_v4()}]})).unwrap());
    f.state.config = Arc::new(config);
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let (status, fleet) = call(&admin, "/api/supervisors", None, None, false).await;
    assert_eq!(status, StatusCode::OK, "{fleet}");
    let scopes = fleet["supervisors"].as_array().unwrap();
    let project_scope = scopes
        .iter()
        .find(|s| s["project_id"] == project.to_string())
        .unwrap();
    let conversation = project_scope["id"].as_str().unwrap();
    assert_ne!(
        conversation,
        scopes.iter().find(|s| s["project_id"].is_null()).unwrap()["id"]
    );
    let uri = format!("/api/supervisors/{conversation}/control");
    let control = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"enabled":true,"intelligence_profile":{"profile_id":"manual-model","profile_version":1,"candidate_id":"codex"}});
    assert_eq!(
        call(&admin, &uri, Some(control.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, receipt) = call(&admin, &uri, Some(control.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(
        call(&admin, &uri, Some(control.clone()), None, true)
            .await
            .1,
        receipt
    );
    let stale = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"enabled":false});
    assert_eq!(
        call(&admin, &uri, Some(stale), None, true).await.0,
        StatusCode::CONFLICT
    );
    let discover = format!("/api/human/conversations/projects/{project}");
    let (status, discovery) = call(&admin, &discover, None, None, false).await;
    assert_eq!(status, StatusCode::OK, "{discovery}");
    assert_eq!(discovery["conversation"]["id"], conversation);
    let message_operation = Uuid::new_v4();
    let message_id = Uuid::new_v4();
    let send = json!({"operation_id":message_operation,"kind":"supervisor.message.send","subject":{"type":"conversation","id":conversation,"revision":"1"},"expected_versions":{"conversation":"1"},"payload":{"operation_id":message_operation,"message_id":message_id,"expected_conversation_revision":"1","body":"Project message receipt QA","context":[],"attachments":[]}});
    let (status, sent) = call(&admin, "/api/v1/actions", Some(send), None, true).await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert_eq!(sent["data"]["disposition"], "confirmed");
    let (status, looked) = call(
        &admin,
        &format!("/api/v1/operations/{message_operation}"),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{looked}");
    assert_eq!(looked["data"]["disposition"], "confirmed");
    let stop = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"enabled":false});
    assert_eq!(
        call(&admin, &uri, Some(stop), None, true).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&admin, &discover, None, None, false).await.1["conversation"],
        discovery["conversation"]
    );

    let poll = json!({"max_slots":3,"repositories":[repo],"reports":[]});
    assert_eq!(
        call(
            &worker,
            "/v1/worker/manual-agents",
            Some(poll.clone()),
            None,
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &worker,
            "/v1/worker/manual-agents",
            Some(poll.clone()),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let selected = json!({"profile_id":"manual-model","profile_version":1,"candidate_id":"codex"});
    let selected_id = Uuid::new_v4();
    let selected_launch = format!("/api/manual-agents/workers/{node}/projects/{project}");
    let selected_body = json!({"operation_id":selected_id,"intelligence_profile":selected});
    assert_eq!(
        call(
            &admin,
            &selected_launch,
            Some(selected_body.clone()),
            None,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "old worker does not advertise Codex"
    );
    let mut upgraded_poll = poll.clone();
    upgraded_poll["providers"] = json!(["anthropic", "openai"]);
    assert_eq!(
        call(
            &worker,
            "/v1/worker/manual-agents",
            Some(upgraded_poll),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let choices = call(&admin, "/api/manual-agents", None, None, false)
        .await
        .1;
    assert_eq!(choices["launch_profiles"].as_array().unwrap().len(), 2);
    let (status, chosen) = call(
        &admin,
        &selected_launch,
        Some(selected_body.clone()),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{chosen}");
    assert_eq!(chosen["spec"]["model"], "gpt-test");
    assert_eq!(chosen["spec"]["reasoning"], "high");
    assert_eq!(chosen["spec"]["options"]["service_tier"], "fast");
    assert_eq!(
        call(
            &admin,
            &selected_launch,
            Some(selected_body.clone()),
            None,
            true
        )
        .await
        .1,
        chosen
    );
    let mut changed = selected_body.clone();
    changed["intelligence_profile"]["candidate_id"] = json!("claude");
    assert_eq!(
        call(&admin, &selected_launch, Some(changed), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut stale = selected_body.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    stale["intelligence_profile"]["profile_version"] = json!(2);
    assert_eq!(
        call(&admin, &selected_launch, Some(stale), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let deleted_digest = "b".repeat(64);
    let mut deleted_catalog = catalog.clone();
    deleted_catalog["catalog"]["intelligence"][0]["candidates"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["provider"] != "openai");
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,1)").bind(&deleted_digest).bind(serde_json::to_vec(&deleted_catalog).unwrap()).bind(f.actor).execute(&pool).await.unwrap();
    sqlx::query("UPDATE profile_catalog_registry SET active_digest=$1 WHERE singleton")
        .bind(&deleted_digest)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &admin,
            &selected_launch,
            Some(selected_body.clone()),
            None,
            true
        )
        .await
        .1,
        chosen,
        "exact retry retains admitted snapshot after a profile is edited/deleted"
    );
    let mut removed = selected_body.clone();
    removed["operation_id"] = json!(Uuid::new_v4());
    assert_eq!(
        call(&admin, &selected_launch, Some(removed), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    sqlx::query("DELETE FROM manual_agent_sessions WHERE id=$1")
        .bind(selected_id)
        .execute(&pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let launch = format!("/api/manual-agents/workers/{node}/projects/{project}");
    assert_eq!(
        call(
            &admin,
            &launch,
            Some(json!({"operation_id":id})),
            None,
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, first) = call(
        &admin,
        &launch,
        Some(json!({"operation_id":id})),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "pending");
    assert_eq!(
        call(
            &admin,
            &launch,
            Some(json!({"operation_id":id})),
            None,
            true
        )
        .await
        .1["id"],
        first["id"]
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM manual_agent_sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let response = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(poll.clone()),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert_eq!(response["sessions"][0]["status"], "preparing");
    let running = json!({"id":id,"status":"running","stage":"Ready to connect","snapshot":"real fixture screen","acknowledged":[]});
    let report = json!({"max_slots":3,"repositories":[repo],"reports":[running]});
    assert_eq!(
        call(
            &worker,
            "/v1/worker/manual-agents",
            Some(report.clone()),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let terminal = format!("/api/manual-agents/{id}/terminal");
    let client = Uuid::new_v4();
    let (status, control) = call(
        &admin,
        &format!("{terminal}/connect"),
        Some(json!({"client_id":client})),
        None,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{control}");
    let renewed = call(
        &admin,
        &format!("{terminal}/connect"),
        Some(json!({"client_id":client,"expected_generation":control["generation"]})),
        None,
        true,
    )
    .await;
    assert_eq!(renewed.0, StatusCode::OK);
    assert_eq!(renewed.1["generation"], control["generation"]);
    let other_client = Uuid::new_v4();
    let takeover = call(
        &admin,
        &format!("{terminal}/connect"),
        Some(json!({"client_id":other_client})),
        None,
        true,
    )
    .await;
    assert_eq!(takeover.0, StatusCode::OK);
    let stale = call(
        &admin,
        &format!("{terminal}/connect"),
        Some(json!({"client_id":client,"expected_generation":control["generation"]})),
        None,
        true,
    )
    .await;
    assert_eq!(
        stale.0,
        StatusCode::CONFLICT,
        "renewal cannot steal control back"
    );
    let (_, control) = call(
        &admin,
        &format!("{terminal}/connect"),
        Some(json!({"client_id":client})),
        None,
        true,
    )
    .await;
    let input = Uuid::new_v4();
    let body = json!({"operation_id":input,"client_id":client,"generation":control["generation"],"bytes":[65]});
    assert_eq!(
        call(
            &admin,
            &format!("{terminal}/input"),
            Some(body.clone()),
            None,
            true
        )
        .await
        .1["disposition"],
        "queued"
    );
    assert_eq!(
        call(&admin, &format!("{terminal}/input"), Some(body), None, true)
            .await
            .1["disposition"],
        "queued"
    );
    let response = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(report.clone()),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert_eq!(
        response["commands"][id.to_string()]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let response = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(report.clone()),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert!(
        response["commands"][id.to_string()].is_null(),
        "input is never replayed"
    );
    let mut acknowledged = report.clone();
    acknowledged["reports"][0]["acknowledged"] = json!([input]);
    call(
        &worker,
        "/v1/worker/manual-agents",
        Some(acknowledged),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(
        call(
            &admin,
            &format!("{terminal}/receipts/{input}"),
            None,
            None,
            true
        )
        .await
        .1["disposition"],
        "applied"
    );
    live_stream_preserves_bytes_and_fences_input(
        &admin,
        &worker,
        &f.credential,
        (
            f.state.config.server.public_origin.trim_end_matches('/'),
            &pool,
        ),
        id,
        client,
        control["generation"].as_u64().unwrap(),
    )
    .await;
    let slots = format!("{launch}/slots");
    let operation = Uuid::new_v4();
    let body = json!({"operation_id":operation,"expected_revision":0,"slots":0});
    let (status, changed) = call(&admin, &slots, Some(body.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["slots"], 0);
    assert_eq!(
        call(&admin, &slots, Some(body), None, true).await.1,
        changed
    );
    assert_eq!(
        call(
            &admin,
            &slots,
            Some(json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"slots":2})),
            None,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let next = Uuid::new_v4();
    call(
        &admin,
        &launch,
        Some(json!({"operation_id":next})),
        None,
        true,
    )
    .await;
    let response = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(report.clone()),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert_eq!(
        response["sessions"].as_array().unwrap().len(),
        1,
        "removing capacity keeps the running session and queues new sessions"
    );
    assert_eq!(response["pending_count"], 2);
    let stop = format!("/api/manual-agents/{id}/stop");
    assert_eq!(
        call(
            &admin,
            &stop,
            Some(json!({"operation_id":Uuid::new_v4()})),
            None,
            true
        )
        .await
        .1["status"],
        "stopping"
    );
    let response = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(report.clone()),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert_eq!(response["sessions"][0]["desired_running"], false);
    let mut stopped = report;
    stopped["reports"][0]["status"] = json!("stopped");
    call(
        &worker,
        "/v1/worker/manual-agents",
        Some(stopped),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(
        call(
            &admin,
            &format!("{terminal}/connect"),
            Some(json!({"client_id":client})),
            None,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &worker,
            "/v1/worker/manual-agents",
            Some(poll),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let tasks: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        tasks, 0,
        "manual sessions do not invent queue tasks or execution grants"
    );
}

async fn live_stream_preserves_bytes_and_fences_input(
    admin: &Router,
    worker: &Router,
    credential: &str,
    environment: (&str, &sqlx::PgPool),
    id: Uuid,
    client: Uuid,
    generation: u64,
) {
    use futures_util::{SinkExt, StreamExt};
    let (origin, pool) = environment;
    use tokio_tungstenite::{
        connect_async,
        tungstenite::{Message, client::IntoClientRequest},
    };
    let al = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ap = al.local_addr().unwrap().port();
    let wl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wp = wl.local_addr().unwrap().port();
    let admin_app = admin.clone();
    let worker_app = worker.clone();
    let at = tokio::spawn(async move { axum::serve(al, admin_app).await.unwrap() });
    let wt = tokio::spawn(async move { axum::serve(wl, worker_app).await.unwrap() });
    let worker_url = format!("ws://127.0.0.1:{wp}/v1/worker/manual-agents/{id}/terminal-stream");
    assert!(
        connect_async(&worker_url).await.is_err(),
        "worker stream requires enrolled authentication"
    );
    let mut request = worker_url.into_client_request().unwrap();
    let mut bearer = axum::http::HeaderValue::from_str(&format!("Bearer {credential}")).unwrap();
    bearer.set_sensitive(true);
    request.headers_mut().insert("authorization", bearer);
    let (mut node, _) = connect_async(request).await.unwrap();
    let url = format!(
        "ws://127.0.0.1:{ap}/api/manual-agents/{id}/terminal/live?client_id={client}&generation={generation}&columns=101&rows=37"
    );
    let mut bad = url.clone().into_client_request().unwrap();
    bad.headers_mut().insert(
        "origin",
        axum::http::HeaderValue::from_static("https://untrusted.example"),
    );
    assert!(
        connect_async(bad).await.is_err(),
        "owner WebSocket must check origin"
    );
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", origin.parse().unwrap());
    let (mut browser, _) = connect_async(request).await.unwrap();
    let message = stream_frame(&mut node).await.into_text().unwrap();
    let attach: Value = serde_json::from_str(&message).unwrap();
    assert_eq!(attach["kind"], "attach");
    assert_eq!(attach["columns"], 101);
    assert_eq!(attach["rows"], 37);
    let connection: Uuid = serde_json::from_value(attach["connection_id"].clone()).unwrap();
    node.send(Message::Text(
        json!({"kind":"ready","connection_id":connection})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    assert!(
        stream_frame(&mut browser)
            .await
            .into_text()
            .unwrap()
            .contains("ready")
    );
    let raw = b"\x1b[3;6Habc\x1b[?2004h";
    let mut output = connection.as_bytes().to_vec();
    output.extend_from_slice(raw);
    node.send(Message::Binary(output.into())).await.unwrap();
    assert_eq!(
        stream_frame(&mut browser).await.into_data(),
        raw.as_slice(),
        "cursor and paste-mode escapes are not screen captures"
    );
    browser
        .send(Message::Binary(b"a".as_slice().into()))
        .await
        .unwrap();
    browser
        .send(Message::Binary(b"\r".as_slice().into()))
        .await
        .unwrap();
    for expected in [json!([97]), json!([13])] {
        let v: Value =
            serde_json::from_str(&stream_frame(&mut node).await.into_text().unwrap()).unwrap();
        assert_eq!(v["kind"], "input");
        assert_eq!(v["bytes"], expected);
        assert_eq!(v["generation"], generation);
    }
    browser
        .send(Message::Text(
            json!({"kind":"resize","columns":90,"rows":31})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let resized: Value =
        serde_json::from_str(&stream_frame(&mut node).await.into_text().unwrap()).unwrap();
    assert_eq!(resized["columns"], 90);
    let other = Uuid::new_v4();
    let takeover = call(
        admin,
        &format!("/api/manual-agents/{id}/terminal/connect"),
        Some(json!({"client_id":other})),
        None,
        true,
    )
    .await;
    assert_eq!(takeover.0, StatusCode::OK);
    browser
        .send(Message::Binary(b"never-forward".as_slice().into()))
        .await
        .unwrap();
    let detached = tokio::time::timeout(std::time::Duration::from_secs(3), stream_frame(&mut node))
        .await
        .unwrap()
        .into_text()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&detached).unwrap()["kind"],
        "detach",
        "stale input is fenced, closing only its attach"
    );
    let node_id: Uuid = sqlx::query_scalar("SELECT node_id FROM manual_agent_sessions WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(node_id)
        .execute(pool)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match node.next().await {
                Some(Ok(Message::Ping(_))) => {
                    node.flush().await.unwrap();
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => panic!("revoked stream delivered data"),
            }
        }
    })
    .await
    .expect("original-node revocation must close the live stream");
    sqlx::query("UPDATE nodes SET revoked_at=NULL WHERE id=$1")
        .bind(node_id)
        .execute(pool)
        .await
        .unwrap();
    at.abort();
    wt.abort();
}

async fn stream_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> tokio_tungstenite::tungstenite::Message {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    loop {
        match socket.next().await.unwrap().unwrap() {
            Message::Ping(_) => socket.flush().await.unwrap(),
            Message::Pong(_) => {}
            frame => return frame,
        }
    }
}

#[tokio::test]
async fn logins_are_owner_requested_private_current_and_revocable() {
    use aq_server::intelligence::config::{
        CredentialConfig, IntelligenceConfig, MaterialDeliveryMode,
    };
    use std::os::unix::fs::PermissionsExt;
    let mut f = common::profiled::fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("login");
    std::fs::write(&path, b"synthetic-private-login\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let choice = &f.selection.choice;
    let credential = aq_selection::AuthorizedCredential {
        profile: choice.credential_profile.clone(),
        provider: "anthropic".into(),
        account_reference: choice.account_reference.clone(),
        endpoint_reference: choice.endpoint_reference.clone(),
        revision: f.selection.credential_revision,
        enabled: true,
        provisionable: false,
        material_owner: None,
        allowed_backends: aq_selection::default_allowed_backends(),
    };
    let cfg = IntelligenceConfig {
        accounting: Default::default(),
        material_store: None,
        revision: 1,
        credentials: vec![CredentialConfig {
            kind: aq_selection::CredentialKind::SubscriptionLogin,
            allowed_backends: aq_selection::default_allowed_backends(),
            credential,
            allowed_node_ids: vec![f.node.node_id],
            allow_enrolled_nodes: false,
            secret_file: Some(path.clone()),
            delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
        }],
        qualified_harnesses: vec![],
    };
    let mut config = (*f.state.config).clone();
    config.intelligence = Some(Arc::new(cfg.load().unwrap()));
    f.state.config = Arc::new(config);
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    assert_eq!(
        call(
            &admin,
            "/api/intelligence/register",
            Some(json!({})),
            None,
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    let node = f.node.node_id;
    let uri = format!("/api/worker-logins/{node}");
    let op = Uuid::new_v4();
    let request = json!({"operation_id":op,"providers":["anthropic"]});
    assert_eq!(
        call(&admin, &uri, Some(request.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&admin, &uri, Some(request.clone()), None, true)
            .await
            .1["generation"],
        op.to_string()
    );
    assert_eq!(
        call(&worker, "/v1/worker/logins", Some(json!({})), None, false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, private) = call(
        &worker,
        "/v1/worker/logins",
        Some(json!({})),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{private}");
    assert_eq!(private["logins"][0]["material"], "synthetic-private-login");
    let pool = f.db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        aq_machine_control::management::hold(&mut conn, node)
            .await
            .unwrap(),
        Some("installing server subscription logins")
    );
    drop(conn);

    let public = call(&admin, "/api/worker-logins", None, None, false)
        .await
        .1;
    assert!(!public.to_string().contains("synthetic-private"));
    assert!(!public.to_string().contains("material"));
    // Placement and credential kind remain authoritative even for an owner-requested push.
    for denial in 0..3 {
        let mut blocked = (*f.state.config).clone();
        let mut registry = blocked.intelligence.as_ref().unwrap().config.clone();
        match denial {
            0 => registry.credentials[0].allowed_node_ids.clear(),
            1 => registry.credentials[0].kind = aq_selection::CredentialKind::ApiKey,
            _ => registry.credentials[0].allowed_backends = vec![aq_selection::BackendKind::Boat],
        }
        blocked.intelligence = Some(Arc::new(registry.load().unwrap()));
        let mut state = f.state.clone();
        state.config = Arc::new(blocked);
        let denied = build_worker_router(state);
        let req = json!({"operation_id":Uuid::new_v4(),"providers":["anthropic"]});
        call(&admin, &uri, Some(req), None, true).await;
        assert_eq!(
            call(
                &denied,
                "/v1/worker/logins",
                Some(json!({})),
                Some(&f.credential),
                false
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let newer = Uuid::new_v4();
    let next = json!({"operation_id":newer,"providers":["anthropic"]});
    assert_eq!(
        call(&admin, &uri, Some(next), None, true).await.0,
        StatusCode::OK
    );
    call(&admin, &uri, Some(request), None, true).await;
    let (_, stale) = call(
        &worker,
        "/v1/worker/logins",
        Some(json!({"generation":op,"applied":true})),
        Some(&f.credential),
        false,
    )
    .await;
    assert!(stale["logins"].as_array().unwrap().is_empty());
    let (_, current) = call(
        &worker,
        "/v1/worker/logins",
        Some(json!({})),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(current["generation"], newer.to_string());
    call(
        &worker,
        "/v1/worker/logins",
        Some(json!({"generation":newer,"applied":true})),
        Some(&f.credential),
        false,
    )
    .await;
    assert!(
        call(
            &worker,
            "/v1/worker/logins",
            Some(json!({})),
            Some(&f.credential),
            false
        )
        .await
        .1["logins"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let pool = f.db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        aq_machine_control::management::hold(&mut conn, node)
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &worker,
            "/v1/worker/logins",
            Some(json!({})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &worker,
            "/api/worker-logins",
            None,
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn remote_worker_checks_are_scoped_idempotent_and_return_results() {
    let f = common::profiled::fixture().await;
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let node = f.node.node_id;
    let path = format!("/api/worker-diagnostics/{node}");
    let id = Uuid::new_v4();
    let request = json!({"operation_id":id});
    assert_eq!(
        call(&admin, &path, Some(request.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&admin, &path, Some(request.clone()), None, true)
            .await
            .1["operation_id"],
        id.to_string()
    );
    assert_eq!(
        call(&admin, &path, Some(request), None, true).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &admin,
            &path,
            Some(json!({"operation_id":Uuid::new_v4()})),
            None,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &worker,
            "/v1/worker/diagnostics",
            Some(json!({})),
            None,
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, job) = call(
        &worker,
        "/v1/worker/diagnostics",
        Some(json!({})),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["operation_id"], id.to_string());
    assert_eq!(job["kind"], "sandbox_startup_v1");
    let malformed = json!({"operation_id":id,"report":{"ok":true,"details":[]}});
    assert_eq!(
        call(
            &worker,
            "/v1/worker/diagnostics",
            Some(malformed),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let report = json!({"ok":false,"phase":"container_start","error":"Synthetic engine diagnostic","details":{"CgroupVersion":"2"}});
    let receipt = json!({"operation_id":id,"report":report});
    assert_eq!(
        call(
            &worker,
            "/v1/worker/diagnostics",
            Some(receipt.clone()),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &worker,
            "/v1/worker/diagnostics",
            Some(receipt),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let public = call(&admin, "/api/worker-diagnostics", None, None, false)
        .await
        .1;
    assert_eq!(public["jobs"][0]["result"], report);
    assert_eq!(public["jobs"][0]["status"], "completed");
    assert_eq!(public["receivers"][0]["node_id"], node.to_string());
    let replay =
        json!({"operation_id":id,"kind":"receiver_update_v1","installer_sha256":"a".repeat(64)});
    assert_eq!(
        call(&admin, &path, Some(replay), None, true).await.0,
        StatusCode::CONFLICT
    );
    let unsupported = json!({"operation_id":Uuid::new_v4(),"kind":"shell","command":"forbidden"});
    assert_ne!(
        call(&admin, &path, Some(unsupported), None, true).await.0,
        StatusCode::OK
    );
    let update = Uuid::new_v4();
    assert_eq!(call(&admin,&path,Some(json!({"operation_id":update,"kind":"receiver_update_v1","installer_sha256":"b".repeat(64)})),None,true).await.0,StatusCode::OK);
    let next = call(
        &worker,
        "/v1/worker/diagnostics",
        Some(json!({})),
        Some(&f.credential),
        false,
    )
    .await
    .1;
    assert_eq!(next["installer_sha256"], "b".repeat(64));
    let pool = f.db.pool().await;
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(node)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &worker,
            "/v1/worker/diagnostics",
            Some(json!({})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn retained_transcript_real_sql_router_contract() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')").bind(project).execute(&pool).await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms,stopped_at_ms) VALUES($1,$2,$3,$4,$5,'stopped','Ended',1,2,2)").bind(id).bind(f.actor).bind(f.node.node_id).bind(project).bind(json!({"provider":"anthropic","model":"fixture"})).execute(&pool).await.unwrap();
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let upload = format!("/v1/worker/manual-agents/{id}/transcript");
    let read = format!("/api/manual-agents/{id}/transcript");
    let (status, initial) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(initial["session"]["archive"]["state"], "not_recorded");
    assert_ne!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":1,"text":"x"})),
            None,
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_ne!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":1,"text":"x","extra":true})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_ne!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":1,"text":"\u{0}"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let chunk = json!({"sequence":1,"text":"é hel"});
    let (status, ack) = call(
        &worker,
        &upload,
        Some(chunk.clone()),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ack}");
    assert_eq!(
        ack,
        json!({"accepted_through":1,"next_sequence":2,"archive_state":"finalizing"})
    );
    assert_eq!(
        call(&worker, &upload, Some(chunk), Some(&f.credential), false)
            .await
            .1,
        ack
    );
    let (status, gap) = call(
        &worker,
        &upload,
        Some(json!({"sequence":3,"text":"bad"})),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(gap["next_sequence"], 2);
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":1,"text":"changed"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":2,"text":"lo world"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let final_body = json!({"final_sequence":2,"coverage":"partial","reason":"fixture"});
    let (status, final_ack) = call(
        &worker,
        &upload,
        Some(final_body.clone()),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(final_ack["archive_state"], "complete");
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(final_body),
            Some(&f.credential),
            false
        )
        .await
        .1,
        final_ack
    );
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"final_sequence":2,"coverage":"complete","reason":null})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":1,"text":"é hel"})),
            Some(&f.credential),
            false
        )
        .await
        .1,
        ack,
        "a late identical retry keeps its original acknowledgement"
    );
    let (_, page) = call(&admin, &format!("{read}?limit=1"), None, None, false).await;
    assert_eq!(page["chunks"], json!([{"sequence":1,"text":"é hel"}]));
    assert_eq!(page["has_more"], true);
    let (_, matches) = call(&admin, &format!("{read}/search?q=HELLO"), None, None, false).await;
    assert_eq!(matches["matches"][0]["offset"], 3);
    assert_eq!(matches["matches"][0]["length"], 5);
    assert_eq!(matches["matches"][0]["sequence"], 1);
    let (_, history) = call(
        &admin,
        "/api/manual-agents/history?status=ended&limit=1",
        None,
        None,
        false,
    )
    .await;
    assert_eq!(history["sessions"][0]["id"], id.to_string());
    assert_eq!(history["sessions"][0]["archive"]["coverage"], "partial");
    // Schema guards survive direct SQL and archive paging extends beyond the fleet cap.
    assert!(
        sqlx::query(
            "INSERT INTO manual_transcript_chunks(session_id,sequence,text,ack_state) VALUES($1,0,'invalid','recording')"
        )
        .bind(id)
        .execute(&pool)
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE manual_transcript_archives SET bytes=67108865 WHERE session_id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .is_err()
    );
    let ids: Vec<Uuid> = (0..260).map(|_| Uuid::new_v4()).collect();
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms,stopped_at_ms) SELECT x,$2,$3,$4,'{}','stopped','Ended',1,1,1 FROM unnest($1::uuid[]) x").bind(&ids).bind(f.actor).bind(f.node.node_id).bind(project).execute(&pool).await.unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut cursor = None;
    loop {
        let path = match cursor {
            Some(ref c) => format!("/api/manual-agents/history?limit=100&cursor={c}"),
            None => "/api/manual-agents/history?limit=100".to_string(),
        };
        let (status, page) = call(&admin, &path, None, None, false).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for item in page["sessions"].as_array().unwrap() {
            assert!(seen.insert(item["id"].as_str().unwrap().to_owned()));
        }
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 261);
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_ne!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":3,"text":"no"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&admin, &read, None, None, false).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn retained_transcript_owner_fences_limits_and_unicode_offsets() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')").bind(project).execute(&pool).await.unwrap();
    let other_node = Uuid::new_v4();
    let other_credential = aq_server::tokens::new_node_credential();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'archive-other',$2,'0.1.0',1,now())")
        .bind(other_node).bind(aq_server::tokens::verifier(&other_credential)).execute(&pool).await.unwrap();
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    // Prove the second credential authenticates as a different enrolled worker.
    let (status, response) = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(json!({"max_slots":1,"repositories":[]})),
        Some(&other_credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let id = Uuid::new_v4();
    let empty_id = Uuid::new_v4();
    let full_id = Uuid::new_v4();
    for session_id in [id, empty_id, full_id] {
        sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms,stopped_at_ms) VALUES($1,$2,$3,$4,'{}','stopped','Ended',1,2,2)")
            .bind(session_id).bind(f.actor).bind(f.node.node_id).bind(project).execute(&pool).await.unwrap();
    }
    let upload = format!("/v1/worker/manual-agents/{id}/transcript");
    let read = format!("/api/manual-agents/{id}/transcript");
    for payload in [
        json!({"sequence":1,"text":"intrusion"}),
        json!({"final_sequence":0,"coverage":"complete","reason":null}),
    ] {
        assert_eq!(
            call(
                &worker,
                &upload,
                Some(payload),
                Some(&other_credential),
                false
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM manual_transcript_archives WHERE session_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    for (sequence, text) in [(1, "İ"), (2, "I HEL"), (3, "LO")] {
        let (status, response) = call(
            &worker,
            &upload,
            Some(json!({"sequence":sequence,"text":text})),
            Some(&f.credential),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{response}");
    }
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"final_sequence":3,"coverage":"complete","reason":null})),
            Some(&other_credential),
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, hits) = call(
        &admin,
        &format!("{read}/search?q=i&limit=1"),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{hits}");
    assert_eq!(hits["matches"][0]["offset"], 0);
    assert_eq!(hits["matches"][0]["length"], 2);
    assert_eq!(hits["matches"][0]["sequence"], 1);
    assert_eq!(hits["next_after"], 0);
    assert_eq!(hits["has_more"], true);
    let (_, second) = call(
        &admin,
        &format!("{read}/search?q=i&limit=1&after=0"),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(second["matches"][0]["offset"], 2);
    assert_eq!(second["matches"][0]["length"], 1);
    assert_eq!(second["matches"][0]["sequence"], 2);
    let (_, cross) = call(&admin, &format!("{read}/search?q=hello"), None, None, false).await;
    assert_eq!(cross["matches"][0]["offset"], 4);
    assert_eq!(cross["matches"][0]["length"], 5);
    assert_eq!(cross["matches"][0]["sequence"], 2);
    // More requests than search slots still complete while unrelated reads remain usable.
    let search_path = format!("{read}/search?q=hello");
    let (a, b, c, d, ordinary) = tokio::join!(
        call(&admin, &search_path, None, None, false),
        call(&admin, &search_path, None, None, false),
        call(&admin, &search_path, None, None, false),
        call(&admin, &search_path, None, None, false),
        call(&admin, &read, None, None, false),
    );
    for (status, response) in [a, b, c, d] {
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["matches"][0]["offset"], 4);
    }
    assert_eq!(ordinary.0, StatusCode::OK);
    assert_eq!(
        call(&admin, &format!("{read}?through=4"), None, None, false)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &admin,
            &format!("{read}/search?q=hello&through=4"),
            None,
            None,
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &admin,
            &format!("/api/manual-agents/history?provider={}", "x".repeat(201)),
            None,
            None,
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &admin,
            &format!("/api/manual-agents/history?model={}", "x".repeat(201)),
            None,
            None,
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let empty_upload = format!("/v1/worker/manual-agents/{empty_id}/transcript");
    let empty_read = format!("/api/manual-agents/{empty_id}/transcript");
    assert_eq!(
        call(
            &worker,
            &empty_upload,
            Some(json!({"sequence":1,"text":""})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &worker,
            &empty_upload,
            Some(json!({"final_sequence":0,"coverage":"complete","reason":null})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, empty) = call(&admin, &empty_read, None, None, false).await;
    assert_eq!(empty["session"]["archive"]["state"], "complete");
    assert_eq!(empty["session"]["archive"]["bytes"], 0);
    assert_eq!(empty["chunks"], json!([]));
    assert_eq!(empty["through_sequence"], 0);
    let (_, no_hits) = call(
        &admin,
        &format!("{empty_read}/search?q=x"),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(no_hits["matches"], json!([]));
    assert_eq!(no_hits["next_after"], Value::Null);
    let full_upload = format!("/v1/worker/manual-agents/{full_id}/transcript");
    assert_eq!(
        call(
            &worker,
            &full_upload,
            Some(json!({"sequence":1,"text":"x".repeat(32769)})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &worker,
            &full_upload,
            Some(json!({"sequence":1,"text":"x".repeat(32768)})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    // Set the accounting field at the cap to test enforcement without allocating 64 MiB.
    sqlx::query("UPDATE manual_transcript_archives SET bytes=67108864 WHERE session_id=$1")
        .bind(full_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &worker,
            &full_upload,
            Some(json!({"sequence":2,"text":"x"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &worker,
            &full_upload,
            Some(json!({"final_sequence":1,"coverage":"partial","reason":"size limit"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    sqlx::query("INSERT INTO worker_settings(kind,id,removed,revision) VALUES('node',$1,true,1)")
        .bind(f.node.node_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&admin, &read, None, None, false).await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, retained) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retained["chunks"].as_array().unwrap().len(), 3);
    assert_eq!(
        call(&admin, &format!("{read}/search?q=hello"), None, None, false)
            .await
            .1["matches"][0]["offset"],
        4
    );
    assert_ne!(
        call(
            &worker,
            &upload,
            Some(json!({"sequence":4,"text":"no"})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
}
