//! Reference evidence for the mounted performance reports on disposable PostgreSQL.
use crate::common::profiled::{Fixture, fixture, fixture_on, insert_offered};
use aq_selection::TaskLineage;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;
mod usage;

const MINUTE: i64 = 60_000;
const READY: i64 = 10 * 24 * 60 * MINUTE + 9 * 60 * MINUTE;
const ELIGIBLE: i64 = READY + 2 * MINUTE;
const CLAIM: i64 = READY + 5 * MINUTE;
const MERGE: i64 = READY + 60 * MINUTE;
const COMPLETE: i64 = READY + 80 * MINUTE;
const CUTOFF: i64 = READY + 90 * MINUTE;
const BASE: &str = "/api/v1/performance";
const OWNER: &str = "owner@localhost";

async fn request(
    app: &Router,
    method: Method,
    path: &str,
    actor: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(actor) = actor {
        req = req.header("x-aq-dev-email", actor);
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"unexpected_body": String::from_utf8_lossy(&bytes)}));
    (status, body)
}

async fn read(app: &Router, path: &str, actor: Option<&str>) -> (StatusCode, Value) {
    request(app, Method::GET, path, actor).await
}

async fn success(app: &Router, endpoint: &str, project: Option<Uuid>, cutoff: i64) -> Value {
    let mut path = format!("{BASE}/{endpoint}?cutoff_ms={cutoff}");
    if let Some(project) = project {
        path.push_str(&format!("&project_id={project}"));
    }
    let (status, body) = read(app, &path, Some(OWNER)).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    assert_eq!(body["api_version"], "dashboard-read-1");
    assert_eq!(body["data"]["definition_version"], 1);
    assert_eq!(body["data"]["cutoff_ms"], cutoff);
    body["data"].clone()
}

