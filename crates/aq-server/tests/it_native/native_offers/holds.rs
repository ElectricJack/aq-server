//! R6 (lessons W83, W87): a ready task the offer path does not offer for good (here the catalog
//! retry policy's `max_attempts` is used up) is held visibly: the hold is recorded on the task
//! once, named in exactly one system comment, shown in the task's work view and counted by the
//! swarm read (one shared definition), and the node takes other work. The hold clears when the
//! task gets a new attempt or is closed. The actual mounted poll, publication and human work
//! route run; the failure owner facts are synthetic (`fail_and_release`).
use crate::common::native_offer::{self, Case, fail_and_release, pin_policy, poll, setup};
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
/// A case whose task's only attempt failed and was released (attempt count 1).
async fn failed_once(max_attempts: Option<u32>) -> (Case, Uuid) {
    let mut c = setup().await;
    if let Some(n) = max_attempts {
        pin_policy(&mut c, |p| p.max_attempts = n).await;
    }
    let (status, body) = poll(&c).await;
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
    assert_eq!(reason, "replacement_policy_retry_budget_exhausted");
    assert!(*commented && cleared.is_none());
    assert_eq!(*comment_id, Uuid::new_v5(&task, b"aq.task-hold/1/0"));
    let comments = system_comments(&c, task).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert_eq!(comments[0].0, *comment_id);
    assert!(
        comments[0]
            .1
            .contains("the catalog retry policy's maximum attempts are used up (hold reason: replacement_policy_retry_budget_exhausted)"),
        "{comments:?}"
    );
    // Replaying the decision directly records and posts nothing more.
    let again = decision(&c, "replacement_policy_retry_budget_exhausted", 1);
    holds::hold(&pool, again, at).await;
    let mut conn = pool.acquire().await.unwrap();
    let other = decision(&c, "lineage_retry_budget_exhausted", 1).unwrap();
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
    assert_eq!(
        reason["hold_reason"],
        "replacement_policy_retry_budget_exhausted"
    );
    assert_eq!(reason["attempt_id"], serde_json::json!(predecessor));
    let project = c.env.lineage.project_id;
    assert_eq!(
        swarm(&c).await,
        [(
            project,
            "replacement_policy_retry_budget_exhausted".to_owned(),
            1
        )]
    );
    let mut conn = pool.acquire().await.unwrap();
    let elsewhere = holds::counts(&mut conn, &[Uuid::new_v4()]).await.unwrap();
    assert!(elsewhere.is_empty(), "another project has none");
    drop(conn);

    // The node takes the other ready task; the held one is never offered.
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = native_offer::poll(&c).await;
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
