#[cfg(feature = "test-support")]
fn main() {
    use aq_worker_protocol::{testing::*, *};
    let offer = sample_offer();
    let accept = sample_acceptance(&offer);
    let report = sample_report(&offer);
    let mut peer = FakeCoordinator::new(offer.clone(), 100);
    let grant = peer.accept(&accept).unwrap();
    let fixture = serde_json::json!({
        "fixture_format":1,"package":"aq-worker-protocol","version":env!("CARGO_PKG_VERSION"),"execution_protocol":2,"foundation_connectivity":[1,1],
        "valid": [WireMessage::PollResponse(PollResponse { server_time_ms:100,offers:vec![offer.clone()],directives:vec![] }),WireMessage::AcceptRequest(accept),WireMessage::AcceptResponse(grant),WireMessage::ResultReport(report.clone()),WireMessage::ResultResponse(peer.report(&report).unwrap())],
        "invalid": [{"message":"ack","body":{"operation_id":"00000000-0000-0000-0000-000000000000"}}],
        "behavioral_tests":["lost_acceptance_replay_reduces_ttl_and_changed_bytes_conflict","expiry_stale_incarnation_and_generation_are_refused","observation_sequences_and_report_replays_are_immutable","cancellation_race_and_late_generation_retain_evidence","spec_tampering_and_response_overflow_are_invalid"]
    });
    println!("{}", serde_json::to_string_pretty(&fixture).unwrap());
}
#[cfg(not(feature = "test-support"))]
fn main() {
    panic!("requires --features test-support");
}
