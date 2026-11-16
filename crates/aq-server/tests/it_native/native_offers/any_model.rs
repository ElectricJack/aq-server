//! Epic `any-model`: an offer needs no measured context fact, a node gets no
//! qualification pin and no measurement is recorded. The actual mounted poll,
//! selection and publication run; the produced offer is checked by the worker's
//! own validators (`aq-admission`, `aq-session-protocol`).
use crate::common::native_offer::{Case, poll_offered, post, setup};
use aq_admission::{
    control::{ControlReply, decode_reply},
    startup::NativeOffer,
};
use aq_server::profiled_execution::context_budget::Qualification;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;

/// Replaces the configured context facts.
async fn facts(c: &mut Case, edit: impl FnOnce(&mut Vec<Qualification>)) {
    let mut config = (*c.env.env.state.config).clone();
    let mut native = (**config.profiled_execution.as_ref().unwrap()).clone();
    native.revision += 1;
    edit(&mut native.context_qualifications);
    config.profiled_execution = Some(Arc::new(native));
    c.env.env.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&c.env.env.state)
        .await
        .unwrap();
}
/// The one offer of a successful poll, checked as the worker checks it. Its
/// token numbers are the rendered byte counts, so the budget inequalities hold
/// by construction: no model window is compared.
async fn offered(c: &Case) -> NativeOffer {
    let (status, body) = poll_offered(c).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&c.request).unwrap();
    assert_eq!(page.offers.len(), 1, "{body}");
    let native = page.offers.remove(0);
    native.validate().unwrap();
    let manifest = &native.offered.offer.manifest;
    aq_session_protocol::Validate::validate(manifest).unwrap();
    let (startup, plan) = (&native.startup, &manifest.context);
    assert_eq!(startup.payload_tokens_upper_bound, startup.payload_bytes);
    assert_eq!(startup.implicit_tokens_upper_bound, 0);
    assert_eq!(plan.startup_byte_budget, startup.payload_bytes);
    assert_eq!(plan.startup_token_budget, startup.payload_bytes);
    assert_eq!(
        plan.reserved_output_tokens,
        manifest.model.max_output_tokens
    );
    assert_eq!(
        plan.context_window_tokens,
        plan.startup_token_budget + plan.reserved_output_tokens
    );
    let tokens: u64 = plan.references.iter().map(|r| r.tokens).sum();
    assert_eq!(tokens, plan.startup_token_budget);
    assert!(
        (startup.renderer_qualification.id.as_str()).starts_with("aq.startup-renderer/"),
        "{:?}",
        startup.renderer_qualification
    );
    native
}
async fn send(c: &Case, path: &str, body: &Value) -> (u16, Value) {
    let bytes = serde_json::to_vec(body).unwrap();
    let r = post(&c.env.env.state, &c.env.env.credential, path, bytes).await;
    let status = r.status().as_u16();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn offer_needs_no_configured_context_fact() {
    let mut c = setup().await;
    facts(&mut c, |f| f.clear()).await;
    let with_none = offered(&c).await.startup.renderer_qualification;
    drop(c);
    // The same rule, whatever is or is not configured.
    let c = setup().await;
    assert_eq!(offered(&c).await.startup.renderer_qualification, with_none);
}

#[tokio::test]
async fn offer_is_made_with_an_expired_context_fact() {
    let mut c = setup().await;
    let now = (c.env.env.state.clock)().timestamp_millis() as u64;
    facts(&mut c, |f| {
        (f[0].observed_at_ms, f[0].expires_at_ms) = (now - 120_000, now - 1);
    })
    .await;
    offered(&c).await;
}

#[tokio::test]
async fn control_pins_no_qualification_and_a_measurement_is_refused_and_not_stored() {
    let c = setup().await;
    let mut conn = c.env.env.pool.acquire().await.unwrap();
    aq_machine_control::claim_key(&mut conn, c.env.env.node, "desk")
        .await
        .unwrap();
    let (status, body) = send(&c, "/v1/worker/control", &json!({"schema_version":1})).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.get("qualification").is_none(), "{body}");
    // An old worker still measuring gets the answer it gets today when no fact
    // can be renewed (409), and nothing is stored.
    let calls: Vec<Value> = [("minimal", 67), ("mixed", 2738), ("adversarial", 5844)]
        .iter()
        .map(|(call, bytes)| {
            json!({"call":call,"prompt_utf8_bytes":bytes,"prompt_sha256":format!("{:0>64}", call.len()),
                "result":{"type":"result","subtype":"success","is_error":false,
                    "usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":20,"output_tokens":4},
                    "modelUsage":{"model-a":{"contextWindow":65536}}}})
        })
        .collect();
    let submission = json!({"schema_version":1,"calls":calls});
    let (status, body) = send(&c, "/v1/worker/qualifications", &submission).await;
    assert_eq!(status, 409, "{body}");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM context_qualification_renewals")
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);
    let (_, body) = send(&c, "/v1/worker/control", &json!({"schema_version":1})).await;
    assert!(body.get("qualification").is_none(), "{body}");
}
