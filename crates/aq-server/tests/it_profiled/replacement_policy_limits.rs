//! Spec 0007 phase 1 (S9, owner answer Q1): a successor the predecessor's
//! pinned selection policy would refuse is blocked before selection, so it is
//! skipped instead of failing the poll. An actual mounted publication pins the
//! policy; the failure owner facts are synthetic.
use crate::common::native_offer::{fail_and_release, pin_policy, poll, setup};
use aq_admission::control::{ControlReply, decode_reply};
use aq_server::profiled_execution::replacement::{self, Assessment};

#[tokio::test]
async fn pinned_policy_budget_blocks_before_selection() {
    let mut c = setup().await;
    pin_policy(&mut c, |p| p.max_attempts = 1).await;
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    let predecessor = page.offers.remove(0).offered.offer.binding.attempt_id;
    fail_and_release(&c, predecessor).await;
    let (state, pool) = (&c.env.env.state, &c.env.env.pool);
    let task = c.env.lineage.task_id;
    let revision = replacement::predecessor_project_revision(pool, predecessor)
        .await
        .unwrap()
        .unwrap();
    let current: i64 =
        sqlx::query_scalar("SELECT revision FROM project_execution_bindings WHERE project_id=$1")
            .bind(c.env.lineage.project_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(revision, current as u64);
    let now = || (state.clock)().timestamp_millis() as u64;
    // B4 (3 attempts) admits it; the pinned policy (1 attempt) does not.
    assert!(matches!(
        replacement::assess_task(state, task, now()).await.unwrap(),
        Assessment::Replace(_)
    ));
    assert_eq!(
        replacement::assess_task_for_revision(state, task, now(), revision)
            .await
            .unwrap(),
        Assessment::Blocked("replacement_policy_retry_budget_exhausted")
    );
    // An owner stop is not charged (Q1): the same lineage is replaceable.
    sqlx::query("INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) VALUES($1,$2,'owner_stop',now())")
        .bind(predecessor)
        .bind(c.env.env.node)
        .execute(pool)
        .await
        .unwrap();
    let Assessment::Replace(r) =
        replacement::assess_task_for_revision(state, task, now(), revision)
            .await
            .unwrap()
    else {
        panic!("an exempt handoff leaves the pinned policy budget");
    };
    assert_eq!((r.attempts_started, r.exempt_attempts), (1, 1));
}
