//! Swarm phase 1, D2 ruling C (#117 re-review): an attempt lost after a crash is released by its
//! node; after the third such crash its task's retry budget is exhausted, so B4 holds the task.
//! The machine is free (its processes are proven dead): the next poll offers other work, never
//! the held task, and the hold is named in a system comment on the task. The actual mounted
//! poll, publication and grant owners run; the host is the synthetic native peer.
use crate::common::native_offer::{self, Case};
use aq_admission::{
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_protocol::{LostAttemptRelease, StopProof};
use aq_server::{
    profiled_execution::{initial_grant, lost, started},
    worker_api::{self, AuthenticatedNode},
};
use serde_json::Value;
use std::sync::atomic::Ordering;
use uuid::Uuid;

async fn auth(c: &Case) -> AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    let bearer = format!("Bearer {}", c.env.env.credential);
    headers.insert("authorization", bearer.parse().unwrap());
    worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap()
}
fn offers(body: &Value) -> Vec<NativeOffer> {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    page.offers
}
/// The offer is accepted, granted and started: the attempt runs (its host later lost).
pub(super) async fn running(c: &mut Case, offer: &NativeOffer) -> Uuid {
    running_started(c, offer).await.binding.attempt_id
}
pub(super) async fn running_started(
    c: &mut Case,
    offer: &NativeOffer,
) -> aq_admission::started::Request {
    let (_, _, request, _) = native_offer::prepare_request(c, offer).await;
    let state = c.env.env.state.clone();
    let auth = auth(c).await;
    let grant = initial_grant::prepare(&state, &auth, &request, Uuid::new_v4())
        .await
        .unwrap();
    let g = &grant.original;
    let mut start: aq_admission::started::Request = serde_json::from_str(include_str!(
        "../../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    let runtime = &state
        .config
        .profiled_execution
        .as_ref()
        .unwrap()
        .qualified_runtimes[0];
    start.operation_id = Uuid::new_v4();
    start.binding = g.grant.binding.clone();
    start.grant_id = g.grant.grant_id;
    (start.guardian_id, start.launch_id) = (g.guardian_id, g.launch_id);
    start.reservation_id = g.reservation_id;
    start.runtime = runtime.runtime.clone();
    start.qualification = runtime.qualification.clone();
    start.journal_revision = 3;
    let intent = &mut start.observation.guardian.intent;
    intent.launch_id = g.launch_id;
    intent.session = start.binding.session.clone();
    intent.manifest_digest = start.binding.launch_manifest_digest.clone();
    start.observation_digest = start.observation.digest().unwrap();
    started::record(&state, &auth, &start).await.unwrap();
    start
}
pub(super) fn crash_request(attempt: Uuid) -> LostAttemptRelease {
    LostAttemptRelease {
        schema_version: 1,
        attempt_id: attempt,
        proof: StopProof::CgroupTeardown {
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: "aq-node@ms1.service".into(),
        },
    }
}
/// The server state over a pool of one connection (#117 review): the release transaction holds
/// it, so any read of the release on a second pool connection times out instead of passing.
async fn one_connection(c: &Case) -> (aq_server::AppState, sqlx::PgPool) {
    let options = (*c.env.env.pool.connect_options()).clone();
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect_with(options)
        .await
        .unwrap();
    let mut state = c.env.env.state.clone();
    state.store = state.store.clone().with_pool_for_testing(small.clone());
    (state, small)
}
async fn crash_release(c: &Case, attempt: Uuid) -> aq_protocol::LostAttemptReleased {
    let (state, small) = one_connection(c).await;
    let auth = auth(c).await;
    // The bound is real: with its one connection taken, the release cannot run.
    let held = small.acquire().await.unwrap();
    let starved = lost::release(&state, &auth, &crash_request(attempt)).await;
    assert!(starved.is_err(), "a one-connection pool is exhausted");
    drop(held);
    let request = crash_request(attempt);
    let released = lost::release(&state, &auth, &request);
    tokio::time::timeout(std::time::Duration::from_secs(30), released)
        .await
        .expect("the release holds no second pool connection")
        .unwrap()
}

#[tokio::test]
async fn a_budget_held_lost_task_frees_the_machine_for_other_work() {
    let (mut c, first) = native_offer::published().await;
    let task = c.env.lineage.task_id;
    let mut offer = first;
    for round in 1..=3 {
        assert_eq!(offer.offered.offer.manifest.bindings.task_id, task);
        let attempt = running(&mut c, &offer).await;
        let released = crash_release(&c, attempt).await;
        if round < 3 {
            assert!(released.released, "crash {round} is retried: {released:?}");
            assert!(
                super::holds::hold_rows(&c, task).await.is_empty(),
                "no hold yet"
            );
            let (status, body) = native_offer::poll_offered(&c).await;
            assert_eq!(status, 200, "{body}");
            offer = offers(&body).remove(0);
            continue;
        }
        // The third crash exhausts the retry budget: B4 holds the task.
        assert_eq!(
            (
                released.charged,
                released.released,
                released.held.as_deref()
            ),
            (true, false, Some("lineage_retry_budget_exhausted")),
            "{released:?}"
        );
    }
    let pool = &c.env.env.pool;
    let busy = aq_server::test_support::node_busy(&c.env.env.state, &auth(&c).await).await;
    assert_eq!(
        busy.map_err(|e| format!("{e:?}")),
        Ok(false),
        "the machine is free"
    );
    // The next poll offers the other ready task, never the held one.
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "never a 409: {body}");
    let offered = offers(&body);
    assert_eq!(offered.len(), 1, "{body}");
    assert_eq!(
        offered[0].offered.offer.manifest.bindings.task_id,
        c.second_task
    );
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        offers(&body)
            .iter()
            .all(|o| o.offered.offer.manifest.bindings.task_id != task),
        "the held task is not re-offered: {body}"
    );
    // The hold is visible on the task: one system comment naming its reason.
    let comments: Vec<(String, String)> = sqlx::query_as(
        "SELECT author_kind,body FROM task_comments WHERE task_id=$1 AND author_kind='system'",
    )
    .bind(task)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].1.contains(
            "was lost after a crash, and its retry budget is exhausted (hold reason: lineage_retry_budget_exhausted)"
        ),
        "{comments:?}"
    );
    // The task's own hold (R6) is that one comment's: the polls since add none.
    let rows = super::holds::hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "lineage_retry_budget_exhausted");
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT comment_id FROM task_comments WHERE task_id=$1 AND author_kind='system'",
    )
    .bind(task)
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(ids, [rows[0].1]);
    assert!(rows[0].2, "marked on record");
    assert_eq!(super::holds::agree(&c, &[task, c.second_task]).await, 1);
    let readiness = super::holds::work(&c, task).await;
    assert!(
        readiness["reasons"]
            .to_string()
            .contains("\"hold_reason\":\"lineage_retry_budget_exhausted\""),
        "{readiness}"
    );
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT held_reason FROM lost_attempt_releases l JOIN execution_attempts a USING(attempt_id) WHERE a.task_id=$1 AND l.held_reason IS NOT NULL",
    )
    .bind(task)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("lineage_retry_budget_exhausted"));
}

