use super::*;
use aq_server::{AppState, InternalWork, config::Config, identity::verifier_from_config};
use axum::{
    Router,
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

async fn app(db: &TestDb, dev_email: &str) -> Router {
    let store = db.store().await;
    let now = Utc::now();
    // A second router over the same database finds both users already there.
    let _ = store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await;
    let _ = store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, now)
        .await;
    let config: Config = toml::from_str(&format!(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="{dev_email}"
[bootstrap]
owner_email="owner@localhost"
enabled=true
"#
    ))
    .unwrap();
    let clock = Arc::new(Utc::now);
    let work = InternalWork::new(db.pool().await, clock.clone(), None, None).unwrap();
    aq_server::build_admin_router(AppState {
        internal_work: work,
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock,
        agent_access: None,
        source_retention: None,
    })
}

async fn call(app: &Router, method: &str, path: &str, csrf: bool) -> (StatusCode, Value) {
    let mut req = HttpRequest::builder().uri(path).method(method);
    if csrf {
        req = req.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

fn about(kind: &str, id: &str, purpose: Purpose) -> Request {
    let mut request = request(false);
    request.purpose = purpose;
    request.affected.kind = kind.into();
    request.affected.id = id.into();
    request
}

async fn seed_runs(pool: &PgPool) -> (Uuid, Uuid, Uuid, Value) {
    let one = [api("openai")];
    let a = store::submit(
        pool,
        &about("task", "T-1", Purpose::RoutingClassification),
        &one,
        NOW,
    )
    .await
    .unwrap();
    let b = store::submit(
        pool,
        &about("task", "T-2", Purpose::PlaybookCall),
        &one,
        NOW + 10,
    )
    .await
    .unwrap();
    let c = store::submit(
        pool,
        &about("task", "T-1", Purpose::RoutingClassification),
        &one,
        NOW + 20,
    )
    .await
    .unwrap();
    let claimed = store::Queue::at(pool, NOW + 25)
        .claim(Way::Api, &[], "r")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, a);
    let usage = Usage {
        input_tokens: 7,
        output_tokens: 3,
    };
    let answer = json!({"choice": "small", "rationale": "short"});
    store::Queue::at(pool, NOW + 125)
        .complete(store::CandidateAttempt::new(a, 0), &answer, Some(&usage))
        .await
        .unwrap();

    (a, b, c, answer)
}

async fn assert_answered_run(app: &Router, a: Uuid, answer: Value) {
    let (status, run) = call(app, "GET", &format!("/api/internal-work/runs/{a}"), false).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(
        run,
        json!({
            "id": a, "purpose": "routing_classification", "status": "answered",
            "affected": {"kind": "task", "id": "T-1", "project_id": null},
            "instructions": "Classify the task.", "input": "Task: fix the login page",
            "answer": answer,
            "attempts": [{"provider": "openai", "model": "openai-model", "way": "api",
                "reason": null, "detail": "", "elapsed_ms": 100}],
            "provider": "openai", "model": "openai-model", "way": "api", "elapsed_ms": 125,
            "usage": {"input_tokens": 7, "output_tokens": 3},
            "created_at_ms": NOW, "finished_at_ms": NOW + 125, "escalation": "none"
        })
    );
}

async fn assert_page_order(app: &Router, a: Uuid, b: Uuid) {
    let (_, page) = call(app, "GET", "/api/internal-work/runs?limit=2", false).await;
    assert_eq!(page["runs"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_cursor"], format!("{}:{b}", NOW + 10));
    let cursor = page["next_cursor"].as_str().unwrap();
    let (_, rest) = call(
        app,
        "GET",
        &format!("/api/internal-work/runs?limit=2&cursor={cursor}"),
        false,
    )
    .await;
    assert_eq!(rest["runs"].as_array().unwrap().len(), 1);
    assert_eq!(rest["runs"][0]["id"], a.to_string());
    assert_eq!(rest["next_cursor"], Value::Null);
}

async fn assert_invalid_run_queries(app: &Router) {
    for bad in [
        "limit=0",
        "limit=101",
        "cursor=nope",
        "purpose=nope",
        "status=nope",
    ] {
        let (status, _) = call(app, "GET", &format!("/api/internal-work/runs?{bad}"), false).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let missing = Uuid::new_v4();
    let (status, _) = call(
        app,
        "GET",
        &format!("/api/internal-work/runs/{missing}"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn runs_are_listed_newest_first_and_filtered_by_affected_item() {
    let db = TestDb::create().await;
    let app = app(&db, "owner@localhost").await;
    let pool = db.pool().await;
    let (a, b, c, answer) = seed_runs(&pool).await;
    let (status, all) = call(&app, "GET", "/api/internal-work/runs", false).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let ids: Vec<&str> = all["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c.to_string(), b.to_string(), a.to_string()]);
    assert_eq!(all["next_cursor"], Value::Null);

    let (_, one_item) = call(
        &app,
        "GET",
        "/api/internal-work/runs?affected_kind=task&affected_id=T-1",
        false,
    )
    .await;
    let ids: Vec<&str> = one_item["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c.to_string(), a.to_string()]);
    let (_, by_purpose) = call(
        &app,
        "GET",
        "/api/internal-work/runs?purpose=playbook_call",
        false,
    )
    .await;
    assert_eq!(by_purpose["runs"].as_array().unwrap().len(), 1);
    let (_, answered) = call(
        &app,
        "GET",
        "/api/internal-work/runs?status=answered",
        false,
    )
    .await;
    assert_eq!(answered["runs"][0]["id"], a.to_string());

    assert_answered_run(&app, a, answer).await;
    assert_page_order(&app, a, b).await;
    assert_invalid_run_queries(&app).await;
}

/// The run read shows what each candidate's agent did; the list does not.
#[tokio::test]
async fn the_run_read_includes_each_candidates_transcript() {
    let db = TestDb::create().await;
    let app = app(&db, "owner@localhost").await;
    let pool = db.pool().await;
    let candidates = [login("openai", "codex"), login("anthropic", "claude")];
    let id = store::submit(&pool, &agent_request(false), &candidates, NOW)
        .await
        .unwrap();
    let steps = json!([{"kind": "tool", "tool": "Bash", "input": "aq-playbooks check"}]);
    store::save_transcript(&pool, id, 1, &steps).await.unwrap();
    store::save_transcript(&pool, id, 0, &json!([]))
        .await
        .unwrap();
    let (status, run) = call(&app, "GET", &format!("/api/internal-work/runs/{id}"), false).await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(
        run["transcripts"],
        json!([{"candidate_index": 0, "events": []},
                {"candidate_index": 1, "events": steps}])
    );
    let (_, all) = call(&app, "GET", "/api/internal-work/runs", false).await;
    assert!(all["runs"][0].get("transcripts").is_none(), "{all}");
}

#[tokio::test]
async fn retry_resubmits_a_failed_run_and_refuses_a_running_one() {
    let db = TestDb::create().await;
    let app = app(&db, "owner@localhost").await;
    let pool = db.pool().await;
    let original = about("task", "T-9", Purpose::DecisionRevision);
    let failed = store::submit(&pool, &original, &[], NOW).await.unwrap();
    let queued = store::submit(&pool, &original, &[api("openai")], NOW + 1)
        .await
        .unwrap();
    let running = store::submit(&pool, &original, &[api("openai")], NOW + 2)
        .await
        .unwrap();
    store::Queue::at(&pool, NOW + 3)
        .claim(Way::Api, &[], "r")
        .await
        .unwrap();
    let held = store::get(&pool, queued).await.unwrap().unwrap();
    let busy = if held.status == "running" {
        queued
    } else {
        running
    };

    let (status, body) = call(
        &app,
        "POST",
        &format!("/api/internal-work/runs/{failed}/retry"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_ne!(new, failed);
    let old = store::get(&pool, failed).await.unwrap().unwrap();
    let row = store::get(&pool, new).await.unwrap().unwrap();
    assert_eq!(row.retry_of, Some(failed));
    assert_eq!(row.purpose, old.purpose);
    assert_eq!(row.request.input, old.request.input);
    assert_eq!(row.request.affected.id, "T-9");
    assert_eq!(old.retry_of, None);
    let (_, shown) = call(
        &app,
        "GET",
        &format!("/api/internal-work/runs/{new}"),
        false,
    )
    .await;
    assert_eq!(shown["retry_of"], failed.to_string());

    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/internal-work/runs/{busy}/retry"),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/internal-work/runs/{}/retry", Uuid::new_v4()),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 4);
}

#[tokio::test]
async fn runs_routes_require_owner() {
    let db = TestDb::create().await;
    let owner = app(&db, "owner@localhost").await;
    let pool = db.pool().await;
    let id = store::submit(&pool, &request(false), &[], NOW)
        .await
        .unwrap();
    let viewer = app(&db, "viewer@localhost").await;
    for (method, path) in [
        ("GET", "/api/internal-work/runs".to_string()),
        ("GET", format!("/api/internal-work/runs/{id}")),
        ("POST", format!("/api/internal-work/runs/{id}/retry")),
    ] {
        let (status, _) = call(&viewer, method, &path, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    let (status, _) = call(
        &owner,
        "POST",
        &format!("/api/internal-work/runs/{id}/retry"),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "retry needs the CSRF header");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM internal_tasks")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}
