//! R6 (lessons W83, W87): a ready task the offer path does not offer for good (here the catalog
//! retry policy's `max_attempts` is used up) is held visibly: the hold is recorded on the task
//! once, named in exactly one system comment, shown in the task's work view and counted by the
//! swarm read (one shared definition), and the node takes other work. The hold clears when the
//! task gets a new attempt or is closed. The actual mounted poll, publication and human work
//! route run; the failure owner facts are synthetic (`fail_and_release`).
use crate::common::native_offer::{
    self, Case, fail_and_release, pin_policy, poll, poll_offered, setup,
};
use aq_admission::control::{ControlReply, decode_reply};
use aq_machine_control::holds::{self, Decision, Successor};
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;

/// The task's work view (`GET /api/human/projects/{p}/tasks/{t}/work`), checked against the
/// human-graph-1 schema.
pub(super) async fn work(c: &Case, task: Uuid) -> Value {
    let state = &c.env.env.state;
    let project = c.env.lineage.project_id;
    let app = aq_server::agent_access::human::routes(state).with_state(state.clone());
    let res = app
        .oneshot(
            axum::http::Request::get(format!("/api/human/projects/{project}/tasks/{task}/work"))
                .header("x-aq-dev-email", "owner@localhost")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, 200, "{body}");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../../protocol/human-graph/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, &body),
        "schema: {body}"
    );
    body["data"]["readiness"].clone()
}
/// The `task_held` reason code in a work view's readiness, if any.
pub(super) fn held_reason(readiness: &Value) -> Option<String> {
    (readiness["reasons"].as_array().unwrap().iter())
        .find(|r| r["code"] == "task_held")
        .map(|r| r["hold_reason"].as_str().unwrap().to_owned())
}
/// What the swarm read counts for the case's project.
pub(super) async fn swarm(c: &Case) -> Vec<(Uuid, String, i64)> {
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    holds::counts(&mut conn, &[c.env.lineage.project_id])
        .await
        .unwrap()
}
/// W83: on the same data the swarm count equals the tasks whose work view shows a hold.
pub(super) async fn agree(c: &Case, tasks: &[Uuid]) -> i64 {
    let mut viewed = 0;
    for task in tasks {
        viewed += i64::from(held_reason(&work(c, *task).await).is_some());
    }
    let counted: i64 = swarm(c).await.iter().map(|(_, _, n)| n).sum();
    assert_eq!(counted, viewed, "the swarm counts what the task views show");
    counted
}
/// System comments on `task`.
pub(super) async fn system_comments(c: &Case, task: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query_as(
        "SELECT comment_id,body FROM task_comments WHERE task_id=$1 AND author_kind='system' ORDER BY created_at",
    )
    .bind(task)
    .fetch_all(&c.env.env.pool)
    .await
    .unwrap()
}
/// Every hold row of `task` (reason, comment id, commented, cleared by).
pub(super) async fn hold_rows(c: &Case, task: Uuid) -> Vec<(String, Uuid, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT reason,comment_id,commented,cleared_by FROM task_holds WHERE task_id=$1 ORDER BY held_at,comment_id",
    )
    .bind(task)
    .fetch_all(&c.env.env.pool)
    .await
    .unwrap()
}
fn offered_tasks(body: &Value) -> Vec<Uuid> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    (page.offers.iter())
        .map(|o| o.offered.offer.manifest.bindings.task_id)
        .collect()
}
/// A case whose task's only attempt failed (attempt count 1) and was released, unless a budget
/// holds the task.
async fn failed_once(max_attempts: Option<u32>) -> (Case, Uuid) {
    let mut c = setup().await;
    if let Some(n) = max_attempts {
        pin_policy(&mut c, |p| p.max_attempts = n).await;
    }
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let predecessor = page.offers.remove(0).offered.offer.binding.attempt_id;
    fail_and_release(&c, predecessor).await;
    (c, predecessor)
}
fn decision(c: &Case, reason: &'static str, seen: i32) -> Option<Decision<'static>> {
    Some(Decision {
        project: c.env.lineage.project_id,
        task: c.env.lineage.task_id,
        reason,
        seen,
        attempt: None,
    })
}

#[tokio::test]
async fn a_task_past_its_policy_max_attempts_is_held_visibly_once_and_other_work_is_offered() {
    let (c, predecessor) = failed_once(Some(1)).await;
    let (task, second) = (c.env.lineage.task_id, c.second_task);
    // Before the decision: no hold, no comment, and the work view names none.
    assert_eq!(held_reason(&work(&c, task).await), None);
    assert!(hold_rows(&c, task).await.is_empty(), "nothing is held yet");
    // A refusal that is only a wait records nothing.
    let (pool, at) = (c.env.env.pool.clone(), (c.env.env.state.clock)());
    let wait = decision(&c, "replacement_prior_execution_uncertain", 1);
    holds::hold(&pool, wait, at).await;
    assert!(hold_rows(&c, task).await.is_empty(), "a wait is not a hold");

    // The decision, made by the actual poll, twice (a replay): no offer, one hold, one comment.
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered_tasks(&body).is_empty(), "{body}");
    }
    let rows = hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (reason, comment_id, commented, cleared) = &rows[0];
    assert_eq!(reason, "lineage_retry_budget_exhausted");
    assert!(*commented && cleared.is_none());
    assert_eq!(*comment_id, Uuid::new_v5(&task, b"aq.task-hold/1/0"));
    let comments = system_comments(&c, task).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].0, *comment_id);
    assert!(
        comments[0].1.contains(
            "its retry budget is exhausted (hold reason: lineage_retry_budget_exhausted)"
        ),
        "{comments:?}"
    );
    // Replaying the decision directly records and posts nothing more.
    let again = decision(&c, "lineage_retry_budget_exhausted", 1);
    holds::hold(&pool, again, at).await;
    let mut conn = pool.acquire().await.unwrap();
    let other = decision(&c, "lineage_time_budget_exhausted", 1).unwrap();
    let noted = holds::note(&mut conn, &other, None, at).await.unwrap();
    assert_eq!(noted, None, "one hold per attempt history");
    drop(conn);
    assert_eq!(system_comments(&c, task).await.len(), 1);
    assert_eq!(hold_rows(&c, task).await.len(), 1);

    // The task's work view shows the hold as recorded; the swarm counts the same.
    let readiness = work(&c, task).await;
    assert_eq!(readiness["state"], "held", "{readiness}");
    let reason = (readiness["reasons"].as_array().unwrap().iter())
        .find(|r| r["code"] == "task_held")
        .unwrap_or_else(|| panic!("{readiness}"));
    assert_eq!(reason["hold_reason"], "lineage_retry_budget_exhausted");
    assert_eq!(reason["attempt_id"], serde_json::json!(predecessor));
    let project = c.env.lineage.project_id;
    assert_eq!(
        swarm(&c).await,
        [(project, "lineage_retry_budget_exhausted".to_owned(), 1)]
    );
    let mut conn = pool.acquire().await.unwrap();
    let elsewhere = holds::counts(&mut conn, &[Uuid::new_v4()]).await.unwrap();
    assert!(elsewhere.is_empty(), "another project has none");
    drop(conn);

    // The node takes the other ready task; the held one is never offered.
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [second], "{body}");
    assert_eq!(system_comments(&c, task).await.len(), 1);
    assert_eq!(agree(&c, &[task, second]).await, 1);

    // The sweep keeps the hold of an active task and clears it once Graph has it closed.
    let tasks = &c.env.env.state.agent_access;
    assert_eq!(holds::sweep(&pool, tasks, at).await.unwrap(), 0);
    assert_eq!(agree(&c, &[task, second]).await, 1, "still held");
    c.mode.store(8, Ordering::SeqCst);
    assert_eq!(holds::sweep(&pool, tasks, at).await.unwrap(), 1);
    assert_eq!(
        hold_rows(&c, task).await[0].3.as_deref(),
        Some("task_closed")
    );
    assert!(swarm(&c).await.is_empty(), "a closed task is not held");
    assert_eq!(held_reason(&work(&c, task).await), None);
    c.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        agree(&c, &[task]).await,
        0,
        "the view agrees once it is open again"
    );
}

