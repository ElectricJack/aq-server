//! Spec 0007 §7.3: node-submitted context qualification renewals extend offer
//! budgeting past the configured fact's 24 h window.
use crate::common;
use aq_admission::{
    ArtifactKind, ArtifactRef,
    control::{ControlReply, decode_reply},
};
use aq_server::profiled_execution::{
    context_budget::Qualification, context_renewal::EFFECTIVE_DELAY_MS, definitions::Definition,
};
use common::native_offer::{Case, poll, post, setup};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const CALLS: [(&str, u64); 3] = [("minimal", 67), ("mixed", 2738), ("adversarial", 5844)];

fn sha(call: &str) -> String {
    format!("{:0>64}", call.len())
}
fn now_ms(c: &Case) -> u64 {
    (c.env.env.state.clock)().timestamp_millis() as u64
}
/// The configured fact gets a measured (JSON) counting body and the given window.
async fn renewable(c: &mut Case, observed_at_ms: u64, expires_at_ms: u64) -> Qualification {
    let body = json!({"schema_version":1,"maximum_tokens_per_utf8_byte":1,"fixed_overhead_tokens":37,
        "measurements":CALLS.iter().map(|(c, b)| json!({"call":c,"prompt_utf8_bytes":b,"prompt_sha256":sha(c)})).collect::<Vec<_>>()})
        .to_string();
    let counting = common::profiled::artifact("native-test-measured-counting", body.as_bytes());
    let mut config = (*c.env.env.state.config).clone();
    let mut native = (*config.profiled_execution.as_ref().unwrap().as_ref()).clone();
    native.revision += 1;
    let fact = &mut native.context_qualifications[0];
    fact.counting_evidence = counting.clone();
    fact.observed_at_ms = observed_at_ms;
    fact.expires_at_ms = expires_at_ms;
    let fact = fact.clone();
    native
        .definition_bodies
        .push(Definition::new(counting, ArtifactKind::Definition, body).unwrap());
    config.profiled_execution = Some(Arc::new(native));
    c.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&c.env.env.state)
        .await
        .unwrap();
    fact
}
fn submission(adversarial_total: u64) -> Value {
    let calls: Vec<Value> = CALLS.iter().map(|(call, bytes)| {
        let read = if *call == "adversarial" { adversarial_total - 12 } else { 20 };
        json!({"call":call,"prompt_utf8_bytes":bytes,"prompt_sha256":sha(call),
            "result":{"type":"result","subtype":"success","is_error":false,
                "usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":read,"output_tokens":4},
                "modelUsage":{"model-a":{"contextWindow":65536}}}})
    }).collect();
    json!({"schema_version":1,"calls":calls})
}
async fn send(c: &Case, path: &str, body: &Value) -> (u16, Value) {
    let r = post(
        &c.env.env.state,
        &c.env.env.credential,
        path,
        serde_json::to_vec(body).unwrap(),
    )
    .await;
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
/// Records the measurement as if submitted `ago_ms` before now.
async fn record_ago(c: &mut Case, ago_ms: i64, body: &Value) -> (u16, Value) {
    let real = c.env.env.state.clock.clone();
    let shifted = real.clone();
    c.env.env.state.clock = Arc::new(move || shifted() - chrono::Duration::milliseconds(ago_ms));
    let out = send(c, "/v1/worker/qualifications", body).await;
    c.env.env.state.clock = real;
    out
}
fn renderer(body: &Value) -> ArtifactRef {
    let ControlReply::Poll(page) = decode_reply(&serde_json::to_vec(body).unwrap()).unwrap() else {
        panic!("{body}")
    };
    assert_eq!(page.offers.len(), 1, "{body}");
    page.offers[0].startup.renderer_qualification.clone()
}
fn digest(pin: &Value) -> String {
    pin["renderer"]["digest"].as_str().unwrap().to_owned()
}
async fn renewals(c: &Case) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM context_qualification_renewals")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn renewal_extends_offers_past_config_expiry() {
    let mut c = setup().await;
    let now = now_ms(&c);
    // The configured fact expired at T = now - 1; the renewal is recorded at T - 60 s.
    let config = renewable(&mut c, now - 120_000, now - 1).await;
    let (status, body) = poll(&c).await;
    assert_eq!(
        status, 409,
        "without the renewal the poll is refused: {body}"
    );
    let (status, pin) = record_ago(&mut c, 61_000, &submission(500)).await;
    assert_eq!(status, 200, "{pin}");
    assert!(pin["effective_at_ms"].as_u64().unwrap() < now);
    assert_eq!(
        pin["expires_at_ms"].as_u64().unwrap() - pin["effective_at_ms"].as_u64().unwrap(),
        86_400_000
    );
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    let used = renderer(&body);
    assert_eq!(used.digest.as_str(), digest(&pin));
    assert_ne!(used, config.artifact().unwrap());
}

