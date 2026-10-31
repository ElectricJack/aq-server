use aq_admission::{Digest, started, stopped, transport::StopDisposition};
fn main() {
 let pair:serde_json::Value=serde_json::from_str(include_str!("../host-proof-pair.json")).unwrap();
 let running:started::RunningObservation=serde_json::from_value(pair["running"].clone()).unwrap();
 let proof:stopped::HostStoppedProof=serde_json::from_value(pair["stopped_proof"].clone()).unwrap();
 let mut start:started::Request=serde_json::from_str(include_str!("../../aq-admission-0.1.4/artifacts/started.fixture.json")).unwrap();
 start.binding.session=running.guardian.intent.session.clone();
 if let aq_admission::SessionIdentity::Remote{attempt_id,assignment_generation,..}=start.binding.session {
  start.binding.attempt_id=attempt_id;start.binding.assignment_generation=assignment_generation;
 } else {panic!("remote host fixture required");}
 start.binding.launch_manifest_digest=running.guardian.intent.manifest_digest.clone();
 start.launch_id=running.guardian.intent.launch_id;
 start.observation=running;
 start.observation_digest=start.observation.digest().unwrap();start.validate().unwrap();
 let mut stop:stopped::Request=serde_json::from_str(include_str!("../../aq-admission-0.1.4/artifacts/stopped.fixture.json")).unwrap();
 stop.evidence.binding=start.binding.clone();stop.evidence.guardian_id=start.guardian_id;stop.evidence.launch_id=start.launch_id;
 stop.reservation_id=start.reservation_id;stop.runtime=start.runtime.clone();stop.qualification=start.qualification.clone();
 stop.proof=proof;
 stop.evidence.disposition=StopDisposition::Stopped{grant_id:start.grant_id,stop_receipt_digest:stop.proof.digest().unwrap()};
 stop.validate_after(&start).unwrap();
 let bytes=serde_json::to_vec(&stop).unwrap();stopped::decode_request(&bytes).unwrap();
 println!("actual host Running/StoppedProof compatibility PASS; pair_sha256={}; stopped_request_bytes={}",Digest::of(include_bytes!("../host-proof-pair.json")).as_str(),bytes.len());
}