/// Spec 0014 §5.5 (step 3a): a task whose effective role is disabled gets no
/// offer and its work view says why; other work is offered, and the task is
/// offered once the role is enabled. No hold is recorded: it is read live.
#[tokio::test]
async fn a_task_whose_role_is_disabled_gets_no_offer_and_other_work_is_offered() {
    use crate::common::save_role;
    let c = setup().await;
    let (task, second) = (c.env.lineage.task_id, c.second_task);
    let (pool, owner) = (c.env.env.pool.clone(), c.env.actor);
    *c.metadata.lock().unwrap() = serde_json::json!({"aq:role":"reviewer"});
    save_role(&pool, owner, "reviewer", "reviewer", false).await;
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered_tasks(&body).is_empty(), "{body}");
    }
    let readiness = work(&c, task).await;
    assert_eq!(readiness["state"], "held", "{readiness}");
    assert_eq!(readiness["reasons"][0]["code"], "role_disabled");
    assert_eq!(readiness["reasons"][0]["role"], "reviewer");
    assert!(
        hold_rows(&c, task).await.is_empty(),
        "read live, not recorded"
    );
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0, "no offer was made");
    // The node takes the other ready task, which runs as the default role.
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [second], "{body}");
}
/// The same task is offered when its role is enabled, and a role that does
/// not exist holds it as a disabled one does.
#[tokio::test]
async fn a_task_is_offered_once_its_role_is_enabled_and_a_missing_role_holds_it() {
    use crate::common::save_role;
    let c = setup().await;
    let task = c.env.lineage.task_id;
    let (pool, owner) = (c.env.env.pool.clone(), c.env.actor);
    *c.metadata.lock().unwrap() = serde_json::json!({"aq:role":"tester"});
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert!(offered_tasks(&body).is_empty(), "{body}");
    assert_eq!(work(&c, task).await["reasons"][0]["code"], "role_missing");
    save_role(&pool, owner, "developer", "tester", false).await;
    let (_, body) = poll(&c).await;
    assert!(offered_tasks(&body).is_empty(), "{body}");
    assert_eq!(work(&c, task).await["reasons"][0]["code"], "role_disabled");
    save_role(&pool, owner, "developer", "tester", true).await;
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [task], "{body}");
}

