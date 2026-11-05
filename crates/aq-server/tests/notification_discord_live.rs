//! Optional provider qualification only. Never run in unit/ordinary CI suites.
use serde::Deserialize;
use std::sync::Arc;
#[tokio::test]
#[ignore = "INT03: sends one real Discord message; requires separately authorized explicit opt-in"]
async fn explicitly_authorized_discord_transport_receipt() {
    assert_eq!(
        std::env::var("AQ_NOTIFICATION_REAL_SEND_CONFIRMED").as_deref(),
        Ok("1"),
        "explicit provider-send authorization is required"
    );
    let path = std::env::var("AQ_NOTIFICATION_REAL_WEBHOOK_FILE")
        .expect("protected webhook file path required");
    #[derive(Deserialize)]
    struct Config {
        #[serde(deserialize_with = "aq_server::notifications::config::deserialize")]
        destinations: Option<Arc<aq_server::notifications::config::Loaded>>,
    }
    let config:Config=serde_json::from_value(serde_json::json!({"destinations":{"revision":1,"endpoints":[{"key":"explicit-qualification","revision":1,"secret_file":path}]}})).expect("protected configuration refused");
    let loaded = config.destinations.unwrap();
    let endpoint = loaded.endpoints["explicit-qualification"]
        .webhook
        .as_ref()
        .expect("protected endpoint file missing");
    let outcome = aq_notifications::discord::Discord::new()
        .unwrap()
        .send(
            endpoint,
            &aq_notifications::Payload {
                content: "AQ attention: F4 automatic notification live qualification (explicit opt-in, one message).".into(),
            },
        )
        .await
        .unwrap();
    let aq_notifications::TransportOutcome::Accepted { message_id } = outcome.outcome else {
        panic!(
            "transport acceptance not established; do not automatically repeat an uncertain send"
        );
    };
    // Redacted receipt: provider message ID and acceptance time only, never the URL.
    println!(
        "receipt discord_message_id={message_id} accepted_at={}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
}
