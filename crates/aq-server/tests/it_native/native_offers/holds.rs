//! R6 (lessons W83, W87): a ready task the offer path does not offer for good (here the catalog
//! retry policy's `max_attempts` is used up) is held visibly: the hold is recorded on the task
//! once, named in exactly one system comment, shown in the task's work view and counted for the
//! swarm read, and the node takes other work. The actual mounted poll, publication and human
//! work route run; the failure owner facts are synthetic (`fail_and_release`).
use crate::common::native_offer::{self, Case, fail_and_release, pin_policy, poll, setup};
use aq_admission::control::{ControlReply, decode_reply};
use aq_machine_control::holds;
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
pub(super) async fn hold_rows(c: &Case, task: Uuid) -> Vec<(String, Uuid, bool)> {
    sqlx::query_as("SELECT reason,comment_id,commented FROM task_holds WHERE task_id=$1")
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

#[tokio::test]
async fn a_task_past_its_policy_max_attempts_is_held_visibly_once_and_other_work_is_offered() {
    let mut c = setup().await;
    pin_policy(&mut c, |p| p.max_attempts = 1).await;
    let (task, project) = (c.env.lineage.task_id, c.env.lineage.project_id);
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let predecessor = page.offers.remove(0).offered.offer.binding.attempt_id;
    // Before the decision: no hold, no comment, and the work view names none.
    let readiness = work(&c, task).await;
    assert!(
        !readiness["reasons"].to_string().contains("task_held"),
        "{readiness}"
    );
    fail_and_release(&c, predecessor).await;
    assert!(hold_rows(&c, task).await.is_empty(), "nothing is held yet");
    // A refusal that is only a wait records nothing.
    let pool = &c.env.env.pool;
    let at = (c.env.env.state.clock)();
    holds::hold(
        pool,
        project,
        task,
        "replacement_prior_execution_uncertain",
        None,
        at,
    )
    .await;
    assert!(hold_rows(&c, task).await.is_empty(), "a wait is not a hold");

    // The decision, made by the actual poll, twice (a replay): no offer, one hold, one comment.
    for _ in 0..2 {
        let (status, body) = poll(&c).await;
        assert_eq!(status, 200, "{body}");
        assert!(offered_tasks(&body).is_empty(), "{body}");
    }
    let rows = hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let (reason, comment_id, commented) = &rows[0];
    assert_eq!(reason, "replacement_policy_retry_budget_exhausted");
    assert!(commented);
    assert_eq!(*comment_id, Uuid::new_v5(&task, b"aq.task-hold/1"));
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
    holds::hold(
        pool,
        project,
        task,
        "replacement_policy_retry_budget_exhausted",
        None,
        at,
    )
    .await;
    let mut conn = pool.acquire().await.unwrap();
    let again = holds::note(
        &mut conn,
        project,
        task,
        "lineage_retry_budget_exhausted",
        None,
        None,
        at,
    );
    assert_eq!(again.await.unwrap(), None, "one hold per attempt history");
    assert_eq!(system_comments(&c, task).await.len(), 1);
    assert_eq!(hold_rows(&c, task).await.len(), 1);

    // The task's work view shows the hold as recorded.
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
    // The swarm read counts it for the project.
    let counts = holds::counts(&mut conn, &[project], at).await.unwrap();
    assert_eq!(
        counts,
        [(
            project,
            "replacement_policy_retry_budget_exhausted".to_owned(),
            1
        )]
    );
    assert!(
        (holds::counts(&mut conn, &[Uuid::new_v4()], at)
            .await
            .unwrap())
        .is_empty(),
        "another project has none"
    );
    drop(conn);

    // The node takes the other ready task; the held one is never offered.
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered_tasks(&body), [c.second_task], "{body}");
    assert_eq!(system_comments(&c, task).await.len(), 1);
}