/// Owner ruling 4b (2026-11-13): a task the default policy completed with no file change
/// reads as such in its work view, from the server's `no_change` settlement evidence.
#[tokio::test]
async fn a_task_completed_with_no_change_says_so_in_its_work_view() {
    let c = setup().await;
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let attempt = page.offers.remove(0).offered.offer.binding.attempt_id;
    let task = c.env.lineage.task_id;
    c.mode.store(10, Ordering::SeqCst);
    let work_view = work(&c, task).await;
    assert_eq!(work_view["state"], "completed", "{work_view}");
    let flag = |view: &Value| view.get("completed_without_changes").cloned();
    assert_eq!(
        flag(&full_work(&c, task).await),
        None,
        "nothing settled yet"
    );
    // A synthetic confirmed default-policy settlement (owner trigger bypassed).
    let mut tx = c.env.env.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,evidence,operation_id,workspace_id,task_id,expected_version,expected_content_revision,payload_bytes,receipt,created_at_ms,updated_at_ms) VALUES($1,$2,'aq.settlement.default-policy@1','confirmed',$3,$4,$4,$5,1,1,'\\x7b7d','{}',1000,1000)")
        .bind(Uuid::new_v4()).bind(attempt)
        .bind(serde_json::json!({"delivery":{"policy":"required","state":"no_change"}}))
        .bind(Uuid::new_v4()).bind(task)
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        flag(&full_work(&c, task).await),
        Some(serde_json::json!(true))
    );
}
/// The whole work view data of `task` (schema-checked by [`work`]).
pub(super) async fn full_work(c: &Case, task: Uuid) -> Value {
    work(c, task).await;
    let state = &c.env.env.state;
    let project = c.env.lineage.project_id;
    let app = aq_server::agent_access::human::routes(state).with_state(state.clone());
    let res = app
        .oneshot(
            axum::http::Request::get(format!("/api/human/projects/{project}/tasks/{task}/work"))
                .header("x-aq-dev-email", "owner@localhost")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    body["data"].clone()
}

/// The owner's `task.retry` dashboard action on the case's task, with the action's `reason`.
async fn retry(c: &Case, operation: Uuid, reason: Option<&str>) -> (axum::http::StatusCode, Value) {
    retry_with(c, operation, reason, false).await
}
/// [`retry`], with the payload's `fresh` option when asked for.
async fn retry_with(
    c: &Case,
    operation: Uuid,
    reason: Option<&str>,
    fresh: bool,
) -> (axum::http::StatusCode, Value) {
    let (task, project) = (c.env.lineage.task_id, c.env.lineage.project_id);
    let mut action = serde_json::json!({"operation_id":operation,"kind":"task.retry",
        "subject":{"type":"task","id":task,"revision":"1"},"expected_versions":{"task":"1"},
        "payload":{"operation_id":operation,"project_id":project,"expected_version":"1"}});
    if fresh {
        action["payload"]["fresh"] = true.into();
    }
    if let Some(reason) = reason {
        action["reason"] = reason.into();
    }
    let res = aq_server::build_admin_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/api/v1/actions")
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(action.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}
/// Owner task actions need the configured Graph mutation identity.
async fn owner_actions(c: &mut Case) {
    let config = aq_server::agent_access::AgentAccessConfig {
        graph_url: c.graph_url.clone(),
        graph_token_file: c.env.env._dir.path().join("graph-token"),
        credential_ttl_ms: 10000,
        mutation_actor_id: Some("synthetic-owner-actions".into()),
    };
    let runtime = aq_server::agent_access::Runtime::new(&config)
        .await
        .unwrap();
    c.env.env.state.agent_access = Some(std::sync::Arc::new(runtime));
}
/// A case whose task's only attempt failed and is held by the budget `code`. Each limit is the
/// Settings value the attempt's launch pinned.
async fn budget_held(code: &str) -> (Case, Uuid) {
    let mut c = setup().await;
    pin_policy(&mut c, |p| match code {
        "lineage_retry_budget_exhausted" => p.max_attempts = 1,
        "lineage_time_budget_exhausted" => p.max_lineage_elapsed_ms = 2_500,
        _ => p.max_lineage_sandbox_ms = 1_000,
    })
    .await;
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let predecessor = page.offers.remove(0).offered.offer.binding.attempt_id;
    if code == "task_sandbox_minutes_budget_exhausted" {
        // The attempt was granted a window (synthetic; the hot guard is bypassed).
        let mut tx = c.env.env.pool.begin().await.unwrap();
        sqlx::query("SET LOCAL session_replication_role=replica")
            .execute(&mut *tx)
            .await
            .unwrap();
        let deadline = (c.env.env.state.clock)().timestamp_millis() + 5_000;
        sqlx::query("UPDATE profiled_authority_hot SET state=jsonb_set(state,'{execution_deadline_ms}',to_jsonb($2::BIGINT)) WHERE attempt_id=$1")
            .bind(predecessor)
            .bind(deadline)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    if code == "lineage_time_budget_exhausted" {
        // Three seconds, so the node's observations stay fresh for the poll after the override.
        c.env.env.time.fetch_add(3_000, Ordering::SeqCst);
    }
    // The failed attempt keeps its claim: B4 releases nothing while a budget holds the task.
    // The hold is recorded as the poll's decision records it (the fixture's synthetic stop
    // frees no compute, so its node cannot poll while the attempt holds the task).
    native_offer::fail(&c, predecessor).await;
    let (pool, at) = (c.env.env.pool.clone(), (c.env.env.state.clock)());
    let d = Decision {
        project: c.env.lineage.project_id,
        task: c.env.lineage.task_id,
        reason: code,
        seen: 1,
        attempt: None,
    };
    holds::hold(&pool, Some(d), at).await;
    (c, predecessor)
}

/// Owner decision 2026-11-18 (task S5): Retry on a task a budget holds is the owner's explicit
/// decision and always releases it. Until then it was refused (M2), and raising the Settings
/// limit did not help because the budget is the one the last attempt pinned. The override is
/// recorded with the owner's reason and named in the task's history, the budget starts again,
/// and the offer path's own assessment admits the next attempt.
#[tokio::test]
async fn an_owner_retry_overrides_each_budget_hold_and_records_the_reason() {
    use aq_server::profiled_execution::replacement::{self, Assessment};
    for code in [
        "lineage_retry_budget_exhausted",
        "lineage_time_budget_exhausted",
        "task_sandbox_minutes_budget_exhausted",
    ] {
        let (mut c, predecessor) = budget_held(code).await;
        let task = c.env.lineage.task_id;
        let pool = c.env.env.pool.clone();
        owner_actions(&mut c).await;
        assert_eq!(held_reason(&work(&c, task).await).as_deref(), Some(code));
        let revision = replacement::predecessor_project_revision(&pool, predecessor)
            .await
            .unwrap()
            .unwrap();
        let assess = || async {
            let state = &c.env.env.state;
            let now = (state.clock)().timestamp_millis() as u64;
            replacement::assess_task_for_revision(state, task, now, revision)
                .await
                .unwrap()
        };
        assert_eq!(assess().await, Assessment::Blocked(code));

        let operation = Uuid::new_v4();
        let (status, body) = retry(&c, operation, Some("S5 was making progress")).await;
        assert_eq!(status, 200, "{code}: {body}");
        assert_eq!(body["data"]["disposition"], "confirmed", "{code}: {body}");
        // The record: who, which hold, why. One row per operation; a replay adds none.
        let (status, replay) = retry(&c, operation, Some("S5 was making progress")).await;
        assert_eq!(status, 200, "{code}: {replay}");
        let mut conn = pool.acquire().await.unwrap();
        let recorded = holds::overrides(&mut conn, task).await.unwrap();
        drop(conn);
        assert_eq!(recorded.len(), 1, "{code}: {recorded:?}");
        let o = &recorded[0];
        assert_eq!(
            (o.operation_id, o.attempt_id, o.hold_code.as_str()),
            (operation, predecessor, code)
        );
        assert_eq!(
            (o.reason.as_str(), o.actor.as_str()),
            ("S5 was making progress", "owner@localhost")
        );
        // The task's history shows it, once.
        let comments = system_comments(&c, task).await;
        let shown: Vec<_> = (comments.iter())
            .filter(|(_, body)| body.contains("overrode this task's hold"))
            .collect();
        assert_eq!(shown.len(), 1, "{code}: {comments:?}");
        assert!(
            shown[0].1.contains(&format!("(hold reason: {code})"))
                && shown[0]
                    .1
                    .ends_with("Owner's reason: S5 was making progress"),
            "{comments:?}"
        );
        // The hold is gone, the claim is released, and the budget started again: nothing
        // before the override is charged, so the offer path's assessment admits a successor.
        assert_eq!(held_reason(&work(&c, task).await), None, "{code}");
        assert!(swarm(&c).await.is_empty(), "{code}");
        let Assessment::Replace(r) = assess().await else {
            panic!("{code}: the next offer assessment is not blocked");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (1, 1), "{code}");
        assert_eq!((r.elapsed_ms, r.sandbox_ms, r.needs_release), (0, 0, false));
        let rows = hold_rows(&c, task).await;
        assert_eq!(rows.len(), 1, "{code}: {rows:?}");
        assert!(rows[0].3.is_some(), "{code}: the hold is cleared: {rows:?}");
        let holding: bool =
            sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
                .bind(predecessor)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!holding, "{code}: the predecessor's claim is released");
        // The record cannot be changed.
        let changed = sqlx::query("UPDATE task_limit_overrides SET reason='x' WHERE task_id=$1")
            .bind(task)
            .execute(&pool)
            .await;
        assert!(changed.is_err(), "{code}: overrides are immutable");
        // The real offer path: the node polls and is offered the task again.
        let (status, body) = poll_offered(&c).await;
        assert_eq!(status, 200, "{code}: {body}");
        assert_eq!(offered_tasks(&body), [task], "{code}: {body}");
        let attempts: i64 =
            sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE task_id=$1")
                .bind(task)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 2, "{code}: a new attempt of the task");
    }
}

/// A retry that states no reason still releases the task (the owner is never stuck), and the
/// record says that none was given.
#[tokio::test]
async fn an_owner_retry_without_a_reason_overrides_and_says_none_was_given() {
    let (mut c, _) = budget_held("lineage_retry_budget_exhausted").await;
    let task = c.env.lineage.task_id;
    owner_actions(&mut c).await;
    let (status, body) = retry(&c, Uuid::new_v4(), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let recorded = holds::overrides(&mut conn, task).await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].reason, "owner retry (no reason given)");
}

