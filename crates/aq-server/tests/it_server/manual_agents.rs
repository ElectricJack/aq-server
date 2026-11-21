//! Real SQL and production auth/router boundaries; no model invocation.
use crate::common;
use aq_profiles::roles::Kind;
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
    let node_change = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"label":"Build worker","max_agents":3});
    assert_eq!(
        call(&admin, &node_path, Some(node_change.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, receipt) = call(&admin, &node_path, Some(node_change.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["revision"], 2);
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
    let remove = json!({"operation_id":Uuid::new_v4(),"expected_revision":2,"label":"Build worker","max_agents":3,"remove":true});
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
    // The level a manual session and the supervisor choose from: one
    // candidate per logged-in harness.
    let level = |rows: &str| {
        format!(
            "---\nid: manual-model\nname: Standard\ndescription: Manual fixture level.\n---\n\
             ## Candidates\n| provider | model | reasoning | harnesses |\n|---|---|---|---|\n{rows}"
        )
    };
    let claude_row = "| anthropic | fixture-model | medium | claude |\n";
    let codex_row = "| openai | gpt-test | high | codex |\n";
    let mut execution = common::roles::execution(8192);
    execution = execution
        .replace("harnesses: [claude,", "harnesses: [claude, codex,")
        .replace(
            "- claude: account-a\n",
            "- claude: account-a\n- codex: account-a\n",
        );
    common::roles::save(&pool, f.actor, Kind::Execution, "sandbox", &execution).await;
    common::roles::save(
        &pool,
        f.actor,
        Kind::Intelligence,
        "manual-model",
        &level(&format!("{claude_row}{codex_row}")),
    )
    .await;
    let snapshot_digest = common::roles::active(&pool).await.digest().to_owned();
    let binding = json!({"enabled":true,"name":"manual fixture","repository_id":repo,"default_intelligence":"manual-model","allowed_node_ids":[],"allowed_worker_groups":[]});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')").bind(project).bind(binding).execute(&pool).await.unwrap();
    let mut config = (*f.state.config).clone();
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
    let control = json!({"operation_id":Uuid::new_v4(),"expected_revision":0,"enabled":true,"intelligence_profile":{"profile_id":"manual-model","candidate_id":"openai:gpt-test:high"}});
    assert_eq!(
        call(&admin, &uri, Some(control.clone()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (status, receipt) = call(&admin, &uri, Some(control.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    // The saved profile is the level choice, and its admission class is the
    // level's id on the candidate's provider.
    assert_eq!(
        receipt["profile"],
        json!({"profile_id":"manual-model","candidate_id":"openai:gpt-test:high","name":"Standard","provider":"openai","model":"gpt-test","reasoning":"high","harness":"codex"})
    );
    let class = aq_swarm_settings::admission::pinned_cell(&receipt["profile"]).unwrap();
    assert_eq!(
        (class.provider.as_str(), class.intelligence_class.as_str()),
        ("openai", "manual-model")
    );
    assert_eq!(
        call(&admin, &uri, Some(control.clone()), None, true)
            .await
            .1,
        receipt
    );
    // The stored choice names no snapshot: every read and every start
    // resolves it against the active one. While the level does not list the
    // candidate, or does not exist, the control stays enabled with its
    // profile and says why the supervisor is not running.
    let entry = |fleet: &Value| {
        let all = fleet["supervisors"].as_array().unwrap();
        all.iter()
            .find(|s| s["id"] == conversation)
            .unwrap()
            .clone()
    };
    let read = || async { entry(&call(&admin, "/api/supervisors", None, None, false).await.1) };
    assert!(read().await.get("unavailable_reason").is_none());
    assert_eq!(
        aq_supervisor_control::resolve_stored(&pool, &receipt["profile"])
            .await
            .ok()
            .unwrap(),
        Ok(snapshot_digest.clone())
    );
    common::roles::save(
        &pool,
        f.actor,
        Kind::Intelligence,
        "manual-model",
        &level(claude_row),
    )
    .await;
    let unlisted = read().await;
    assert_eq!(
        unlisted["unavailable_reason"],
        "its level `manual-model` no longer lists gpt-test on codex; choose a level"
    );
    assert_eq!(unlisted["enabled"], true);
    assert_eq!(unlisted["profile"], receipt["profile"]);
    assert!(
        aq_supervisor_control::resolve_stored(&pool, &receipt["profile"])
            .await
            .ok()
            .unwrap()
            .is_err()
    );
    // Once the level lists the candidate again it resolves, with no owner
    // action, against the snapshot that is active now.
    common::roles::save(
        &pool,
        f.actor,
        Kind::Intelligence,
        "manual-model",
        &level(&format!("{claude_row}{codex_row}")),
    )
    .await;
    assert!(read().await.get("unavailable_reason").is_none());
    assert_eq!(
        aq_supervisor_control::resolve_stored(&pool, &receipt["profile"])
            .await
            .ok()
            .unwrap(),
        Ok(common::roles::active(&pool).await.digest().to_owned())
    );
    // A carried choice whose level has no document (migration 0134 keeps an
    // old profile id that is no level).
    sqlx::query("UPDATE supervisor_controls SET profile=jsonb_set(profile,'{profile_id}','\"engineering\"') WHERE conversation_id=$1::uuid")
        .bind(conversation)
        .execute(&pool)
        .await
        .unwrap();
    let gone = read().await;
    assert_eq!(
        gone["unavailable_reason"],
        "its level `engineering` no longer exists; choose a level"
    );
    assert_eq!(gone["enabled"], true);
    sqlx::query("UPDATE supervisor_controls SET profile=$2 WHERE conversation_id=$1::uuid")
        .bind(conversation)
        .bind(&receipt["profile"])
        .execute(&pool)
        .await
        .unwrap();
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
    let selected = json!({"profile_id":"manual-model","candidate_id":"openai:gpt-test:high"});
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
    // Manual sessions list the levels of the active snapshot: one entry per
    // level candidate a logged-in harness can run.
    let offered: Vec<&Value> = (choices["launch_profiles"].as_array().unwrap().iter())
        .filter(|c| c["profile_id"] == "manual-model")
        .collect();
    assert_eq!(offered.len(), 2, "{choices}");
    assert_eq!(
        offered[1],
        &json!({"snapshot_digest":snapshot_digest,"profile_id":"manual-model","candidate_id":"openai:gpt-test:high","name":"Standard","provider":"openai","model":"gpt-test","reasoning":"high","harness":"codex"})
    );
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
    // The launch pins its level (the admission class) and the snapshot.
    assert_eq!(chosen["spec"]["intelligence_class"], "manual-model");
    assert_eq!(chosen["spec"]["harness"], "codex");
    assert_eq!(chosen["spec"]["snapshot_digest"], snapshot_digest);
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
    changed["intelligence_profile"]["candidate_id"] = json!("anthropic:fixture-model:medium");
    assert_eq!(
        call(&admin, &selected_launch, Some(changed), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut stale = selected_body.clone();
    stale["operation_id"] = json!(Uuid::new_v4());
    stale["intelligence_profile"]["profile_id"] = json!("no-such-level");
    assert_eq!(
        call(&admin, &selected_launch, Some(stale), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    // The level loses its Codex candidate.
    common::roles::save(
        &pool,
        f.actor,
        Kind::Intelligence,
        "manual-model",
        &level(claude_row),
    )
    .await;
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
            "manual",
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
    environment: (&str, &sqlx::PgPool, &str),
    id: Uuid,
    client: Uuid,
    generation: u64,
) {
    use futures_util::{SinkExt, StreamExt};
    // The subject kind: a hand-started agent, a worker host shell or a queued attempt.
    let (origin, pool, kind) = environment;
    let (human_prefix, worker_prefix, node_query) = match kind {
        "host" => (
            "/api/worker-terminals/shells",
            "/v1/worker/host-terminals",
            "SELECT node_id FROM worker_host_shells WHERE id=$1",
        ),
        "attempt" => (
            "/api/attempt-terminals",
            "/v1/worker/attempt-terminals",
            "SELECT node_id FROM execution_attempts WHERE attempt_id=$1",
        ),
        _ => (
            "/api/manual-agents",
            "/v1/worker/manual-agents",
            "SELECT node_id FROM manual_agent_sessions WHERE id=$1",
        ),
    };
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
    let worker_url = format!("ws://127.0.0.1:{wp}{worker_prefix}/{id}/terminal-stream");
    assert!(
        connect_async(&worker_url).await.is_err(),
        "worker stream requires enrolled authentication"
    );
    let mut request = worker_url.into_client_request().unwrap();
    let mut bearer = axum::http::HeaderValue::from_str(&format!("Bearer {credential}")).unwrap();
    bearer.set_sensitive(true);
    request.headers_mut().insert("authorization", bearer);
    let (mut node, _) = connect_async(request).await.unwrap();
    // A queued attempt's pane keeps its launch geometry, so its viewer sends none.
    let fixed = kind == "attempt";
    let geometry = if fixed { "" } else { "&columns=101&rows=37" };
    let url = format!(
        "ws://127.0.0.1:{ap}{human_prefix}/{id}/terminal/live?client_id={client}&generation={generation}{geometry}"
    );
    if fixed {
        let mut sized = format!("{url}&columns=101&rows=37")
            .into_client_request()
            .unwrap();
        sized
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        assert!(
            connect_async(sized).await.is_err(),
            "an attempt viewer cannot choose the pane geometry"
        );
    }
    if fixed {
        // A read-only viewer of an attempt pane never resizes it: the frame ends
        // its attach and only a detach reaches the worker.
        let mut watching = format!("{url}&read_only=true")
            .into_client_request()
            .unwrap();
        watching
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        let (mut viewer, _) = connect_async(watching).await.unwrap();
        let attach: Value =
            serde_json::from_str(&stream_frame(&mut node).await.into_text().unwrap()).unwrap();
        assert_eq!(attach["kind"], "attach");
        assert_eq!(attach["read_only"], true);
        let watching_connection: Uuid =
            serde_json::from_value(attach["connection_id"].clone()).unwrap();
        node.send(Message::Text(
            json!({"kind":"ready","connection_id":watching_connection,"columns":120,"rows":40})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&stream_frame(&mut viewer).await.into_text().unwrap())
                .unwrap()["kind"],
            "ready"
        );
        viewer
            .send(Message::Text(
                json!({"kind":"resize","columns":100,"rows":30})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let detached: Value = serde_json::from_str(
            &tokio::time::timeout(std::time::Duration::from_secs(3), stream_frame(&mut node))
                .await
                .unwrap()
                .into_text()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(detached["kind"], "detach", "a read-only resize is refused");
        assert_eq!(detached["connection_id"], json!(watching_connection));
    }
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
    if fixed {
        assert!(attach.get("columns").is_none() && attach.get("rows").is_none());
        assert_eq!(attach["read_only"], false);
    } else {
        assert_eq!(attach["columns"], 101);
        assert_eq!(attach["rows"], 37);
    }
    let connection: Uuid = serde_json::from_value(attach["connection_id"].clone()).unwrap();
    // The worker reports the pane's own geometry when the attach is ready.
    node.send(Message::Text(
        json!({"kind":"ready","connection_id":connection,"columns":120,"rows":40})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ready: Value =
        serde_json::from_str(&stream_frame(&mut browser).await.into_text().unwrap()).unwrap();
    assert_eq!(ready["kind"], "ready");
    if fixed {
        assert_eq!(
            (&ready["columns"], &ready["rows"]),
            (&json!(120), &json!(40))
        );
    }
    // A viewer of a queued attempt's pane is recorded for the stall ladder.
    let viewers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM attempt_terminal_viewers WHERE attempt_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(viewers, i64::from(kind == "attempt"));
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
    if fixed {
        // An attempt viewer asks for a fresh paint instead of resizing the pane.
        browser
            .send(Message::Text(json!({"kind":"repaint"}).to_string().into()))
            .await
            .unwrap();
        let repaint: Value =
            serde_json::from_str(&stream_frame(&mut node).await.into_text().unwrap()).unwrap();
        assert_eq!(repaint["kind"], "repaint");
        assert_eq!(repaint["connection_id"], json!(connection));
        // The worker re-opens the pane and reports ready, ending the paint.
        node.send(Message::Text(
            json!({"kind":"ready","connection_id":connection,"columns":120,"rows":40})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let painted = stream_frame(&mut browser).await.into_text().unwrap();
        assert!(painted.contains("ready"));
        // After ready, its current controller may resize the pane. The server adds
        // the checked connection and generation to the worker relay frame.
        browser
            .send(Message::Text(
                json!({"kind":"resize","columns":100,"rows":30})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let resized: Value =
            serde_json::from_str(&stream_frame(&mut node).await.into_text().unwrap()).unwrap();
        assert_eq!(
            resized,
            json!({"kind":"resize","columns":100,"rows":30,
                "connection_id":connection,"generation":generation})
        );
    } else {
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
    }
    let other = Uuid::new_v4();
    let takeover = call(
        admin,
        &format!("{human_prefix}/{id}/terminal/connect"),
        Some(json!({"client_id":other})),
        None,
        true,
    )
    .await;
    assert_eq!(takeover.0, StatusCode::OK);
    if fixed {
        // A stale controller cannot resize either.
        browser
            .send(Message::Text(
                json!({"kind":"resize","columns":90,"rows":31})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    } else {
        browser
            .send(Message::Binary(b"never-forward".as_slice().into()))
            .await
            .unwrap();
    }
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
    let node_id: Uuid = sqlx::query_scalar(node_query)
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
    // A Codex login is never pushed to a node: its sessions lease it.
    for providers in [json!(["openai"]), json!(["anthropic", "openai"])] {
        let codex = json!({"operation_id":Uuid::new_v4(),"providers":providers});
        assert_eq!(
            call(&admin, &uri, Some(codex), None, true).await.0,
            StatusCode::BAD_REQUEST
        );
    }
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
async fn worker_host_shell_is_scoped_separate_persistent_and_fenced() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let node = f.node.node_id;
    let id = Uuid::new_v4();
    let path = format!("/api/worker-terminals/{node}");
    assert_eq!(
        call(&admin, &path, Some(json!({"operation_id":id})), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&admin, &path, Some(json!({"operation_id":id})), None, true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &worker,
            "/v1/worker/host-terminals",
            Some(json!({})),
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
            "/v1/worker/host-terminals",
            Some(json!({})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, opened) = call(&admin, &path, Some(json!({"operation_id":id})), None, true).await;
    assert_eq!(status, StatusCode::OK, "{opened}");
    assert_eq!(opened["shell"]["id"], id.to_string());
    assert_eq!(
        call(
            &admin,
            &path,
            Some(json!({"operation_id":Uuid::new_v4()})),
            None,
            true
        )
        .await
        .1["shell"]["id"],
        id.to_string()
    );
    assert_eq!(
        call(&admin, &path, None, None, false).await.1["shell"]["id"],
        id.to_string()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM manual_agent_sessions")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    let receipt = json!({"id":id,"status":"running"});
    assert_eq!(
        call(
            &worker,
            "/v1/worker/host-terminals",
            Some(receipt),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::OK
    );
    let client = Uuid::new_v4();
    let terminal = format!("/api/worker-terminals/shells/{id}/terminal/connect");
    let (_, control) = call(
        &admin,
        &terminal,
        Some(json!({"client_id":client})),
        None,
        true,
    )
    .await;
    live_stream_preserves_bytes_and_fences_input(
        &admin,
        &worker,
        &f.credential,
        (
            f.state.config.server.public_origin.trim_end_matches('/'),
            &pool,
            "host",
        ),
        id,
        client,
        control["generation"].as_u64().unwrap(),
    )
    .await;
    let stopped = call(
        &admin,
        &format!("/api/worker-terminals/shells/{id}/stop"),
        Some(json!({"operation_id":Uuid::new_v4()})),
        None,
        true,
    )
    .await;
    assert_eq!(stopped.0, StatusCode::OK);
    assert_eq!(stopped.1["shell"]["desired_running"], false);
    assert_eq!(
        call(
            &admin,
            &terminal,
            Some(json!({"client_id":client})),
            None,
            true
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    call(
        &worker,
        "/v1/worker/host-terminals",
        Some(json!({"id":id,"status":"ended"})),
        Some(&f.credential),
        false,
    )
    .await;
    let new = Uuid::new_v4();
    assert_eq!(
        call(&admin, &path, Some(json!({"operation_id":new})), None, true)
            .await
            .1["shell"]["id"],
        new.to_string()
    );
}

/// Provider/class admission uses real SQL locks and committed preparation
/// reservations across workers, with no model process involved.
#[tokio::test]
async fn swarm_parallel_admission_reserves_once_and_failure_releases_capacity() {
    use aq_swarm_settings::admission::{self, CEILING, Cell};
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')")
        .bind(project).execute(&pool).await.unwrap();
    let cell = Cell {
        provider: "openai".into(),
        intelligence_class: "complex".into(),
    };
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"complex","limit":1}]))
        .execute(&pool)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let id = Uuid::new_v4();
        ids.push(id);
        sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'pending','Waiting for a slot',1,1)")
            .bind(id).bind(f.actor).bind(f.node.node_id).bind(project).bind(json!({"provider":"openai","intelligence_class":"complex"})).execute(&pool).await.unwrap();
    }
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for id in &ids {
        let pool = pool.clone();
        let cell = cell.clone();
        let barrier = barrier.clone();
        let id = *id;
        tasks.push(tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            barrier.wait().await;
            let refused = admission::check(&mut tx, Some(&cell)).await.unwrap();
            if refused.is_none() {
                sqlx::query("UPDATE manual_agent_sessions SET status='preparing' WHERE id=$1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
            }
            tx.commit().await.unwrap();
            (id, refused)
        }));
    }
    let a = tasks.remove(0).await.unwrap();
    let b = tasks.remove(0).await.unwrap();
    assert_eq!([a.1, b.1].iter().filter(|r| r.is_none()).count(), 1);
    assert!([a.1, b.1].contains(&Some(CEILING)));
    let running = if a.1.is_none() { a.0 } else { b.0 };
    let mut conn = pool.acquire().await.unwrap();
    let counts = admission::counts(&mut conn).await.unwrap();
    let count = counts
        .iter()
        .find(|c| c.provider == cell.provider && c.intelligence_class == cell.intelligence_class)
        .unwrap();
    assert_eq!((count.running, count.waiting), (1, 1));
    // Lowering to zero holds new work without touching a running reservation.
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"complex","limit":0}]))
        .execute(&mut *conn)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        admission::check(&mut tx, Some(&cell)).await.unwrap(),
        Some(CEILING)
    );
    tx.rollback().await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM manual_agent_sessions WHERE id=$1")
        .bind(running)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(status, "preparing");
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"complex","limit":1}]))
        .execute(&mut *conn)
        .await
        .unwrap();
    // Repeated polls of one queued task count once, and a stale observation
    // expires without leaving a permanent wait behind.
    let waiting_task = Uuid::new_v4();
    let mut wait_tx = pool.begin().await.unwrap();
    assert_eq!(
        admission::observe(&mut wait_tx, waiting_task, Some(&cell), 10_000)
            .await
            .unwrap(),
        Some(CEILING)
    );
    assert_eq!(
        admission::observe(&mut wait_tx, waiting_task, Some(&cell), 10_000)
            .await
            .unwrap(),
        Some(CEILING)
    );
    wait_tx.commit().await.unwrap();
    assert_eq!(admission::counts_at(&mut conn,10_001).await.unwrap().iter().find(|c|c.provider==cell.provider && c.intelligence_class==cell.intelligence_class).unwrap().waiting,2);
    assert_eq!(admission::counts_at(&mut conn,70_001).await.unwrap().iter().find(|c|c.provider==cell.provider && c.intelligence_class==cell.intelligence_class).unwrap().waiting,1);
    admission::clear_waiting(&mut conn, waiting_task)
        .await
        .unwrap();
    // Cancelled/deleted tasks stop polling. Later observations reclaim their
    // expired rows in bounded batches while retaining fresh waits.
    sqlx::query("INSERT INTO swarm_waiting(task_id,provider,intelligence_class,reason,observed_at_ms,expires_at_ms) SELECT gen_random_uuid(),'openai','complex',$1,1,1 FROM generate_series(1,129)")
        .bind(CEILING).execute(&mut *conn).await.unwrap();
    let fresh_wait = Uuid::new_v4();
    sqlx::query("INSERT INTO swarm_waiting(task_id,provider,intelligence_class,reason,observed_at_ms,expires_at_ms) VALUES($1,'openai','complex',$2,70000,1000000)")
        .bind(fresh_wait).bind(CEILING).execute(&mut *conn).await.unwrap();
    let mut cleanup_tx = pool.begin().await.unwrap();
    admission::observe(&mut cleanup_tx, waiting_task, Some(&cell), 70_001)
        .await
        .unwrap();
    let expired: i64 =
        sqlx::query_scalar("SELECT count(*) FROM swarm_waiting WHERE expires_at_ms<=70001")
            .fetch_one(&mut *cleanup_tx)
            .await
            .unwrap();
    assert_eq!(
        expired, 1,
        "one observation reclaims at most 128 expired waits"
    );
    admission::observe(&mut cleanup_tx, waiting_task, Some(&cell), 70_001)
        .await
        .unwrap();
    let expired: i64 =
        sqlx::query_scalar("SELECT count(*) FROM swarm_waiting WHERE expires_at_ms<=70001")
            .fetch_one(&mut *cleanup_tx)
            .await
            .unwrap();
    assert_eq!(expired, 0);
    let fresh: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM swarm_waiting WHERE task_id=$1)")
            .bind(fresh_wait)
            .fetch_one(&mut *cleanup_tx)
            .await
            .unwrap();
    assert!(fresh, "cleanup preserves unexpired observations");
    cleanup_tx.commit().await.unwrap();
    admission::clear_waiting(&mut conn, waiting_task)
        .await
        .unwrap();
    admission::clear_waiting(&mut conn, fresh_wait)
        .await
        .unwrap();
    // Launch failure releases exactly the preparing session's occupied slot.
    sqlx::query("UPDATE manual_agent_sessions SET status='failed',stopped_at_ms=2 WHERE id=$1")
        .bind(running)
        .execute(&mut *conn)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(admission::check(&mut tx, Some(&cell)).await.unwrap(), None);
    // A failed publication rolls back the tentative reservation and its lock.
    sqlx::query(
        "UPDATE manual_agent_sessions SET status='preparing' WHERE id=$1 AND status='pending'",
    )
    .bind(if a.1.is_some() { a.0 } else { b.0 })
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(admission::check(&mut tx, Some(&cell)).await.unwrap(), None);
    tx.rollback().await.unwrap();
    // Native publication uses SERIALIZABLE. Its earlier snapshot cannot miss a
    // manual admission committed while it waited: the settings row version
    // forces a retry even though the user-facing revision stays unchanged.
    drop(conn);
    let mut stale = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *stale)
        .await
        .unwrap();
    sqlx::query("SELECT revision FROM swarm_settings")
        .execute(&mut *stale)
        .await
        .unwrap();
    let mut winner = pool.begin().await.unwrap();
    assert_eq!(
        admission::check(&mut winner, Some(&cell)).await.unwrap(),
        None
    );
    winner.commit().await.unwrap();
    let error = admission::check(&mut stale, Some(&cell)).await.unwrap_err();
    assert!(
        matches!(error,sqlx::Error::Database(ref error) if error.code().as_deref()==Some("40001"))
    );
    stale.rollback().await.unwrap();
}

#[test]
fn swarm_class_is_the_level_a_stored_choice_pins() {
    use aq_swarm_settings::admission::pinned_cell;
    // A manual session's spec pins `intelligence_class`; a supervisor's saved
    // profile names its level as `profile_id`.
    let spec = json!({"provider":"openai","model":"same-model","intelligence_class":"simple-low"});
    assert_eq!(pinned_cell(&spec).unwrap().intelligence_class, "simple-low");
    let profile = json!({"provider":"openai","model":"same-model","profile_id":"complex-high"});
    let cell = pinned_cell(&profile).unwrap();
    assert_eq!(
        (cell.provider.as_str(), cell.intelligence_class.as_str()),
        ("openai", "complex-high")
    );
    assert!(pinned_cell(&json!({"provider":"openai"})).is_none());
    assert!(pinned_cell(&json!({"intelligence_class":"simple-low"})).is_none());
}

#[tokio::test]
async fn swarm_manual_poll_explains_zero_limit_and_rejects_report_admission_bypass() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{\"enabled\":true}',1,'{}')").bind(project).execute(&pool).await.unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'pending','Waiting',1,1)").bind(id).bind(f.actor).bind(f.node.node_id).bind(project).bind(json!({"provider":"openai","intelligence_class":"complex"})).execute(&pool).await.unwrap();
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"complex","limit":0}]))
        .execute(&pool)
        .await
        .unwrap();
    let worker = build_worker_router(f.state.clone());
    let report = json!({"max_slots":2,"repositories":[],"providers":["openai"],"reports":[{"id":id,"status":"preparing","stage":"Bypass"}]});
    let (status, body) = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(report),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["sessions"].as_array().unwrap().is_empty());
    let saved: (String, String) =
        sqlx::query_as("SELECT status,stage FROM manual_agent_sessions WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        saved,
        (
            "pending".into(),
            aq_swarm_settings::admission::CEILING.into()
        )
    );
    sqlx::query("UPDATE swarm_settings SET normal=$1 WHERE singleton")
        .bind(json!([{"provider":"openai","intelligence_class":"complex","limit":1}]))
        .execute(&pool)
        .await
        .unwrap();
    let poll = json!({"max_slots":2,"repositories":[],"providers":["openai"],"reports":[]});
    let (status, body) = call(
        &worker,
        "/v1/worker/manual-agents",
        Some(poll),
        Some(&f.credential),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessions"][0]["status"], "preparing");
}

#[tokio::test]
async fn swarm_provider_ceiling_counts_other_classes_and_unclassified_reservations() {
    use aq_swarm_settings::admission::{self, CEILING, Cell};
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')")
        .bind(project).execute(&pool).await.unwrap();
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(json!({"provider_limits":[{"provider":"openai","limit":2}],"cell_limits":[{"provider":"openai","intelligence_class":"complex","limit":5}]}))
        .execute(&pool).await.unwrap();
    let mut ids = Vec::new();
    for spec in [
        json!({"provider":"openai","intelligence_class":"simple"}),
        json!({"provider":"openai"}),
    ] {
        let id = Uuid::new_v4();
        ids.push(id);
        sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'preparing','Preparing',1,1)")
            .bind(id).bind(f.actor).bind(f.node.node_id).bind(project).bind(spec).execute(&pool).await.unwrap();
    }
    let cell = Cell {
        provider: "openai".into(),
        intelligence_class: "complex".into(),
    };
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        admission::check(&mut tx, Some(&cell)).await.unwrap(),
        Some(CEILING)
    );
    // Remove the unclassified reservation; other-class usage still counts,
    // while one remaining slot admits this class below its own ceiling.
    sqlx::query("UPDATE manual_agent_sessions SET stopped_at_ms=2,status='failed' WHERE id=$1")
        .bind(ids[1])
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(admission::check(&mut tx, Some(&cell)).await.unwrap(), None);
    sqlx::query("UPDATE swarm_settings SET limits=$1 WHERE singleton")
        .bind(json!({"provider_limits":[{"provider":"openai","limit":0}]}))
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        admission::check(&mut tx, Some(&cell)).await.unwrap(),
        Some(CEILING)
    );
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manual_agent_sessions WHERE stopped_at_ms IS NULL",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(live, 1, "lowering ceilings preserves existing reservations");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn node_slot_default_setting_is_revisioned_idempotent_and_does_not_resize_nodes() {
    let f = common::profiled::fixture().await;
    let app = build_admin_router(f.state.clone());
    let path = "/api/worker-management/default/node-slots";
    let change = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"max_agents":5});
    let (status, receipt) = call(&app, path, Some(change.clone()), None, true).await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert_eq!(call(&app, path, Some(change), None, true).await.1, receipt);
    let stale = json!({"operation_id":Uuid::new_v4(),"expected_revision":1,"max_agents":8});
    assert_eq!(
        call(&app, path, Some(stale), None, true).await.0,
        StatusCode::CONFLICT
    );
    let (_, list) = call(&app, "/api/worker-management", None, None, false).await;
    assert_eq!(list["node_slot_default"]["slots"], 5);
    assert_eq!(list["node_slot_default"]["revision"], 2);
    let pool = f.db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        aq_machine_control::slots::count(&mut conn, f.node.node_id)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn native_conversation_is_independent_and_requires_the_original_worker() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let id = Uuid::new_v4();
    let project = f.selection.lineage.project_id;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,'{}',1,'{}')")
        .bind(project).execute(&pool).await.unwrap();
    let other = Uuid::new_v4();
    let other_credential = aq_server::tokens::new_node_credential();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'conversation-other',$2,'0.1.0',1,now())")
        .bind(other).bind(aq_server::tokens::verifier(&other_credential)).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms,stopped_at_ms) VALUES($1,$2,$3,$4,'{}','stopped','Ended',1,2,2)")
        .bind(id).bind(f.actor).bind(f.node.node_id).bind(project).execute(&pool).await.unwrap();
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let read = format!("/api/manual-agents/{id}/conversation");
    let upload = format!("/v1/worker/manual-agents/{id}/conversation");
    let snapshot = json!({"messages":[
        {"role":"user","text":"Show the result"},
        {"role":"assistant","text":"Résumé\nSecond line"},
    ],"partial":false});

    let (status, empty) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        empty,
        json!({"session_id":id,"project_id":project,"conversation":null})
    );
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(snapshot.clone()),
            Some(&other_credential),
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        let (status, receipt) = call(
            &worker,
            &upload,
            Some(snapshot.clone()),
            Some(&f.credential),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt, json!({"session_id":id}));
    }
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(json!({"messages":[{"role":"assistant","text":"Changed"}],"partial":false})),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (status, saved) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        saved,
        json!({"session_id":id,"project_id":project,"conversation":snapshot})
    );
    let (status, history) = call(
        &admin,
        "/api/manual-agents/history?limit=1",
        None,
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    assert_eq!(history["sessions"][0]["id"], json!(id));
    assert_eq!(history["sessions"][0]["conversation_available"], true);
    let (status, details) = call(
        &admin,
        &format!("/api/manual-agents/{id}"),
        None,
        None,
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(details["id"], json!(id));
    assert_eq!(details["status"], "stopped");
}

#[tokio::test]
async fn queued_conversation_is_bound_to_the_stopped_attempt_and_read_by_session() {
    use common::native_started::{context, setup, stop_request};

    let (f, started) = setup(true).await;
    let auth = context(&f).await;
    let attempt = started.binding.attempt_id;
    let aq_session_protocol::SessionIdentity::Remote { session_id: id, .. } =
        started.binding.session
    else {
        panic!("fixture needs a remote session");
    };
    let project = f.selection.lineage.project_id;
    let read = format!("/api/v1/sessions/{id}/conversation");
    let upload = format!("/v1/worker/attempts/{attempt}/conversation");
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let snapshot = json!({"messages":[
        {"role":"user","text":"Show the result"},
        {"role":"assistant","text":"Résumé\nSecond line"},
    ],"partial":false});

    let (status, empty) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(
        empty,
        json!({"session_id":id,"project_id":project,"conversation":null})
    );
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(snapshot.clone()),
            Some(&f.credential),
            false
        )
        .await
        .0,
        StatusCode::CONFLICT,
    );
    aq_server::profiled_execution::started::record(&f.state, &auth, &started)
        .await
        .unwrap();
    aq_server::profiled_execution::stopped::record(&f.state, &auth, &stop_request(&started))
        .await
        .unwrap();

    let other = aq_server::tokens::new_node_credential();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'conversation-other',$2,'0.1.0',1,now())")
        .bind(Uuid::new_v4()).bind(aq_server::tokens::verifier(&other)).execute(&f.db.pool().await).await.unwrap();
    assert_eq!(
        call(
            &worker,
            &upload,
            Some(snapshot.clone()),
            Some(&other),
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN,
    );
    for _ in 0..2 {
        let (status, receipt) = call(
            &worker,
            &upload,
            Some(snapshot.clone()),
            Some(&f.credential),
            false,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt, json!({"attempt_id":attempt,"session_id":id}));
    }
    let (status, saved) = call(&admin, &read, None, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        saved,
        json!({"session_id":id,"project_id":project,"conversation":snapshot})
    );
}

