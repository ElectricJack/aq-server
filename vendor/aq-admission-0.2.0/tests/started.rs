mod started_common;
use aq_admission::{Digest, started::*};
use serde_json::json;
#[test]
fn public_artifacts_and_independent_python_canonical_vectors_match() {
    let request = started_common::request();
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../artifacts/started.fixture.json"
        ))
        .unwrap()
    );
    assert_eq!(
        request_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/started.schema.json"))
            .unwrap()
    );
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/started.python-vectors.json")).unwrap();
    assert_eq!(
        request.digest().unwrap().as_str(),
        vectors["request"].as_str().unwrap()
    );
    assert_eq!(
        request.observation.digest().unwrap().as_str(),
        vectors["observation"].as_str().unwrap()
    );
    let mut unicode = request;
    unicode.runtime.id = aq_admission::Text::new("runtime/雪").unwrap();
    assert_eq!(
        unicode.digest().unwrap().as_str(),
        vectors["unicode_request"].as_str().unwrap()
    );
}
#[test]
fn started_identity_is_original_owner_bound_not_a_renewal_or_physical_stop() {
    let request = started_common::request();
    request.validate().unwrap();
    assert_ne!(request.guardian_id, request.observation.guardian.nonce);
    let bytes = serde_json::to_vec(&request).unwrap();
    assert_eq!(
        decode_request(&bytes).unwrap().digest().unwrap(),
        request.digest().unwrap()
    );
    for phase in [
        "waiting",
        "starting",
        "stopping",
        "stopped",
        "stop_uncertain",
    ] {
        let mut value = serde_json::to_value(&request).unwrap();
        value["observation"]["phase"] = json!(phase);
        assert!(decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
    }
    for mutate in [
        |r: &mut Request| r.observation.kernel_wait_echild = true,
        |r: &mut Request| r.observation.root_status = Some(0),
        |r: &mut Request| r.observation.permit_deadline_boot_ms = r.observation.observed_boot_ms,
        |r: &mut Request| r.observation.guardian.intent.manifest_digest = Digest::of(b"different"),
        |r: &mut Request| r.observation.guardian.intent.launch_id = uuid::Uuid::nil(),
        |r: &mut Request| r.observation_digest = Digest::of(b"forged"),
        |r: &mut Request| r.journal_revision = 0,
    ] {
        let mut altered = request.clone();
        mutate(&mut altered);
        assert!(altered.validate().is_err());
    }
}
#[test]
fn receipt_is_exact_historical_ack_and_strict_boundaries_refuse_unsupported_input() {
    let request = started_common::request();
    let receipt = Receipt {
        schema_version: 1,
        operation_id: request.operation_id,
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        request_digest: request.digest().unwrap(),
        recorded_at_ms: 2_000,
        authority_revision_at_record: 4,
    };
    let bytes = serde_json::to_vec(&receipt).unwrap();
    decode_receipt(&bytes, &request).unwrap();
    let mut altered = request.clone();
    altered.journal_revision += 1;
    assert!(decode_receipt(&bytes, &altered).is_err());
    let mut value = serde_json::to_value(&request).unwrap();
    value["extend_grant"] = json!(true);
    assert!(decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(decode_request(&vec![b' '; MAX_BYTES + 1]).is_err());
    let mut value = serde_json::to_value(&request).unwrap();
    value["observation"]["pane"] = json!(null);
    assert!(decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
}
