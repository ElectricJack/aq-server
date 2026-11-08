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
async fn rows(pool: &sqlx::PgPool) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM intelligence_decisions),(SELECT count(*) FROM admission_artifacts)")
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
    assert_eq!(rows(pool).await, before, "a held poll writes no history");
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
