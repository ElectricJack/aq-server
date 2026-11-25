//! Opt-in local benchmark. Supply the diagnostics switch outside the test
//! process so the production startup path controls capture in both arms.
use crate::common::native_offer::setup;
use aq_admission::{
    control::{ControlReply, NativePollPage, decode_reply},
    transport::Poll,
};
use serde_json::json;
use sqlx::PgPool;
use std::time::{Duration, Instant};
use uuid::Uuid;

const POLLS: usize = 250;
const CADENCE: Duration = Duration::from_millis(30);

async fn poll(
    http: &reqwest::Client,
    endpoint: &str,
    credential: &str,
    request: &Poll,
) -> NativePollPage {
    let response = http
        .post(endpoint)
        .bearer_auth(credential)
        .json(request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let ControlReply::Poll(page) = decode_reply(&response.bytes().await.unwrap()).unwrap() else {
        panic!("native poll response required");
    };
    page.validate_for(request).unwrap();
    page
}

async fn writes(pool: &PgPool) -> (i64, i64) {
    sqlx::query_as(
        "SELECT COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del),0)::bigint,
                COALESCE(sum(n_tup_ins+n_tup_upd+n_tup_del)
                    FILTER (WHERE relname LIKE 'performance_scheduling_%'),0)::bigint
         FROM pg_stat_user_tables",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "opt-in timed benchmark requiring disposable PostgreSQL"]
async fn poll_diagnostics_overhead() {
    let case = setup().await;
    let pool = &case.env.env.pool;
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker_origin = format!("http://{}", worker.local_addr().unwrap());
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let daemon = tokio::spawn(aq_server::run_with_listeners(
        case.env.env.state.clone(),
        admin,
        worker,
        async move { stopped.await.unwrap() },
    ));
    let endpoint = format!("{worker_origin}/v1/worker/profiled/poll");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    http.get(format!("{worker_origin}/healthz"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let page = poll(&http, &endpoint, &case.env.env.credential, &case.request).await;
            if !page.offers.is_empty() {
                break page;
            }
            tokio::time::sleep(CADENCE).await;
        }
    })
    .await
    .expect("one new offer on the same authenticated fixture in both arms");
    assert_eq!(first.offers.len(), 1);
    let expected_offer = first.offers[0].offered.offer.binding.clone();
    tokio::time::sleep(Duration::from_millis(6_100)).await;
    let (before_total, before_diagnostics) = writes(pool).await;
    let mut latencies = Vec::with_capacity(POLLS);
    let mut empty = 0;
    let mut redeliveries = 0;
    let started = Instant::now();
    let mut pace = tokio::time::interval(CADENCE);
    for i in 0..POLLS {
        pace.tick().await;
        let mut request = case.request.clone();
        request.operation_id = Uuid::new_v4();
        if i % 5 != 0 {
            request.available.slots = 0;
        }
        let start = Instant::now();
        let page = poll(&http, &endpoint, &case.env.env.credential, &request).await;
        latencies.push(start.elapsed().as_micros());
        if request.available.slots == 0 {
            assert!(page.offers.is_empty());
            empty += 1;
        } else {
            assert_eq!(page.offers.len(), 1);
            assert_eq!(page.offers[0].offered.offer.binding, expected_offer);
            redeliveries += 1;
        }
        assert!(page.cancellations.is_empty());
    }
    let elapsed = started.elapsed();
    latencies.sort_unstable();
    // Allow the five-second capture flush and PostgreSQL statistics publication
    // to finish after the last request. The wait is outside measured latency.
    tokio::time::sleep(Duration::from_millis(6_100)).await;
    let (after_total, after_diagnostics) = writes(pool).await;
    let captures: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(count),0)::bigint FROM performance_scheduling_minute
         WHERE reason='zero_available_slots'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let drops: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(count),0)::bigint FROM performance_scheduling_minute
         WHERE reason='dropped_observation'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let claims: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_attempts WHERE holds_task")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(claims, 1);
    let enabled = std::env::var("AQ_PERFORMANCE_SCHEDULING_DIAGNOSTICS").as_deref() != Ok("0");
    assert_eq!(captures + drops, if enabled { empty } else { 0 });
    println!(
        "AQ_PERF_POLL {}",
        json!({
            "enabled": enabled, "polls": POLLS, "elapsed_s": elapsed.as_secs_f64(),
            "throughput": POLLS as f64 / elapsed.as_secs_f64(),
            "p50_us": latencies[POLLS.div_ceil(2) - 1], "p95_us": latencies[(POLLS * 95).div_ceil(100) - 1],
            "empty": empty, "redeliveries": redeliveries, "claims": claims,
        "captures": captures, "drops": drops,
            "total_tuple_writes": after_total - before_total,
            "diagnostic_tuple_writes": after_diagnostics - before_diagnostics,
        })
    );
    shutdown.send(()).unwrap();
    daemon.await.unwrap().unwrap();
}
