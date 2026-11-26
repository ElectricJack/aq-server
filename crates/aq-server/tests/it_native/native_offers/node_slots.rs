//! Multi-slot admission uses the production HTTP, SQL, grant and lost-release owners.
//! Graph, Git and host observations are synthetic; no physical worker is qualified.
use super::*;
use aq_admission::{startup::NativeOffer, transport::Register};
use aq_server::profiled_execution::{lost, registration};
use common::native_offer::Case;

async fn auth(c: &Case) -> aq_server::worker_api::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", c.env.env.credential).parse().unwrap(),
    );
    aq_server::worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap()
}
pub(super) async fn register(c: &Case, slots: u32, boot: Option<Uuid>) -> Register {
    let raw: Value = sqlx::query_scalar(
        "SELECT state#>'{native,request}' FROM execution_nodes WHERE node_id=$1",
    )
    .bind(c.env.env.node)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    let mut request: Register = serde_json::from_value(raw).unwrap();
    request.operation_id = Uuid::new_v4();
    if let Some(boot) = boot {
        request.identity.boot_id = boot;
    }
    registration::register_slots(&c.env.env.state, &auth(c).await, &request, Some(slots))
        .await
        .unwrap();
    request
}
async fn set_max_agents(c: &Case, count: i32) {
    sqlx::query("UPDATE worker_settings SET max_agents=$2 WHERE kind='node' AND id=$1")
        .bind(c.env.env.node.to_string())
        .bind(count)
        .execute(&c.env.env.pool)
        .await
        .unwrap();
}

async fn authority_state(c: &Case, attempt: Uuid) -> Value {
    sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}

pub(super) async fn poll_slot(c: &Case, slot: u32) -> Vec<NativeOffer> {
    poll_status(c, slot).await.1
}
/// Polls until `slot` is offered work, as a worker does. A poll waits only 750 ms
/// for offer assembly (`producer::workflow::poll`); slower assembly (a loaded CI
/// runner) continues in the background, further polls of the slot return an empty
/// page until it finishes, and a later poll redelivers the published offer. A
/// fixed number of back-to-back polls therefore cannot wait for it.
pub(super) async fn offered(c: &Case, slot: u32) -> NativeOffer {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let mut offers = poll_slot(c, slot).await;
        if !offers.is_empty() {
            return offers.remove(0);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "slot {slot} received no offer in 30 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn publication_retries_the_same_assembly_after_a_slot_lock_timeout() {
    use std::time::Duration;

    let c = setup().await;
    let pool = &c.env.env.pool;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM node_allocation WHERE node_id=$1 FOR UPDATE")
        .bind(c.env.env.node)
        .execute(&mut *lock)
        .await
        .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let release = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE NOT granted AND $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(pool)
            .await
            .unwrap()
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Publication's first transaction has a 2s lock timeout.
        tokio::time::sleep(Duration::from_millis(2_050)).await;
        lock.commit().await.unwrap();
    };
    let (first, ()) = tokio::join!(poll_slot(&c, 1), release);
    assert!(first.is_empty());
    common::native_offer::assembled(&c).await;
    let claimed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_attempts WHERE node_id=$1 AND holds_task",
    )
    .bind(c.env.env.node)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        claimed, 1,
        "the first poll publishes without starting another assembly"
    );
}

#[tokio::test]
async fn a_refused_publication_releases_staging_and_retains_the_artifacts() {
    let c = setup().await;
    let pool = &c.env.env.pool;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM node_allocation WHERE node_id=$1 FOR UPDATE")
        .bind(c.env.env.node)
        .execute(&mut *lock)
        .await
        .unwrap();
    assert!(poll_slot(&c, 1).await.is_empty());
    // Keep the publication lock through every bounded retry of this assembly.
    common::native_offer::assembled(&c).await;
    let (stages, closed, pins, objects, attempts): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM admission_offer_stages),
                (SELECT count(*) FROM admission_offer_stages WHERE closed_at_ms IS NOT NULL),
                (SELECT count(*) FROM admission_offer_stage_pins),
                (SELECT count(*) FROM admission_artifacts WHERE offer_staged),
                (SELECT count(*) FROM execution_attempts WHERE node_id=$1 AND holds_task)",
    )
    .bind(c.env.env.node)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((stages, closed, pins, attempts), (1, 1, 0, 0));
    assert!(objects > 0, "finished assemblies retain their exact bytes");
    let (charged, legacy): (i64, i64) = sqlx::query_as(
        "SELECT usage.scope_rows,
            (SELECT count(*) FROM admission_artifacts a WHERE a.owner_key=s.project_id::text
             AND NOT a.offer_staged AND a.created_at_ms>=$1-600000
             AND NOT EXISTS(SELECT 1 FROM profiled_launch_artifacts l WHERE l.artifact_id=a.artifact_id))
         FROM admission_offer_stages s CROSS JOIN LATERAL sealed_history_usage('admission_artifacts',s.project_id,$1) usage",
    )
    .bind((c.env.env.state.clock)().timestamp_millis())
    .fetch_one(pool).await.unwrap();
    assert_eq!(
        charged, legacy,
        "the charged rows no longer include the refused assembly"
    );
    lock.commit().await.unwrap();
    let next = offered(&c, 1).await;
    let claim: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(next.offered.offer.binding.attempt_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(
        claim,
        "the normal next poll can admit one replacement offer"
    );
}

