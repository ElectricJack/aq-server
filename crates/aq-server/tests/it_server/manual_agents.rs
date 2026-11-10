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