/// W83: a lost-path hold for a quarantine is counted by the swarm read exactly as the task views
/// show it, with its one comment, while the node takes other work.
#[tokio::test]
async fn a_quarantined_lost_hold_is_counted_as_the_task_views_show_it() {
    let (mut c, offer) = native_offer::published().await;
    let task = c.env.lineage.task_id;
    let attempt = running(&mut c, &offer).await;
    let pool = c.env.env.pool.clone();
    sqlx::query("INSERT INTO stall_quarantines(attempt_id,lineage_id,reason,escalation,held,action_id,created_at_ms) VALUES($1,$2,'test quarantine','owner','{}',$3,1)")
        .bind(attempt).bind(task).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let released = crash_release(&c, attempt).await;
    assert_eq!(
        released.held.as_deref(),
        Some("stall_quarantined"),
        "{released:?}"
    );
    let rows = super::holds::hold_rows(&c, task).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "stall_quarantined");
    assert_eq!(
        rows[0].1,
        aq_machine_control::lost::hold_comment_id(attempt)
    );
    c.mode.store(7, Ordering::SeqCst);
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    let offered: Vec<Uuid> = (offers(&body).iter())
        .map(|o| o.offered.offer.manifest.bindings.task_id)
        .collect();
    assert_eq!(offered, [c.second_task], "{body}");
    assert_eq!(super::holds::agree(&c, &[task, c.second_task]).await, 1);
    let comments = super::holds::system_comments(&c, task).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0]
            .1
            .contains("and the stall ladder quarantined it"),
        "{comments:?}"
    );
}

