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
    let catalog = json!({"catalog":{"intelligence":[{"id":"manual-model","version":1,"candidates":[{"provider":"anthropic","model":"fixture-model"}]}]}});
    sqlx::query("INSERT INTO profile_catalog_artifacts(digest,artifact,created_by,created_at_ms) VALUES($1,$2,$3,1)").bind(&digest).bind(serde_json::to_vec(&catalog).unwrap()).bind(f.actor).execute(&pool).await.unwrap();
    let binding = json!({"enabled":true,"name":"manual fixture","repository_id":repo,"catalog_digest":digest,"allowed_intelligence_profiles":["manual-model@1"],"allowed_node_ids":[],"allowed_worker_groups":[]});
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}')").bind(project).bind(binding).execute(&pool).await.unwrap();
    let mut config = (*f.state.config).clone();
    config.profiled_execution=Some(serde_json::from_value(json!({"revision":1,"qualified_runtimes":[],"projects":[{"project_id":project,"enabled":true,"priority":0,"source":{"kind":"immutable","object":{"format":"sha1","hex":"a".repeat(40)}},"retention_handle":Uuid::new_v4()}]})).unwrap());
    f.state.config = Arc::new(config);
    let admin = build_admin_router(f.state.clone());
    let worker = build_worker_router(f.state.clone());
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