/// M2 (owner decision 2026-11-13) still stands for a hold that is not a limit: Retry on a
/// quarantined task is refused with the reason, nothing is overridden and nothing changes,
/// even when a budget would hold the task too.
#[tokio::test]
async fn retry_on_a_quarantined_task_is_refused_and_overrides_nothing() {
    let (mut c, predecessor) = budget_held("lineage_retry_budget_exhausted").await;
    let task = c.env.lineage.task_id;
    owner_actions(&mut c).await;
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'test quarantine','owner','{}',$3,1)")
        .bind(predecessor)
        .bind(task)
        .bind(Uuid::new_v4())
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    let (status, body) = retry(&c, Uuid::new_v4(), Some("try anyway")).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "stall_quarantined");
    assert_eq!(body["retryable"], false);
    let message = body["message"].as_str().unwrap();
    for part in [
        "the stall ladder quarantined it",
        "A retry cannot clear this hold",
        "cancel this task and create a new one",
    ] {
        assert!(message.contains(part), "{message}");
    }
    // Nothing changed: no operation was admitted, no override recorded, the claim is kept.
    let admitted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dashboard_operations WHERE kind='task.retry'")
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(admitted, 0);
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    assert!(holds::overrides(&mut conn, task).await.unwrap().is_empty());
    drop(conn);
    assert_eq!(
        held_reason(&work(&c, task).await).as_deref(),
        Some("lineage_retry_budget_exhausted")
    );
}

