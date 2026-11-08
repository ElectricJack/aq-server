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
async fn running(c: &mut Case, offer: &NativeOffer) -> Uuid {
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
    g.grant.binding.attempt_id
}
async fn crash_release(c: &Case, attempt: Uuid) -> aq_protocol::LostAttemptReleased {
    let request = LostAttemptRelease {
        schema_version: 1,
        attempt_id: attempt,
        proof: StopProof::CgroupTeardown {
            invocation_id: "0123456789abcdef0123456789abcdef".into(),
            unit: "aq-node@ms1.service".into(),
        },
    };
    lost::release(&c.env.env.state, &auth(c).await, &request)
        .await
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
            let (status, body) = native_offer::poll(&c).await;
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
    let (status, body) = native_offer::poll(&c).await;
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
        comments[0].1.contains("lineage_retry_budget_exhausted")
            && comments[0]
                .1
                .contains("retry budget is exhausted after lost attempts"),
        "{comments:?}"
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
