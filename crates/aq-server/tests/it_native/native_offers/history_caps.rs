//! Spec 0008 phase A history holds on the poll. The offer rate limits (30 per node,
//! 240 per project, 960 per server in an hour) and the worker report rate limits (720
//! per node, 11,520 per server) were removed on 2026-11-13 (owner): more than that in
//! an hour holds nothing. A live cap still holds a project.
use crate::common::{
    native_offer::{self, Case},
    sealed,
};
use aq_admission::control::{ControlReply, decode_reply};
use aq_server::worker_api;
use uuid::Uuid;

async fn node(c: &Case) -> Uuid {
    let mut headers = axum::http::HeaderMap::new();
    let bearer = format!("Bearer {}", c.env.env.credential);
    headers.insert("authorization", bearer.parse().unwrap());
    worker_api::authenticate_context(&c.env.env.state, &headers)
        .await
        .unwrap()
        .id()
}
fn offered(body: &serde_json::Value) -> usize {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    page.offers.len()
}
/// What the poll gate decides for this node and project, plus every hold it sees.
async fn gate(c: &Case, node: Uuid, now: i64) -> (bool, bool, Vec<String>) {
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    let project = c.env.lineage.project_id;
    let gate = aq_history_caps::poll_gate(
        &mut conn,
        node,
        &[project],
        aq_history_caps::Snapshot::new(now as u64, None),
    )
    .await
    .unwrap();
    let holds = aq_history_caps::holds(
        &mut conn,
        &[node],
        &[project],
        aq_history_caps::Snapshot::new(now as u64, None),
    )
    .await
    .unwrap();
    (
        gate.stop.is_some(),
        gate.is_held(project),
        holds.iter().map(|h| h.message()).collect(),
    )
}

/// More offers in an hour than the removed limits allowed, for the node (31), its
/// project (241) and the server (961): the gate holds nothing and the poll offers work.
#[tokio::test]
async fn a_node_a_project_and_the_server_past_the_old_offer_rates_are_not_held() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    let project = c.env.lineage.project_id;
    sealed::offers(pool, id, project, now - 1_000, 31).await;
    assert_eq!(gate(&c, id, now).await, (false, false, vec![]), "node: 31");
    sealed::spread_offers(pool, Some(project), 210, now - 1_000).await;
    assert_eq!(
        gate(&c, id, now).await,
        (false, false, vec![]),
        "project: 241"
    );
    sealed::spread_offers(pool, None, 720, now - 1_000).await;
    assert_eq!(
        gate(&c, id, now).await,
        (false, false, vec![]),
        "server: 961"
    );
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered(&body), 1, "{body}");
    assert!(
        !body.to_string().contains("Offer rate limit"),
        "no offer-rate hold: {body}"
    );
}

/// More worker reports in the last hour than the removed report rate allowed, for the
/// node (721 against 720) and the server (12,241 against 11,520): no hold, and the poll
/// offers work. The reports are a minute old (expired), so no live cap counts them.
#[tokio::test]
async fn a_node_and_the_server_past_the_old_report_rates_are_not_held() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    sealed::inventories(pool, id, 721, now - 60_000).await;
    assert_eq!(gate(&c, id, now).await, (false, false, vec![]), "node: 721");
    for _ in 0..16 {
        sealed::inventories(pool, Uuid::new_v4(), 720, now - 60_000).await;
    }
    assert_eq!(gate(&c, id, now).await, (false, false, vec![]), "server");
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered(&body), 1, "{body}");
}

/// A project at its artifact cap is dropped before the per-poll project limit (4)
/// applies, so it is never charged a turn and cannot crowd out a project with work.
#[tokio::test]
async fn four_held_projects_do_not_starve_the_fifth() {
    let mut c = native_offer::setup().await;
    let pool = c.env.env.pool.clone();
    let now = (c.env.env.state.clock)().timestamp_millis();
    let mut config = (*c.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    let template = profiled.projects[0].clone();
    let mut tx = sealed::replica(&pool).await;
    for _ in 0..4 {
        let mut held = template.clone();
        held.project_id = Uuid::new_v4();
        held.priority = 100;
        for _ in 0..449 {
            sealed::artifact(&mut tx, held.project_id, now - 1_000).await;
        }
        profiled.projects.push(held);
    }
    tx.commit().await.unwrap();
    config.profiled_execution = Some(std::sync::Arc::new(profiled));
    c.env.env.state.config = std::sync::Arc::new(config);
    c.env.env.app = aq_server::build_admin_router(c.env.env.state.clone());
    aq_server::profiled_execution::policies::install(&c.env.env.state)
        .await
        .unwrap();
    let (status, body) = native_offer::poll_offered(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        offered(&body),
        1,
        "the fifth project's task is offered: {body}"
    );
    let served: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_project_schedule")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        served, 1,
        "only the project that was tried is charged a turn"
    );
}