#[tokio::test]
async fn renewal_not_effective_before_delay() {
    let mut c = setup().await;
    let now = now_ms(&c);
    let config = renewable(&mut c, now - 1, now + 600_000).await;
    let (status, pin) = send(&c, "/v1/worker/qualifications", &submission(500)).await;
    assert_eq!(status, 200, "{pin}");
    let effective = pin["effective_at_ms"].as_u64().unwrap();
    assert!(effective >= now + EFFECTIVE_DELAY_MS, "{pin}");
    let (status, body) = poll(&c).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(renderer(&body), config.artifact().unwrap());
}

/// The server enforces the counting bound (prompt bytes × 1 + 37 here, as
/// `ms1-profiled.py` `measured()` does with its constants); nothing is stored.
#[tokio::test]
async fn over_bound_measurement_is_refused_and_stores_nothing() {
    let mut c = setup().await;
    let now = now_ms(&c);
    renewable(&mut c, now - 1, now + 600_000).await;
    let (status, body) = send(&c, "/v1/worker/qualifications", &submission(5844 + 37 + 1)).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("bound adversarial"), "{body}");
    assert_eq!(renewals(&c).await, 0);
    let (status, body) = send(&c, "/v1/worker/qualifications", &submission(5844 + 37)).await;
    assert_eq!(status, 200, "exactly the bound is accepted: {body}");
    assert_eq!(renewals(&c).await, 1);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn expired_fact_logs_warn_reason() {
    let mut c = setup().await;
    let now = now_ms(&c);
    renewable(&mut c, now - 120_000, now - 1).await;
    let logs = Captured::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let (status, body) = {
        let _guard = tracing::subscriber::set_default(subscriber);
        poll(&c).await
    };
    assert_eq!(status, 409, "{body}");
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("WARN")
            && text.contains("qualified context facts are unavailable, expired or ambiguous"),
        "{text}"
    );
}

#[tokio::test]
async fn node_outside_credential_cannot_renew() {
    let mut c = setup().await;
    let now = now_ms(&c);
    renewable(&mut c, now - 1, now + 600_000).await;
    let mut config = (*c.env.env.state.config).clone();
    let mut intelligence = config.intelligence.as_ref().unwrap().config.clone();
    for credential in &mut intelligence.credentials {
        credential.allowed_node_ids.clear();
        credential.allow_enrolled_nodes = false;
    }
    config.intelligence = Some(Arc::new(intelligence.load().unwrap()));
    c.env.env.state.config = Arc::new(config);
    let (status, body) = send(&c, "/v1/worker/qualifications", &submission(500)).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(renewals(&c).await, 0);
}

#[tokio::test]
async fn control_reports_qualification_pin() {
    let mut c = setup().await;
    let now = now_ms(&c);
    let config = renewable(&mut c, now - 1, now + 600_000).await;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    aq_machine_control::claim_key(&mut conn, c.env.env.node, "desk")
        .await
        .unwrap();
    let control = json!({"schema_version":1});
    let (status, body) = send(&c, "/v1/worker/control", &control).await;
    assert_eq!(status, 200, "{body}");
    let configured = config.artifact().unwrap();
    assert_eq!(
        body["qualification"]["renderer"]["digest"],
        configured.digest.as_str()
    );
    assert_eq!(body["qualification"]["effective_at_ms"], now - 1);
    let (status, pin) = send(&c, "/v1/worker/qualifications", &submission(500)).await;
    assert_eq!(status, 200, "{pin}");
    // The pending renewal is announced so the service can prepare the switch.
    let (_, body) = send(&c, "/v1/worker/control", &control).await;
    assert_eq!(body["qualification"], pin, "{body}");
}

/// Review I7: a renewal is refused (409, typed) while one of the same fact is
/// pending, and a node renews at most once per 10 minutes.
#[tokio::test]
async fn renewals_wait_for_the_pending_one_and_ten_minutes_per_node() {
    let mut c = setup().await;
    let now = now_ms(&c);
    renewable(&mut c, now - 1, now + 3_600_000).await;
    let body = submission(500);
    let at = async |c: &mut Case, ms: i64| record_ago(c, -ms, &body).await;
    let (status, pin) = at(&mut c, -120_000).await;
    assert_eq!(status, 200, "{pin}");
    let (status, v) = at(&mut c, 0).await;
    assert_eq!(status, 409, "{v}");
    assert!(v.to_string().contains("renewal_too_soon"), "{v}");
    assert_eq!(renewals(&c).await, 1);
    let (status, pin) = at(&mut c, 660_000).await;
    assert_eq!(status, 200, "{pin}");
    let (status, v) = at(&mut c, 690_000).await;
    assert_eq!(status, 409, "{v}");
    assert!(v.to_string().contains("renewal_pending"), "{v}");
    assert_eq!(renewals(&c).await, 2);
    let (status, pin) = at(&mut c, 1_320_000).await;
    assert_eq!(status, 200, "{pin}");
    assert_eq!(renewals(&c).await, 3);
}
