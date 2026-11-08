//! Spec 0008 phase A: the offer rate holds a node with an empty poll, never a 409,
//! and writes no history; offers older than an hour do not count.
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
async fn rows(pool: &sqlx::PgPool) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM intelligence_decisions),(SELECT count(*) FROM admission_artifacts),(SELECT count(*) FROM profiled_source_exports),(SELECT count(*) FROM profiled_project_schedule)")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn offer_rate_limit_holds_the_node_without_a_409() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    sealed::offers(pool, id, c.env.lineage.project_id, now - 1_000, 30).await;
    let before = rows(pool).await;
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "never a 409: {body}");
    assert_eq!(offered(&body), 0, "{body}");
    assert_eq!(
        rows(pool).await,
        before,
        "a held poll writes no decision, artifact, export or schedule row"
    );
    let mut conn = pool.acquire().await.unwrap();
    let holds = aq_history_caps::node_holds(&mut conn, id, now as u64, None)
        .await
        .unwrap();
    assert_eq!(
        holds.iter().map(|h| h.message()).collect::<Vec<_>>(),
        vec!["Offer rate limit reached: 30 offers in the last hour (limit 30)".to_string()]
    );
}

#[tokio::test]
async fn offers_older_than_an_hour_do_not_count() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    let now = (c.env.env.state.clock)().timestamp_millis();
    let id = node(&c).await;
    sealed::offers(pool, id, c.env.lineage.project_id, now - 3_700_000, 30).await;
    let (status, body) = native_offer::poll(&c).await;
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
    let (status, body) = native_offer::poll(&c).await;
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

/// The offer-rate fence lost a race with another poll that passed the gate: the
/// poll offers nothing (200), never a 500, and leaves no attempt behind.
#[tokio::test]
async fn the_offer_rate_fence_racing_past_the_gate_is_an_empty_page() {
    let c = native_offer::setup().await;
    let pool = &c.env.env.pool;
    sqlx::raw_sql("CREATE FUNCTION fence_race() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'sealed history offer rate' USING ERRCODE='54000'; END $$; CREATE TRIGGER fence_race BEFORE INSERT ON profiled_launches FOR EACH ROW EXECUTE FUNCTION fence_race();")
        .execute(pool)
        .await
        .unwrap();
    let (status, body) = native_offer::poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(offered(&body), 0, "{body}");
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempts")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}