#[tokio::test]
async fn mounted_performance_requires_owner_and_validates_time_and_scope() {
    let f = fixture().await;
    f.time.store(CUTOFF, Ordering::SeqCst);
    let pool = f.db.pool().await;
    sqlx::query(
        "INSERT INTO users(id,email,role,issuer,subject,created_at)
         VALUES($1,'performance-viewer@localhost','viewer','dev','performance-viewer@localhost',now())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap", Some(OWNER)).await;
    let capabilities = bootstrap["data"]["capabilities"].as_array().unwrap();
    assert!(
        capabilities
            .iter()
            .any(|c| c["name"] == "performance.metrics.v1" && c["enabled"] == true),
        "{capabilities:?}"
    );
    let (status, body) = read(&app, &format!("{BASE}/summary"), Some("")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let detail = format!("{BASE}/tasks/{}", Uuid::new_v4());
    for path in [
        format!("{BASE}/summary"),
        format!("{BASE}/holds"),
        format!("{BASE}/machines"),
        detail.clone(),
    ] {
        let (status, body) = read(&app, &path, Some("performance-viewer@localhost")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
        assert!(body["request_id"].is_string(), "{body}");
    }
    let (status, body) = read(&app, &detail, Some(OWNER)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    for path in [
        format!("{BASE}/summary?cutoff_ms=0"),
        format!("{BASE}/summary?cutoff_ms={}", CUTOFF + 1),
        format!(
            "{BASE}/summary?cutoff_ms={}",
            CUTOFF - 7 * 24 * 60 * MINUTE - 1
        ),
        format!("{BASE}/summary?cutoff_ms=invalid"),
        format!("{BASE}/summary?project_id=invalid"),
        format!("{BASE}/machines?project_id={}", Uuid::new_v4()),
        format!("{BASE}/holds?limit=0"),
        format!("{BASE}/holds?limit=101"),
    ] {
        let (status, body) = read(&app, &path, Some(OWNER)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{path}: {body}");
        assert!(body["request_id"].is_string(), "{body}");
    }
    let status = request(&app, Method::POST, &format!("{BASE}/summary"), Some(OWNER))
        .await
        .0;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let default = read(&app, &format!("{BASE}/summary"), Some(OWNER)).await;
    assert_eq!(default.0, StatusCode::OK, "{}", default.1);
    assert_eq!(default.1["data"]["cutoff_ms"], CUTOFF);
}

struct ReportTask {
    task: Uuid,
    project: Uuid,
    attempt: Uuid,
}

impl ReportTask {
    async fn insert(f: &Fixture) -> Self {
        let attempt = insert_offered(f, CLAIM as u64).await;
        Self {
            task: f.selection.lineage.task_id,
            project: f.selection.lineage.project_id,
            attempt,
        }
    }

    async fn record_merge(&self, pool: &PgPool, operation: Uuid, repository: Uuid) {
        sqlx::query(
            "INSERT INTO delivery_merge_evidence(operation_id,project_id,task_id,pull_request,
             target,target_ref,expected_head_sha,merge_sha,observed_at_ms)
             VALUES($1,$2,$3,7,$4,'refs/heads/main',$5,$6,$7)
             ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation)
        .bind(self.project)
        .bind(self.task)
        .bind(json!({"git":{"repository_id":repository,"binding_generation":1},"default_branch":"main"}))
        .bind("1".repeat(40))
        .bind("2".repeat(40))
        .bind(MERGE)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn record_completion(&self, pool: &PgPool, workspace: Uuid, operation: Uuid) {
        let mut tx = pool.begin().await.unwrap();
        // Install the upstream stop/result owner's synthetic historical rows.
        // Only this disposable fixture transaction suppresses their write guards.
        sqlx::query("SET LOCAL session_replication_role=replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO profiled_result_reports(report_id,attempt_id,node_id,operation_id,
             report_bytes,report_digest,receipt_bytes,outcome_artifact_id)
             SELECT $1,$2,node_id,$1,$3,encode(sha256($3),'hex'),$3,$1
             FROM execution_attempts WHERE attempt_id=$2 ON CONFLICT(report_id) DO NOTHING",
        )
        .bind(operation)
        .bind(self.attempt)
        .bind(br#"{"outcome":"success"}"#.as_slice())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO result_aggregates(result_id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(operation)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO result_revision_pins(report_id,result_id,report_revision,project_id,
             task_id,submitted_at_ms,capture_policy)
             VALUES($1,$1,repeat('a',64),$2,$3,$4,'{}') ON CONFLICT DO NOTHING",
        )
        .bind(operation)
        .bind(self.project)
        .bind(self.task)
        .bind(MERGE - MINUTE)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("SET LOCAL session_replication_role=origin")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SET LOCAL aq.settlement_owner='aq.settlement-owner.auto.1'")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,
             evidence,operation_id,workspace_id,task_id,expected_version,
             expected_content_revision,payload_bytes,receipt,created_at_ms,updated_at_ms)
             VALUES($1,$2,'aq.settlement.default-policy@1','confirmed','{}',$1,$3,$4,1,1,
                    $5,$6,$7,$8) ON CONFLICT(operation_id) DO NOTHING",
        )
        .bind(operation)
        .bind(self.attempt)
        .bind(workspace)
        .bind(self.task)
        .bind(b"synthetic graph operation".as_slice())
        .bind(json!({"operation_id":operation,"task_id":self.task}))
        .bind(CLAIM)
        .bind(COMPLETE)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    async fn record_hold(&self, pool: &PgPool) {
        sqlx::query(
            "INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,
             comment_body,held_at) VALUES($1,$2,1,$3,'retry_budget_exhausted',$4,
             'Synthetic qualification hold',to_timestamp($5::bigint / 1000.0))",
        )
        .bind(Uuid::new_v4())
        .bind(self.task)
        .bind(self.project)
        .bind(self.attempt)
        .bind(READY)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn record_reminder(&self, pool: &PgPool) {
        sqlx::query(
            "INSERT INTO supervisor_hold_followups(project_id,identity_digest,since_ms,task_id,
             attempt_id,next_check_at_ms,initial_message_id,last_message_id,reminders_sent)
             SELECT project_id,key,since_ms,task_id,attempt_id,$1,$2,$3,1
             FROM supervisor_hold_waits WHERE project_id=$4 AND task_id=$5",
        )
        .bind(MERGE + 45 * MINUTE)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(self.project)
        .bind(self.task)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn sibling(f: &Fixture, project: Uuid) -> Fixture {
    fixture_on(
        f.db.clone(),
        Some((
            f.actor,
            TaskLineage {
                project_id: project,
                task_id: Uuid::new_v4(),
                ..f.selection.lineage.clone()
            },
        )),
    )
    .await
}

#[tokio::test]
async fn reference_report_preserves_evidence_across_scope_cutoff_and_replay() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    f.time.store(CUTOFF, Ordering::SeqCst);
    let task = ReportTask::insert(&f).await;
    let held = ReportTask::insert(&sibling(&f, task.project).await).await;
    let hidden = ReportTask::insert(&sibling(&f, Uuid::new_v4()).await).await;
    let repository = f.profiled.source.repository_id;
    let workspace = f.selection.lineage.workspace_id;
    let merge_op = Uuid::new_v4();
    let completion_op = Uuid::new_v4();
    task.record_merge(&pool, merge_op, repository).await;
    task.record_completion(&pool, workspace, completion_op)
        .await;
    held.record_hold(&pool).await;
    hidden.record_hold(&pool).await;
    hidden
        .record_merge(&pool, Uuid::new_v4(), Uuid::new_v4())
        .await;
    sqlx::query(
        "INSERT INTO performance_task_first_ready(project_id,task_id,ready_at_ms)
         VALUES($1,$2,$3)",
    )
    .bind(task.project)
    .bind(task.task)
    .bind(READY)
    .execute(&pool)
    .await
    .unwrap();

    let app = aq_server::build_admin_router(f.state.clone());
    assert_machine_and_queue_evidence(&app, &pool, &task).await;
    let before_completion = success(&app, "summary", Some(task.project), COMPLETE - 1).await;
    assert_eq!(before_completion["outcomes"]["verified_merges"], 1);
    assert_eq!(before_completion["outcomes"]["confirmed_completions"], 0);
    let untracked = success(&app, "summary", Some(task.project), CUTOFF).await;
    assert_eq!(untracked["outcomes"]["delivery"]["state"], "not_tracked");
    assert_reference_outcomes(&untracked["outcomes"], &task);

    let definition = json!({"kind":"target_branch_merge","repository_id":repository,
        "target_ref":"refs/heads/main"});
    record_definition(&pool, f.actor, task.project, definition, 1, READY - MINUTE).await;
    let tracked = success(&app, "summary", Some(task.project), CUTOFF).await;
    assert_reference_outcomes(&tracked["outcomes"], &task);
    assert_eq!(tracked["outcomes"]["delivery"]["delivered_tasks"], 1);
    assert_eq!(
        tracked["outcomes"]["delivery"]["timing"]["median_ms"],
        60 * MINUTE
    );

    // A later settings revision cannot rewrite the cohort's frozen definition.
    record_definition(
        &pool,
        f.actor,
        task.project,
        json!({"kind":"confirmed_completion"}),
        2,
        CLAIM,
    )
    .await;
    let holds_before = success(&app, "holds", Some(task.project), CUTOFF).await;
    assert_eq!(holds_before["holds"]["count"], 1);
    assert_eq!(
        holds_before["holds"]["rows"][0]["task_id"],
        json!(held.task)
    );
    assert_eq!(holds_before["holds"]["rows"][0]["age_ms"], 90 * MINUTE);
    held.record_reminder(&pool).await;

    let fleet = success(&app, "summary", None, CUTOFF).await;
    assert_eq!(fleet["outcomes"]["verified_merges"], 2);
    let hidden_detail = format!("{BASE}/tasks/{}?project_id={}", hidden.task, task.project);
    let (status, body) = read(&app, &hidden_detail, Some(OWNER)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Reopen the store and rebuild the HTTP router before replaying the same
    // owner evidence identities. All established read facts should survive.
    drop(app);
    let mut restarted = f.state.clone();
    restarted.store = f.db.store().await;
    let app = aq_server::build_admin_router(restarted);
    task.record_merge(&pool, merge_op, repository).await;
    task.record_completion(&pool, workspace, completion_op)
        .await;
    let replayed = success(&app, "summary", Some(task.project), CUTOFF).await;
    assert_reference_outcomes(&replayed["outcomes"], &task);
    assert_eq!(replayed["outcomes"]["delivery"]["delivered_tasks"], 1);
    assert_eq!(
        replayed["outcomes"]["delivery"]["timing"]["median_ms"],
        60 * MINUTE
    );
    let after = success(&app, "holds", Some(task.project), CUTOFF).await;
    assert_eq!(after["holds"]["oldest_age_ms"], 90 * MINUTE);
    assert_eq!(after["holds"]["rows"][0]["since_ms"], READY);
    assert_eq!(
        after["holds"]["rows"][0]["next_check_at_ms"],
        MERGE + 45 * MINUTE
    );
    assert_hold_pagination(&app, task.project, [held.task, hidden.task]).await;
}

fn assert_reference_outcomes(outcomes: &Value, task: &ReportTask) {
    assert_eq!(outcomes["confirmed_completions"], 1, "{outcomes}");
    assert_eq!(outcomes["verified_merges"], 1, "{outcomes}");
    assert_eq!(outcomes["completion_timing"]["covered"], 1, "{outcomes}");
    assert_eq!(outcomes["completion_timing"]["median_ms"], 80 * MINUTE);
    assert_eq!(outcomes["merge_timing"]["covered"], 1, "{outcomes}");
    assert_eq!(outcomes["merge_timing"]["median_ms"], 60 * MINUTE);
    let tasks = outcomes["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{outcomes}");
    assert_eq!(tasks[0]["task_id"], json!(task.task));
    assert_eq!(tasks[0]["first_ready_at_ms"], READY);
}

async fn record_definition(
    pool: &PgPool,
    actor: Uuid,
    project: Uuid,
    definition: Value,
    revision: i64,
    effective_at_ms: i64,
) {
    sqlx::query(
        "INSERT INTO performance_delivery_definitions(project_id,work_class,revision,
         effective_at_ms,definition,operation_id,actor_id) VALUES($1,'',$2,$3,$4,$5,$6)",
    )
    .bind(project)
    .bind(revision)
    .bind(effective_at_ms)
    .bind(definition)
    .bind(Uuid::new_v4())
    .bind(actor)
    .execute(pool)
    .await
    .unwrap();
}

async fn assert_hold_pagination(app: &Router, project: Uuid, tasks: [Uuid; 2]) {
    let page = format!("{BASE}/holds?limit=1&cutoff_ms={CUTOFF}");
    let (status, first) = read(app, &page, Some(OWNER)).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let cursor = first["data"]["holds"]["next_cursor"].as_str().unwrap();
    let next = format!("{page}&cursor={cursor}");
    let (status, second) = read(app, &next, Some(OWNER)).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let mut actual = [first, second].map(|v| {
        v["data"]["holds"]["rows"][0]["task_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap()
    });
    actual.sort();
    let mut expected = tasks;
    expected.sort();
    assert_eq!(actual, expected);
    for changed in [
        format!("{next}&project_id={project}"),
        format!(
            "{BASE}/holds?limit=1&cutoff_ms={}&cursor={cursor}",
            CUTOFF - 1
        ),
    ] {
        let (status, body) = read(app, &changed, Some(OWNER)).await;
        assert_eq!(status, StatusCode::GONE, "{body}");
    }
}

async fn assert_machine_and_queue_evidence(app: &Router, pool: &PgPool, task: &ReportTask) {
    let missing = success(app, "machines", None, CUTOFF).await;
    assert_eq!(missing["machines"]["state"], "unavailable");
    assert_eq!(missing["scheduling"]["covered_claims"], 0);
    assert!(missing["scheduling"]["p50_eligible_to_claim_ms"].is_null());

    // Replay the same minute writes. Demand belongs to the aligned fleet tick.
    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO performance_fleet_minutes
             (minute_ms,observed_at_ms,inventory_complete,ready_count,ready_accuracy)
             SELECT minute,minute,true,1,'exact' FROM generate_series($1::bigint,$2,60000) minute
             ON CONFLICT DO NOTHING",
        )
        .bind(READY)
        .bind(READY + 22 * MINUTE)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO performance_machine_minutes
             (minute_ms,machine_id,label,state,enabled_slots,occupied_slots,free_slots,
              enabled_occupied_slots)
             SELECT minute_ms,machine,machine,
                    CASE WHEN minute_ms=$1 AND machine='idle-incompatible' THEN 'stale'
                         ELSE 'online' END,
                    1,(machine='working')::int,(machine<>'working')::int,(machine='working')::int
             FROM performance_fleet_minutes
             CROSS JOIN unnest(ARRAY['working','idle-incompatible']) AS machine
             ON CONFLICT DO NOTHING",
        )
        .bind(READY + 11 * MINUTE)
        .execute(pool)
        .await
        .unwrap();
    }
    // This durable fixture exercises a 3 min arithmetic case. The publication
    // probe may observe a shorter interval on a live scheduler.
    sqlx::query(
        "INSERT INTO performance_scheduling_timings
         (attempt_id,task_id,generation,eligible_at_ms,claimed_at_ms) VALUES($1,$2,1,$3,$4)",
    )
    .bind(task.attempt)
    .bind(task.task)
    .bind(ELIGIBLE)
    .bind(CLAIM)
    .execute(pool)
    .await
    .unwrap();
    let report = success(app, "machines", None, CUTOFF).await;
    let machines = &report["machines"];
    assert_eq!(machines["state"], "partial", "{machines}");
    assert_eq!(machines["coverage"]["observed_minutes"], 23);
    assert_eq!(machines["ready"].as_array().unwrap().len(), 23);
    assert_eq!(machines["ready"][0]["count"], 1);
    let rows = machines["machines"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let idle = rows
        .iter()
        .find(|m| m["id"] == "idle-incompatible")
        .unwrap();
    assert_eq!(idle["samples"][11]["state"], "stale");
    assert_eq!(
        idle["idle_with_queue"],
        json!([
            {"start_ms": READY, "end_ms": READY + 11 * MINUTE},
            {"start_ms": READY + 12 * MINUTE, "end_ms": READY + 23 * MINUTE},
        ])
    );
    let schedule = &report["scheduling"];
    assert!(schedule["claims"].as_i64().unwrap() >= 1, "{schedule}");
    assert_eq!(schedule["covered_claims"], 1, "{schedule}");
    assert_eq!(
        schedule["p50_eligible_to_claim_ms"],
        3 * MINUTE,
        "{schedule}"
    );
    assert_eq!(schedule["p95_eligible_to_claim_ms"], 3 * MINUTE);
    let earlier = success(app, "machines", None, CLAIM - 1).await;
    assert_eq!(earlier["scheduling"]["covered_claims"], 0);
    assert_eq!(earlier["machines"]["ready"].as_array().unwrap().len(), 5);
}
