//! Actual local physical lifecycle after actual remote phase issuance. No remote
//! Started/Stopped acknowledgment or task settlement is claimed by this probe.
use super::*;
fn pin(binary: &str, hash: &str, id: &str) -> Value {
    let path =
        std::fs::canonicalize(std::env::var_os(binary).expect("explicit host binary")).unwrap();
    let expected: Digest =
        serde_json::from_value(json!(std::env::var(hash).expect("explicit host digest"))).unwrap();
    assert_eq!(Digest::of(&std::fs::read(&path).unwrap()), expected);
    json!({"path":path,"artifact":{"id":id,"version":"1","digest":expected}})
}
pub(super) fn configuration() -> Value {
    json!({
        "helper":pin("AQ_NATIVE_HOST_BINARY","AQ_NATIVE_HOST_SHA256","aq.synthetic-host-helper"),
        "tmux":pin("AQ_NATIVE_TMUX_BINARY","AQ_NATIVE_TMUX_SHA256","aq.synthetic-system-tmux34"),
        "program":pin("AQ_NATIVE_SYNTHETIC_ELF","AQ_NATIVE_SYNTHETIC_SHA256","aq.synthetic-native-harness"),
        "renderer":{"id":"aq.synthetic-baseline-ascii","version":"1",
            "digest":Digest::of(b"synthetic baseline only; no Snapshot qualification")}
    })
}
pub(super) fn verify(root: &std::path::Path, result: &Value) {
    let started: aq_admission::started::Request =
        serde_json::from_value(result["host_started"].clone()).unwrap();
    started.validate().unwrap();
    let delivery: aq_admission::preparation_v2::ExecutionDelivery =
        serde_json::from_value(result["execution_delivery"].clone()).unwrap();
    assert_eq!(started.binding, delivery.receipt.grant.binding);
    assert_eq!(started.grant_id, delivery.receipt.grant.grant_id);
    assert_ne!(started.guardian_id, started.observation.guardian.nonce);
    let snapshot: Value =
        serde_json::from_slice(&std::fs::read(root.join("journal/journal.json")).unwrap()).unwrap();
    let history =
        &snapshot["state"]["attempts"][started.binding.attempt_id.to_string()]["native_host"];
    assert_eq!(history["running"]["request"], result["host_started"]);
    assert_eq!(history["stopped"], result["host_stopped"]);
    assert_eq!(
        history["stopped"]["recorded_revision"],
        result["host_revision"]
    );
    assert!(history["baseline"].is_object());
    assert!(history["started_receipt"].is_null());
    assert!(history["uncertain"].is_null());
    assert_eq!(
        history["stopped"]["observation"]["kernel_wait_echild"],
        true
    );
    assert!(history["stopped"]["observation"]["root_status"].is_i64());
    assert_eq!(
        history["stopped"]["observation"]["guardian"],
        history["running"]["observation"]["guardian"]
    );
    assert!(
        history["stopped"]["recorded_revision"].as_u64().unwrap()
            > history["running"]["recorded_revision"].as_u64().unwrap()
    );
    // Format10 pairs the actual physical facts with fsynced phase envelopes.
    // This host-only probe sends neither envelope and claims no remote ACK.
    let terminal = &history["terminal"];
    let phase_started = &terminal["started"]["request"];
    let phase_stopped = &terminal["stopped"]["request"];
    assert_eq!(phase_started["schema_version"], 1);
    assert_eq!(phase_stopped["schema_version"], 1);
    assert_eq!(phase_started["original"], result["host_started"]);
    let stopped: aq_admission::stopped::Request =
        serde_json::from_value(phase_stopped["original"].clone()).unwrap();
    stopped.validate_after(&started).unwrap();
    assert_eq!(
        serde_json::to_value(&stopped.proof.observation).unwrap(),
        history["stopped"]["observation"]
    );
    assert_eq!(phase_started["origin"], phase_stopped["origin"]);
    assert_eq!(phase_started["origin"]["kind"], "phase_execution_v2");
    assert_eq!(
        phase_started["origin"]["execution_receipt_digest"],
        json!(delivery.receipt.digest().unwrap())
    );
    let attempt = &snapshot["state"]["attempts"][started.binding.attempt_id.to_string()];
    let accepted: aq_admission::preparation_v2::AcceptedReceipt =
        serde_json::from_value(attempt["phase_preparation"]["delivery"]["receipt"].clone())
            .unwrap();
    assert_eq!(
        phase_started["origin"]["accepted_receipt_digest"],
        json!(accepted.digest().unwrap())
    );
    assert_eq!(phase_started["negotiation"], phase_stopped["negotiation"]);
    assert_eq!(
        phase_started["negotiation"]["protocol"],
        "aq.phase-terminal.v1"
    );
    assert_eq!(
        phase_started["negotiation"]["features"],
        json!(["started_stopped_closure_v1", "original_lease_no_renewal_v1"])
    );
    let contract: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../aq-phase-terminal-protocol/artifacts/artifact.json"
    )))
    .unwrap();
    assert_eq!(phase_started["negotiation"]["contract"], contract);
    assert_eq!(
        terminal["started"]["recorded_revision"],
        history["running"]["recorded_revision"]
    );
    assert_eq!(
        terminal["stopped"]["recorded_revision"],
        history["stopped"]["recorded_revision"]
    );
    assert_eq!(
        phase_started["original"]["operation_id"],
        history["intent"]["started_operation_id"]
    );
    assert_eq!(
        phase_stopped["original"]["evidence"]["stop_operation_id"],
        history["intent"]["stopped_operation_id"]
    );
    if result.get("phase_stopped_ack").is_some() {
        assert_eq!(terminal["started"]["receipt"], result["phase_started_ack"]);
        assert_eq!(terminal["stopped"]["receipt"], result["phase_stopped_ack"]);
    } else {
        assert!(terminal["started"]["receipt"].is_null());
        assert!(terminal["stopped"]["receipt"].is_null());
    }
    let directory = root.join("host").join(started.launch_id.to_string());
    assert_eq!(
        std::fs::read(directory.join("fixture-marker")).unwrap(),
        std::fs::read(directory.join("context-0.bin")).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(directory.join("fixture-marker.cwd")).unwrap(),
        result["workspace_path"].as_str().unwrap()
    );
    eprintln!(
        "actual host: Running then durable ECHILD; execution_revision={}, stopped_revision={}",
        result["execution_revision"], result["host_revision"]
    );
}