/// I1, I3, M7: a hold is recorded only against the attempt count its decision saw, racing
/// recorders write one row and one comment, a lost mark is repaired without a second comment,
/// and a new attempt clears the hold in the view and the swarm alike.
#[tokio::test]
async fn hold_records_race_free_and_post_their_stored_comment_once() {
    let (c, predecessor) = failed_once(None).await;
    let (task, pool) = (c.env.lineage.task_id, c.env.env.pool.clone());
    let at = (c.env.env.state.clock)();
    // M7: eight polls race to record and post the same hold.
    let racers: Vec<_> = (0..8)
        .map(|_| {
            let (pool, d) = (
                pool.clone(),
                decision(&c, "lineage_retry_budget_exhausted", 1),
            );
            tokio::spawn(async move { holds::hold(&pool, d, at).await })
        })
        .collect();
    for r in racers {
        r.await.unwrap();
    }
    let rows = hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].2, "commented");
    assert_eq!(system_comments(&c, task).await.len(), 1);
    assert_eq!(agree(&c, &[task]).await, 1);

    // I3: the post succeeded but its mark was lost. The next decision, whatever its own reason,
    // posts the stored comment (a replay of the same id) and marks it, with no second comment.
    let unmark = "UPDATE task_holds SET commented=FALSE WHERE task_id=$1";
    sqlx::query(unmark).bind(task).execute(&pool).await.unwrap();
    holds::hold(&pool, decision(&c, "lineage_time_budget_exhausted", 1), at).await;
    assert!(hold_rows(&c, task).await[0].2, "marked again");
    let comments = system_comments(&c, task).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0]
            .1
            .contains("hold reason: lineage_retry_budget_exhausted")
    );
    // An id already taken by another body counts as posted too, and is marked once.
    sqlx::query(
        "UPDATE task_holds SET commented=FALSE,comment_body='AQ: another body' WHERE task_id=$1",
    )
    .bind(task)
    .execute(&pool)
    .await
    .unwrap();
    holds::hold(&pool, decision(&c, "lineage_retry_budget_exhausted", 1), at).await;
    assert!(
        hold_rows(&c, task).await[0].2,
        "an id conflict is marked posted"
    );
    assert_eq!(system_comments(&c, task).await.len(), 1);
    let mut conn = pool.acquire().await.unwrap();
    let marked = decision(&c, "lineage_retry_budget_exhausted", 1).unwrap();
    assert_eq!(
        holds::note(&mut conn, &marked, None, at).await.unwrap(),
        None
    );

    // I1: B counts the task's attempts (1), node A then publishes attempt 2, then B records
    // its decision. The new attempt cleared the hold, and B's decision is not recorded.
    let seen = holds::attempts(&pool, task).await;
    assert_eq!(seen, Some(1));
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,holds_task,state) VALUES($1,$2,$3,$4,$5,FALSE,'{}')")
        .bind(Uuid::new_v4()).bind(task).bind(c.env.env.node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&pool).await.unwrap();
    let rows = hold_rows(&c, task).await;
    assert_eq!(rows[0].3.as_deref(), Some("new_attempt"), "{rows:?}");
    assert_eq!(
        agree(&c, &[task]).await,
        0,
        "a new attempt clears both reads"
    );
    let stale = decision(&c, "lineage_retry_budget_exhausted", 1).unwrap();
    assert_eq!(
        holds::note(&mut conn, &stale, None, at).await.unwrap(),
        None
    );
    holds::hold(&pool, Some(stale), at).await;
    assert_eq!(
        hold_rows(&c, task).await.len(),
        1,
        "B's stale decision is not recorded"
    );
    // The same decision against the current count is (a positive control).
    let fresh = Decision {
        seen: 2,
        attempt: Some(predecessor),
        ..stale
    };
    holds::hold(&pool, Some(fresh), at).await;
    assert_eq!(agree(&c, &[task]).await, 1);
    assert_eq!(
        system_comments(&c, task).await.len(),
        2,
        "a new hold, its own comment"
    );
}