/// The offer is accepted and granted, and the attempt never started.
async fn granted(c: &mut Case, offer: &NativeOffer) -> Uuid {
    let (_, _, request, _) = native_offer::prepare_request(c, offer).await;
    let state = c.env.env.state.clone();
    let auth = auth(c).await;
    let grant = initial_grant::prepare(&state, &auth, &request, Uuid::new_v4())
        .await
        .unwrap();
    grant.original.grant.binding.attempt_id
}
/// Revokes the case's node (its machine, when it belongs to one) in one transaction.
async fn revoke(c: &Case) -> u32 {
    let node = c.env.env.node;
    let pool = &c.env.env.pool;
    let key: Option<String> = sqlx::query_scalar("SELECT machine_key FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_one(pool)
        .await
        .unwrap();
    let target = key.unwrap_or_else(|| format!("node:{node}"));
    let mut tx = pool.begin().await.unwrap();
    let n = aq_machine_control::revoke_machine(&mut tx, &target, chrono::Utc::now())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    n
}
/// The attempt is released as a revoked node's: recorded, stopped, its claim freed, not charged.
async fn assert_released(c: &Case, attempt: Uuid) {
    let pool = &c.env.env.pool;
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(!held, "the revoked node's attempt no longer holds its task");
    let (reason, charged): (String, bool) =
        sqlx::query_as("SELECT reason,charged FROM lost_attempt_releases WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!((reason.as_str(), charged), ("node_revoked", false));
    let (status, stop): (String, String) = sqlx::query_as(
        "SELECT state->>'status',state#>>'{stop,kind}' FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), stop.as_str()), ("finished", "stopped"));
    let releases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profiled_replacement_releases WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(releases, 1, "the task is retryable as after a lost attempt");
}
async fn release_count(c: &Case) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM lost_attempt_releases")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn revoking_a_node_releases_its_granted_attempt() {
    let (mut c, offer) = native_offer::published().await;
    let attempt = granted(&mut c, &offer).await;
    assert_eq!(revoke(&c).await, 1);
    assert_released(&c, attempt).await;
}

#[tokio::test]
async fn revoking_a_node_releases_its_running_attempt_and_replays_harmlessly() {
    let (mut c, offer) = native_offer::published().await;
    let attempt = running(&mut c, &offer).await;
    assert_eq!(revoke(&c).await, 1);
    assert_released(&c, attempt).await;
    // A replay revokes no live node and releases nothing more; the function itself is idempotent.
    assert_eq!(revoke(&c).await, 0);
    let again: i32 = sqlx::query_scalar("SELECT release_revoked_node_attempts($1,now())")
        .bind(c.env.env.node)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(release_count(&c).await, 1);
    assert_released(&c, attempt).await;
}

#[tokio::test]
async fn revoking_a_node_without_held_attempts_releases_nothing() {
    let (c, _offer) = native_offer::published().await;
    assert_eq!(revoke(&c).await, 1);
    assert_eq!(release_count(&c).await, 0);
}