#[tokio::test]
async fn running_siblings_redeliver_to_their_slot_restart_and_release_independently() {
    let mut c = setup().await;
    let node = c.env.env.node;
    register(&c, 2, None).await;
    c.mode.store(7, Ordering::SeqCst);
    let first = offered(&c, 1).await;
    assert_eq!(
        offered(&c, 1).await.offered.offer.binding,
        first.offered.offer.binding
    );
    let a = super::lost::running(&mut c, &first).await;
    // Reopen the production store while A runs: reservations and boot are durable.
    c.env.env.state.store =
        aq_server::store::Store::connect_with((*c.env.env.pool.connect_options()).clone())
            .await
            .unwrap();
    let second = offered(&c, 2).await;
    assert_ne!(
        first.offered.offer.binding.attempt_id,
        second.offered.offer.binding.attempt_id
    );
    assert!(poll_slot(&c, 1).await.is_empty());
    assert_eq!(
        offered(&c, 2).await.offered.offer.binding,
        second.offered.offer.binding
    );
    assert!(poll_slot(&c, 3).await.is_empty());
    let b = super::lost::running(&mut c, &second).await;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let slots = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((slots.total, slots.occupied, slots.idle), (2, 2, 0));
    drop(conn);
    set_max_agents(&c, 1).await;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let lowered = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((lowered.total, lowered.occupied, lowered.idle), (1, 2, 0));
    drop(conn);
    assert!(poll_slot(&c, 3).await.is_empty());
    set_max_agents(&c, 2).await;
    // A same-boot count refresh is accepted under load; Settings still caps at two.
    register(&c, 3, None).await;
    assert!(poll_slot(&c, 3).await.is_empty());
    let old = c.request.identity.boot_id;
    let new = Uuid::new_v4();
    register(&c, 2, Some(new)).await;
    let mut tx = c.env.env.pool.begin().await.unwrap();
    assert!(
        registration::current_runtime_locked(&c.env.env.state, &mut tx, node, old)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let before = authority_state(&c, b).await;
    let auth = auth(&c).await;
    let req = super::lost::crash_request(a);
    lost::release(&c.env.env.state, &auth, &req).await.unwrap();
    lost::release(&c.env.env.state, &auth, &req).await.unwrap();
    let after = authority_state(&c, b).await;
    assert_eq!(before, after, "late/duplicate A evidence must not change B");
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let slots = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert!(slots.free(1));
    assert!(!slots.free(2));
    sqlx::query("UPDATE worker_settings SET max_agents=1 WHERE kind='node' AND id=$1")
        .bind(node.to_string())
        .execute(&mut *conn)
        .await
        .unwrap();
    let slots = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((slots.total, slots.occupied, slots.idle), (1, 1, 0));
    assert!(!slots.free(1));
    drop(conn);
    lost::release(&c.env.env.state, &auth, &super::lost::crash_request(b))
        .await
        .unwrap();
    let held:i64=sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE node_id=$1 AND native_attempt_holds_compute(attempt_id,node_id)").bind(node).fetch_one(&c.env.env.pool).await.unwrap();
    assert_eq!(held, 0);
}

#[tokio::test]
async fn an_old_worker_is_one_slot_even_when_the_setting_is_two() {
    let c = setup().await;
    c.mode.store(7, Ordering::SeqCst);
    assert!(poll_slot(&c, 2).await.is_empty());
    let first = offered(&c, 1).await;
    assert_eq!(
        offered(&c, 1).await.offered.offer.binding,
        first.offered.offer.binding
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE node_id=$1")
            .bind(c.env.env.node)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 1);
}

#[tokio::test]
async fn new_boot_accepts_old_boot_stop_evidence_while_lost_sibling_resolves_separately() {
    let mut c = setup().await;
    register(&c, 2, None).await;
    c.mode.store(7, Ordering::SeqCst);
    let first = offered(&c, 1).await;
    let second = offered(&c, 2).await;
    let start = super::lost::running_started(&mut c, &first).await;
    let b = super::lost::running(&mut c, &second).await;
    register(&c, 2, Some(Uuid::new_v4())).await;
    let auth = auth(&c).await;
    let request = common::native_started::stop_request(&start);
    let receipt = aq_server::profiled_execution::stopped::record(&c.env.env.state, &auth, &request)
        .await
        .unwrap();
    let again = aq_server::profiled_execution::stopped::record(&c.env.env.state, &auth, &request)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(receipt).unwrap(),
        serde_json::to_value(again).unwrap()
    );
    let before = authority_state(&c, start.binding.attempt_id).await;
    lost::release(&c.env.env.state, &auth, &super::lost::crash_request(b))
        .await
        .unwrap();
    let after = authority_state(&c, start.binding.attempt_id).await;
    assert_eq!(before, after);
}