/// M5: a successor is held as stuck only after it has been skipped on every poll for ten
/// minutes and at least 20 times. Many nodes skipping it quickly do not trip it sooner.
#[tokio::test]
async fn a_stuck_successor_needs_ten_minutes_of_skips_not_only_twenty() {
    let (c, predecessor) = failed_once(None).await;
    let task = c.env.lineage.task_id;
    let pool = c.env.env.pool.clone();
    let s = Successor {
        project: c.env.lineage.project_id,
        task,
        predecessor,
        seen: Some(1),
    };
    let t0 = (c.env.env.state.clock)();
    let at = |seconds: i64| t0 + chrono::Duration::seconds(seconds);
    // A fleet skipping it 60 times in its first minute.
    for i in 0..60 {
        holds::skipped(&pool, s, "test refusal", at(i)).await;
    }
    assert!(
        hold_rows(&c, task).await.is_empty(),
        "60 skips in a minute are not stuck"
    );
    // Then one skip every 30 s, never a gap of five minutes.
    for i in 1..20 {
        holds::skipped(&pool, s, "test refusal", at(30 * i)).await;
        assert!(
            hold_rows(&c, task).await.is_empty(),
            "not stuck at {}s",
            30 * i
        );
    }
    holds::skipped(&pool, s, "test refusal", at(600)).await;
    let rows = hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "replacement_successor_stuck");
    // A gap of five minutes starts the count again.
    let mut conn = pool.acquire().await.unwrap();
    let gap = holds::skip(&mut conn, task, predecessor, at(600 + 301))
        .await
        .unwrap();
    assert_eq!(gap, (1, None), "a five-minute gap restarts the count");
}

/// The never-granted bound is Settings › Selection defaults `max_never_granted_offers`, and an
/// owner retry overrides its hold too: the count starts again and the node is offered the task
/// through the real poll.
#[tokio::test]
async fn an_owner_retry_overrides_the_never_granted_bound_and_the_task_is_offered_again() {
    use aq_server::profiled_execution::no_grant;
    let mut c = setup().await;
    pin_policy(&mut c, |p| p.max_never_granted_offers = 1).await;
    let (task, pool) = (c.env.lineage.task_id, c.env.env.pool.clone());
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let first = page.offers.remove(0).offered.offer.binding.attempt_id;
    // The offer expires without a grant and is closed as never granted.
    c.env.env.time.fetch_add(30_000, Ordering::SeqCst);
    let node = c.env.env.node;
    no_grant::close_expired(&c.env.env.state, node, first)
        .await
        .unwrap()
        .unwrap();
    // The fixture's node observations are fresh for five seconds only, so the case clock goes
    // back and the closure's instant with it (synthetic; its guards are bypassed).
    c.env.env.time.fetch_sub(30_000, Ordering::SeqCst);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_never_granted_closures SET closed_at_ms=closed_at_ms-30000 WHERE attempt_id=$1")
        .bind(first)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let counts = no_grant::attempt_counts(&pool, task).await.unwrap();
    assert_eq!((counts.never_granted, counts.max_never_granted), (1, 1));
    // The poll holds the task at the bound.
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert!(offered_tasks(&body).is_empty(), "{body}");
    owner_actions(&mut c).await;
    let code = "never_granted_retry_bound_exhausted";
    assert_eq!(held_reason(&work(&c, task).await).as_deref(), Some(code));

    let (status, body) = retry(&c, Uuid::new_v4(), Some("the node was down")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut conn = pool.acquire().await.unwrap();
    let recorded = holds::overrides(&mut conn, task).await.unwrap();
    drop(conn);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(
        (recorded[0].hold_code.as_str(), recorded[0].reason.as_str()),
        (code, "the node was down")
    );
    assert_eq!(recorded[0].attempt_id, first);
    assert_eq!(held_reason(&work(&c, task).await), None);
    let counts = no_grant::attempt_counts(&pool, task).await.unwrap();
    assert_eq!(counts.never_granted, 0, "the count starts again");
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [task], "{body}");
}