/// A queued attempt's pane is watched and typed into over the same live stream as a
/// hand-started agent: only the attempt's own node serves it, only while the attempt
/// runs, and a viewer is recorded as human control of the pane.
#[tokio::test]
async fn queued_attempt_terminal_uses_the_live_stream_of_its_own_running_node() {
    use common::native_started::{context, setup};

    let (f, started) = setup(true).await;
    let auth = context(&f).await;
    let attempt = started.binding.attempt_id;
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
    let pool = f.db.pool().await;
    aq_server::profiled_execution::started::record(&f.state, &auth, &started)
        .await
        .unwrap();
    let connect = format!("/api/attempt-terminals/{attempt}/terminal/connect");
    let missing = format!("/api/attempt-terminals/{}/terminal/connect", Uuid::new_v4());
    let client = Uuid::new_v4();
    let body = Some(json!({"client_id":client}));
    assert_eq!(
        call(&admin, &missing, body.clone(), None, true).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, control) = call(&admin, &connect, body, None, true).await;
    assert_eq!(status, StatusCode::OK, "{control}");
    live_stream_preserves_bytes_and_fences_input(
        &admin,
        &worker,
        &f.credential,
        (
            f.state.config.server.public_origin.trim_end_matches('/'),
            &pool,
            "attempt",
        ),
        attempt,
        client,
        control["generation"].as_u64().unwrap(),
    )
    .await;
    let viewers: i64 = sqlx::query_scalar("SELECT count(*) FROM attempt_terminal_viewers")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(viewers, 0, "a detached viewer no longer holds the pane");
}