#[tokio::test]
async fn never_started_lapse_frees_only_that_slots_attempt() {
    let mut c = setup().await;
    register(&c, 2, None).await;
    c.mode.store(7, Ordering::SeqCst);
    let first = offered(&c, 1).await;
    let second = offered(&c, 2).await;
    let (_, _, prepare, _) = common::native_offer::prepare_request(&mut c, &first).await;
    let grant = aq_server::profiled_execution::initial_grant::prepare(
        &c.env.env.state,
        &auth(&c).await,
        &prepare,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let b = super::lost::running(&mut c, &second).await;
    let at = grant.original.grant.expires_at_ms as i64 + 1;
    c.env.env.state.clock =
        std::sync::Arc::new(move || chrono::DateTime::from_timestamp_millis(at).unwrap());
    aq_server::profiled_execution::closure::settlement::settle_and_release(
        &c.env.env.state,
        first.offered.offer.binding.attempt_id,
        None,
    )
    .await
    .unwrap();
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let view = aq_machine_control::slots::view(&mut conn, c.env.env.node)
        .await
        .unwrap();
    assert!(view.free(1));
    assert!(!view.free(2));
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(b)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn count_one_redelivers_its_full_reservation_and_admits_no_second_attempt() {
    let c = setup().await;
    register(&c, 2, None).await;
    set_max_agents(&c, 1).await;
    c.mode.store(7, Ordering::SeqCst);
    let first = offered(&c, 1).await;
    assert_eq!(
        offered(&c, 1).await.offered.offer.binding,
        first.offered.offer.binding
    );
    assert!(poll_slot(&c, 2).await.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_attempts WHERE node_id=$1")
            .bind(c.env.env.node)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap(),
        1
    );
}

/// One request of a slot's poll: the status and the offers it carried.
async fn poll_status(c: &Case, slot: u32) -> (u16, Vec<NativeOffer>) {
    let response = aq_server::build_worker_router(c.env.env.state.clone())
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/poll")
                .header("authorization", format!("Bearer {}", c.env.env.credential))
                .header("content-type", "application/json")
                .header("aq-node-slot", slot.to_string())
                .body(axum::body::Body::from(
                    serde_json::to_vec(&c.request).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    match decode_reply(&bytes) {
        Ok(ControlReply::Poll(page)) if status == 200 => (status, page.offers),
        _ => panic!("slot {slot}: {status} {}", String::from_utf8_lossy(&bytes)),
    }
}

/// The live fault of 2026-11-17: the slots of one node poll at the same time.
/// No poll is refused (409) or told the node row is contended (503), each ready
/// task is offered to exactly one slot, and each slot is redelivered its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_slots_of_one_node_poll_at_once_without_a_refused_or_contended_poll() {
    let c = std::sync::Arc::new(setup().await);
    set_max_agents(&c, 8).await;
    register(&c, 8, None).await;
    c.mode.store(7, Ordering::SeqCst);
    let mut held = std::collections::BTreeMap::new();
    let started = std::time::Instant::now();
    // An assembly outlives the poll that started it; the next polls deliver it.
    while held.len() < 2 || started.elapsed() < std::time::Duration::from_secs(3) {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(40),
            "{held:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let polls = (1..=8u32).map(|slot| {
            let c = c.clone();
            tokio::spawn(async move { (slot, poll_status(&c, slot).await) })
        });
        for poll in polls.collect::<Vec<_>>() {
            let (slot, (status, offers)) = poll.await.unwrap();
            assert_eq!(status, 200, "slot {slot} was refused");
            assert!(offers.len() <= 1);
            for offer in offers {
                let attempt = offer.offered.offer.binding.attempt_id;
                assert_eq!(*held.entry(attempt).or_insert(slot), slot, "moved slot");
            }
        }
    }
    // The fixture's Graph lists two ready tasks: two slots hold one each.
    let slots = held.values().collect::<std::collections::BTreeSet<_>>();
    assert_eq!((held.len(), slots.len()), (2, 2), "{held:?}");
    let tasks: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT task_id) FROM execution_attempts WHERE node_id=$1 AND holds_task",
    )
    .bind(c.env.env.node)
    .fetch_one(&c.env.env.pool)
    .await
    .unwrap();
    assert_eq!(tasks, 2);
}

/// A ready task of a project that does not admit the polling node is no offer
/// and no refusal: the poll is a quiet success, as with nothing ready.
#[tokio::test]
async fn a_poll_with_nothing_this_node_may_take_is_a_quiet_success() {
    let c = setup().await;
    sqlx::query("UPDATE project_execution_bindings SET binding=binding||jsonb_build_object('allow_enrolled_nodes',false,'allowed_worker_groups','[]'::jsonb,'allowed_node_ids',jsonb_build_array($1::text))")
        .bind(Uuid::new_v4().to_string())
        .execute(&c.env.env.pool)
        .await
        .unwrap();
    for _ in 0..3 {
        let (status, offers) = poll_status(&c, 1).await;
        assert_eq!((status, offers.len()), (200, 0));
    }
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}