/// A cost hold is recorded by selection, not by B4's assessment, so the retry reads the
/// recorded hold. Overriding it is the owner's decision to spend more: the reason and the
/// lineage's spend at that moment are recorded, and only later spend counts against the cost
/// budget (`holds::cost_baseline`, which the selection budget snapshot subtracts).
#[tokio::test]
async fn an_owner_retry_overrides_a_recorded_cost_hold_and_starts_the_cost_budget_again() {
    let (mut c, predecessor) = failed_once(None).await;
    let (task, pool) = (c.env.lineage.task_id, c.env.env.pool.clone());
    let code = "lineage_cost_budget_exhausted";
    holds::hold(&pool, decision(&c, code, 1), (c.env.env.state.clock)()).await;
    owner_actions(&mut c).await;
    assert_eq!(held_reason(&work(&c, task).await).as_deref(), Some(code));
    assert_eq!(holds::cost_baseline(&pool, task).await.unwrap(), 0);

    let (status, body) = retry(&c, Uuid::new_v4(), Some("spend more on this one")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let mut conn = pool.acquire().await.unwrap();
    let recorded = holds::overrides(&mut conn, task).await.unwrap();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(
        (recorded[0].hold_code.as_str(), recorded[0].attempt_id),
        (code, predecessor)
    );
    assert_eq!(recorded[0].reason, "spend more on this one");
    drop(conn);
    assert_eq!(held_reason(&work(&c, task).await), None);
    // A later override that saw 700 microunits spent: the cost budget counts from there.
    let mut conn = pool.acquire().await.unwrap();
    let later = holds::Override {
        operation_id: Uuid::new_v4(),
        recorded_at_ms: recorded[0].recorded_at_ms + 1,
        spent_microunits: Some(700),
        ..recorded[0].clone()
    };
    let at = (c.env.env.state.clock)();
    holds::record_override(&mut conn, task, &later, at)
        .await
        .unwrap();
    drop(conn);
    assert_eq!(holds::cost_baseline(&pool, task).await.unwrap(), 700);
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [task], "{body}");
}

/// A case whose task's only attempt failed and keeps its claim, and whose project binding then
/// changed (2026-11-19: the owner raised a ceiling), so the offer path holds the task with
/// `replacement_project_revision_changed`: a retry would reuse the attempt's source, exported
/// under the revision it was offered at. The hold is recorded as the poll's decision records it.
/// Also returns the binding's new revision.
async fn binding_changed() -> (Case, Uuid, i64) {
    let mut c = setup().await;
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let predecessor = page.offers.remove(0).offered.offer.binding.attempt_id;
    native_offer::fail(&c, predecessor).await;
    // The fixture's binding is not at revision 1, so the change is relative.
    let revision: i64 = sqlx::query_scalar(
        "UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1 RETURNING revision",
    )
    .bind(c.env.lineage.project_id)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    let (pool, at) = (c.env.env.pool.clone(), (c.env.env.state.clock)());
    holds::hold(
        &pool,
        decision(&c, "replacement_project_revision_changed", 1),
        at,
    )
    .await;
    owner_actions(&mut c).await;
    (c, predecessor, revision)
}

