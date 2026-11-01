use aq_phase_terminal_protocol::*;
fn main(){
 let expected:serde_json::Value=serde_json::from_str(include_str!("/tmp/aq-phase-terminal-extracted/aq-phase-terminal-protocol-0.1.0/artifacts/fixtures.json")).unwrap();
 let artifact:serde_json::Value=serde_json::from_str(include_str!("/tmp/aq-phase-terminal-extracted/aq-phase-terminal-protocol-0.1.0/artifacts/artifact.json")).unwrap();
 assert_eq!(serde_json::to_value(contract_artifact().unwrap()).unwrap(),artifact);
 let q:StartedRequest=serde_json::from_value(expected["started_request"].clone()).unwrap();
 let receipt:StartedReceipt=serde_json::from_value(expected["started_receipt"].clone()).unwrap();
 receipt.validate_for(&q).unwrap();
 assert_eq!(q.digest().unwrap(),receipt.request_digest);
 println!("PASS preserve_order: exact contract artifact and outer canonical request digest unchanged");
}
