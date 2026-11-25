//! Owner request 2026-11-24: changing which nodes may run a project (`allowed_node_ids`,
//! `allow_enrolled_nodes`, `allowed_worker_groups`) does not strand its held tickets. After
//! placement-only binding edits a failed attempt's replacement continues from its retained
//! commit under the current binding; any other change since the binding the attempt pinned
//! still holds it with `replacement_project_revision_changed`. The actual mounted poll,
//! export admission and publication run; the failure owner facts are synthetic.
use crate::common::native_offer::{Case, assembled, fail_and_release, poll, poll_offered, setup};
use aq_admission::control::{ControlReply, decode_reply};
use aq_server::profiled_execution::replacement::{self, Assessment};
use serde_json::{Value, json};
use uuid::Uuid;

const HOLD: &str = "replacement_project_revision_changed";

/// The one offer of a poll, as a worker polls.
async fn offered(c: &Case) -> Uuid {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    page.offers[0].offered.offer.binding.attempt_id
}
/// A case whose task's only attempt failed and was released, as the next poll would.
async fn failed() -> (Case, Uuid) {
    let c = setup().await;
    let predecessor = offered(&c).await;
    fail_and_release(&c, predecessor).await;
    (c, predecessor)
}
/// What `project.set` keeps: each revision's record in the binding history.
async fn record_history(c: &Case) {
    sqlx::query("INSERT INTO project_execution_binding_history(project_id,revision,record,actor_id,created_at_ms) SELECT project_id,revision,jsonb_build_object('revision',revision,'binding',binding,'validated_at_ms',validated_at_ms,'validation',binding_observation),(SELECT id FROM users ORDER BY id LIMIT 1),1 FROM project_execution_bindings WHERE project_id=$1 ON CONFLICT DO NOTHING")
        .bind(c.env.lineage.project_id)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
}
/// A `project.set` that applies `change` to the binding: its next revision, recorded.
async fn edit(c: &Case, change: impl FnOnce(&mut Value)) -> u64 {
    let (pool, project) = (&c.env.env.pool, c.env.lineage.project_id);
    record_history(c).await;
    let mut binding: Value =
        sqlx::query_scalar("SELECT binding FROM project_execution_bindings WHERE project_id=$1")
            .bind(project)
            .fetch_one(pool)
            .await
            .unwrap();
    change(&mut binding);
    let revision: i64 = sqlx::query_scalar("UPDATE project_execution_bindings SET revision=revision+1,binding=$2 WHERE project_id=$1 RETURNING revision")
        .bind(project)
        .bind(&binding)
        .fetch_one(pool)
        .await
        .unwrap();
    record_history(c).await;
    revision as u64
}
/// What the offer path decides for the task under `revision`.
async fn assess(c: &Case, revision: u64) -> Assessment {
    let state = &c.env.env.state;
    let now = (state.clock)().timestamp_millis() as u64;
    let task = c.env.lineage.task_id;
    (replacement::assess_task_for_revision(state, task, now, revision).await).unwrap()
}
fn replaces(assessment: &Assessment, predecessor: Uuid) -> bool {
    matches!(assessment, Assessment::Replace(r) if r.predecessor == predecessor)
}
/// The source export request `attempt` was launched with.
async fn export(c: &Case, attempt: Uuid) -> Value {
    sqlx::query_scalar("SELECT convert_from(e.request_bytes,'UTF8')::jsonb FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) WHERE x.attempt_id=$1")
        .bind(attempt)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}
/// `next` replaced `predecessor` from its retained commit, exported again under `revision`.
async fn continues(c: &Case, predecessor: Uuid, next: Uuid, revision: u64) {
    assert_ne!(next, predecessor);
    let (old, new) = (export(c, predecessor).await, export(c, next).await);
    assert_eq!(new["project_revision"], revision, "{new}");
    assert_eq!(new["base"], old["base"], "the retained commit");
    assert_eq!(new["source_retention_handle"], old["operation_id"], "{new}");
    for same in ["repository_id", "binding_generation", "logical_identity"] {
        assert_eq!(new[same], old[same], "{same}");
    }
    let released: Vec<Uuid> =
        sqlx::query_scalar("SELECT attempt_id FROM profiled_replacement_releases WHERE task_id=$1")
            .bind(c.env.lineage.task_id)
            .fetch_all(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(released, [predecessor], "a replacement, not a fresh start");
}

#[tokio::test]
async fn adding_a_node_lets_the_replacement_continue_from_the_retained_source() {
    let (c, predecessor) = failed().await;
    let revision = edit(&c, |b| {
        let nodes = b["allowed_node_ids"].as_array_mut().unwrap();
        nodes.push(json!(Uuid::new_v4()));
    })
    .await;
    assert!(replaces(&assess(&c, revision).await, predecessor));
    let next = offered(&c).await;
    continues(&c, predecessor, next, revision).await;
}

/// Two placement edits in a row: the replacement follows the current node list, so this node,
/// left out by the first edit and allowed again by the second, takes it only then.
#[tokio::test]
async fn successive_placement_edits_offer_the_replacement_to_the_newly_allowed_node() {
    let (c, predecessor) = failed().await;
    let other = Uuid::new_v4();
    let first = edit(&c, |b| {
        b["allowed_node_ids"] = json!([other]);
        b["allow_enrolled_nodes"] = json!(false);
        b["allowed_worker_groups"] = json!([]);
    })
    .await;
    assert!(replaces(&assess(&c, first).await, predecessor));
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assembled(&c).await;
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(
        attempts, 1,
        "this node may not take it under the first edit"
    );
    let node = c.env.env.node;
    let second = edit(&c, |b| b["allowed_node_ids"] = json!([other, node])).await;
    let next = offered(&c).await;
    continues(&c, predecessor, next, second).await;
}

#[tokio::test]
async fn a_ceiling_change_still_holds_the_replacement() {
    let (c, predecessor) = failed().await;
    let revision = edit(&c, |b| {
        let wall = b["ceilings"]["wall_time_ms"].as_u64().unwrap();
        b["ceilings"]["wall_time_ms"] = json!(wall - 1);
    })
    .await;
    assert_eq!(assess(&c, revision).await, Assessment::Blocked(HOLD));
    // The same binding again, only placed differently: nothing holds it.
    let revision = edit(&c, |b| {
        let wall = b["ceilings"]["wall_time_ms"].as_u64().unwrap();
        b["ceilings"]["wall_time_ms"] = json!(wall + 1);
        b["allowed_node_ids"] = json!([Uuid::new_v4(), c.env.env.node]);
    })
    .await;
    assert!(replaces(&assess(&c, revision).await, predecessor));
}

#[tokio::test]
async fn a_placement_edit_then_a_repository_generation_change_holds_the_replacement() {
    let (c, predecessor) = failed().await;
    let placed = edit(&c, |b| b["allowed_worker_groups"] = json!(["spare"])).await;
    assert!(replaces(&assess(&c, placed).await, predecessor));
    let revision = edit(&c, |b| {
        let generation = b["repository_binding_generation"].as_u64().unwrap();
        b["repository_binding_generation"] = json!(generation + 1);
    })
    .await;
    assert_eq!(assess(&c, revision).await, Assessment::Blocked(HOLD));
}