/// Owner decision 2026-11-19: after a binding change a plain retry stays refused (its source
/// was exported under the old revision), and says that `fresh` starts the task again. A fresh
/// retry releases the held attempt, records who and why, and the next offer is a first attempt
/// from the project's current pin under the current binding. The old attempt, its retained
/// source and its release are untouched.
#[tokio::test]
async fn a_fresh_owner_retry_starts_a_task_held_by_a_binding_change_from_the_current_pin() {
    use aq_server::profiled_execution::replacement::{self, Assessment};
    let (c, predecessor, revision) = binding_changed().await;
    let (task, pool) = (c.env.lineage.task_id, c.env.env.pool.clone());
    let code = "replacement_project_revision_changed";
    let readiness = work(&c, task).await;
    assert_eq!(held_reason(&readiness).as_deref(), Some(code));
    let message = (readiness["reasons"].as_array().unwrap().iter())
        .find(|r| r["code"] == "task_held")
        .unwrap()["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("fresh=true"), "{message}");
    // A plain retry is refused, naming the option, and admits nothing.
    let (status, body) = retry(&c, Uuid::new_v4(), Some("try")).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], code);
    assert!(body["message"].as_str().unwrap().contains("fresh=true"));
    let retained_before: Value = sqlx::query_scalar(
        "SELECT state->'retention' FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(predecessor)
    .fetch_one(&pool)
    .await
    .unwrap();
    // The fresh retry releases it.
    let operation = Uuid::new_v4();
    let (status, body) = retry_with(&c, operation, Some("ceilings raised"), true).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["disposition"], "confirmed", "{body}");
    let (status, replay) = retry_with(&c, operation, Some("ceilings raised"), true).await;
    assert_eq!(status, 200, "{replay}");
    let recorded: Vec<(Uuid, Uuid, String, String, String, i64)> = sqlx::query_as("SELECT attempt_id,released_attempt_id,hold_code,reason,actor,project_revision FROM task_fresh_starts WHERE task_id=$1")
        .bind(task)
        .fetch_all(&pool)
        .await
        .unwrap();
    let want = (
        predecessor,
        predecessor,
        code,
        "ceilings raised",
        "owner@localhost",
        revision,
    );
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let r = &recorded[0];
    assert_eq!(
        (r.0, r.1, r.2.as_str(), r.3.as_str(), r.4.as_str(), r.5),
        want
    );
    let shown: Vec<_> = (system_comments(&c, task).await.into_iter())
        .filter(|(_, body)| body.contains("started this task fresh"))
        .collect();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(shown[0].1.ends_with("Owner's reason: ceilings raised"));
    let changed = sqlx::query("UPDATE task_fresh_starts SET reason='x' WHERE task_id=$1")
        .bind(task)
        .execute(&pool)
        .await;
    assert!(changed.is_err(), "fresh starts are immutable");
    // The hold is gone and the claim released; nothing of the attempt was deleted.
    assert_eq!(held_reason(&work(&c, task).await), None);
    assert!(swarm(&c).await.is_empty());
    let holding: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(predecessor)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!holding);
    let retained_after: Value = sqlx::query_scalar(
        "SELECT state->'retention' FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(predecessor)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_after, retained_before);
    assert_eq!(retained_after["kind"], "retained");
    let exports: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_launch_source_exports WHERE attempt_id=$1",
    )
    .bind(predecessor)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        exports, 1,
        "the previous attempt's source is still retained"
    );
    // The offer path sees a task without history: a first attempt, not a replacement.
    let state = &c.env.env.state;
    let now = (state.clock)().timestamp_millis() as u64;
    let assessed = replacement::assess_task_for_revision(state, task, now, revision as u64).await;
    assert_eq!(assessed.unwrap(), Assessment::Fresh);
    let counts = aq_server::profiled_execution::no_grant::attempt_counts(&pool, task).await;
    assert_eq!(
        (counts.as_ref().unwrap().started, counts.unwrap().history),
        (0, 1)
    );
    let (status, body) = poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [task], "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let next = page.offers[0].offered.offer.binding.attempt_id;
    assert_ne!(next, predecessor);
    // The new attempt's source is exported from the current pin, under the current binding.
    let pinned = replacement::predecessor_project_revision(&pool, next).await;
    assert_eq!(pinned.unwrap(), Some(revision as u64));
    let releases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_replacement_releases WHERE task_id=$1")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(releases, 1, "only the held attempt was released");
}

/// The option is for the binding-change hold alone: a task held by anything else, or not held
/// at all, is refused with a clear reason and nothing is admitted or recorded.
#[tokio::test]
async fn a_fresh_owner_retry_is_refused_for_any_other_hold() {
    let (mut c, predecessor) = budget_held("lineage_retry_budget_exhausted").await;
    let (task, pool) = (c.env.lineage.task_id, c.env.env.pool.clone());
    owner_actions(&mut c).await;
    let (status, body) = retry_with(&c, Uuid::new_v4(), Some("try"), true).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "lineage_retry_budget_exhausted");
    let message = body["message"].as_str().unwrap();
    assert!(message.starts_with("Fresh retry refused"), "{message}");
    assert!(message.contains("project binding changed"), "{message}");
    // A quarantine is refused the same way.
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'test quarantine','owner','{}',$3,1)")
        .bind(predecessor)
        .bind(task)
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = retry_with(&c, Uuid::new_v4(), None, true).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "stall_quarantined");
    let admitted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dashboard_operations WHERE kind='task.retry'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(admitted, 0);
    let fresh: i64 = sqlx::query_scalar("SELECT count(*) FROM task_fresh_starts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(fresh, 0);
}

#[tokio::test]
async fn a_changed_binding_can_start_fresh_after_its_old_lineage_budget_expires() {
    let (mut c, predecessor) = budget_held("lineage_time_budget_exhausted").await;
    owner_actions(&mut c).await;
    let (status, same) = retry_with(&c, Uuid::new_v4(), Some("same binding"), true).await;
    assert_eq!(status, 422, "{same}");
    assert_eq!(same["code"], "lineage_time_budget_exhausted");
    sqlx::query("UPDATE project_execution_bindings SET revision=revision+1 WHERE project_id=$1")
        .bind(c.env.lineage.project_id)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    let (status, plain) = retry(&c, Uuid::new_v4(), Some("old binding")).await;
    assert_eq!(status, 422, "{plain}");
    assert_eq!(plain["code"], "replacement_project_revision_changed");
    let operation = Uuid::new_v4();
    let (status, fresh) = retry_with(&c, operation, Some("pinned review source"), true).await;
    assert_eq!(status, 200, "{fresh}");
    assert_eq!(fresh["data"]["disposition"], "confirmed", "{fresh}");
    let recorded: Uuid = sqlx::query_scalar(
        "SELECT released_attempt_id FROM task_fresh_starts WHERE operation_id=$1",
    )
    .bind(operation)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(recorded, predecessor);
}
