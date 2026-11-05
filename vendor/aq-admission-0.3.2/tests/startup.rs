mod startup_common;
use aq_admission::{startup::*, *};
use aq_session_protocol::{ContextDelivery, canonical_digest};
use startup_common::common::*;
use startup_common::fixture;
#[test]
fn byte_for_byte_native_payload_retains_constituents_without_duplicate_delivery() {
    let (native, bytes) = fixture();
    native.validate().unwrap();
    native.startup.verify_payload(&bytes).unwrap();
    let encoded = serde_json::to_vec(&native).unwrap();
    let replay = decode_native_offer(&encoded).unwrap();
    assert_eq!(serde_json::to_vec(&replay).unwrap(), encoded);
    let mut changed = bytes.clone();
    changed.extend_from_slice(b"\n");
    assert!(native.startup.verify_payload(&changed).is_err());
    changed = bytes;
    changed[0] = 0xff;
    assert!(native.startup.verify_payload(&changed).is_err());
}
#[test]
fn missing_reordered_duplicate_or_extra_startup_inputs_refuse() {
    let (native, _) = fixture();
    for inputs in [
        vec![],
        vec![native.startup.startup_inputs[0].clone()],
        vec![
            native.startup.startup_inputs[1].clone(),
            native.startup.startup_inputs[0].clone(),
        ],
        vec![
            native.startup.startup_inputs[0].clone(),
            native.startup.startup_inputs[0].clone(),
        ],
        vec![
            native.startup.startup_inputs[0].clone(),
            native.startup.startup_inputs[1].clone(),
            InputSpan {
                artifact: artifact("extra"),
                offset: 26,
                bytes: 5,
            },
        ],
    ] {
        let mut changed = native.clone();
        changed.startup.startup_inputs = inputs;
        assert!(changed.validate().is_err());
    }
    let mut changed = native;
    changed.offered.offer.manifest.context.references[1].delivery = ContextDelivery::OnDemand;
    changed.offered.offer.manifest.context.references[1].required = false;
    changed.offered.offer.binding.launch_manifest_digest =
        canonical_digest(&changed.offered.offer.manifest).unwrap();
    assert!(changed.validate().is_err());
}
#[test]
fn overhead_overflow_payload_alias_unretained_and_unknown_fields_refuse() {
    let (native, _) = fixture();
    let mut changed = native.clone();
    changed.startup.implicit_tokens_upper_bound = u64::MAX;
    assert!(changed.validate().is_err());
    changed = native.clone();
    changed.startup.payload_tokens_upper_bound = 4090;
    assert!(changed.validate().is_err());
    changed = native.clone();
    changed.startup.payload = changed.startup.startup_inputs[0].artifact.clone();
    assert!(changed.validate().is_err());
    changed = native.clone();
    changed.offered.artifacts.pop();
    assert!(changed.validate().is_err());
    let mut value = serde_json::to_value(&native).unwrap();
    value["startup"]["shell_hook"] = serde_json::json!("x");
    assert!(decode_native_offer(&serde_json::to_vec(&value).unwrap()).is_err());
    let raw = serde_json::to_string(&native).unwrap().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert!(decode_native_offer(raw.as_bytes()).is_err());
}

#[test]
fn span_witness_checks_actual_constituent_bytes_and_descriptor_is_launch_bound() {
    let (native, bytes) = fixture();
    let mut altered = native.clone();
    altered.startup.startup_inputs[1].offset = 13;
    assert!(altered.startup.verify_payload(&bytes).is_err());
    assert!(altered.validate().is_err()); // Original launch binding pins descriptor.
    altered = native.clone();
    altered.startup.startup_inputs[1].offset = u64::MAX;
    assert!(altered.startup.verify_payload(&bytes).is_err());
    altered = native;
    altered.startup.startup_inputs[1].artifact.digest = Digest::of(b"forged task");
    assert!(altered.startup.verify_payload(&bytes).is_err());
}
#[test]
fn independently_hashed_published_payload_descriptor_and_schema_do_not_drift() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/startup.fixture.json")).unwrap();
    let native =
        decode_native_offer(&serde_json::to_vec(&expected["native_offer"]).unwrap()).unwrap();
    let bytes = expected["payload_utf8"].as_str().unwrap().as_bytes();
    native.startup.verify_payload(bytes).unwrap();
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/startup.python-vectors.json")).unwrap();
    assert_eq!(
        native.startup.payload.digest.as_str(),
        vectors["payload_sha256"].as_str().unwrap()
    );
    assert_eq!(
        native.startup.artifact().unwrap().digest.as_str(),
        vectors["descriptor_sha256"].as_str().unwrap()
    );
    for (i, input) in native.startup.startup_inputs.iter().enumerate() {
        assert_eq!(
            input.artifact.digest.as_str(),
            vectors["input_sha256"][i].as_str().unwrap()
        );
    }
    assert_eq!(
        native_offer_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/startup.schema.json"))
            .unwrap()
    );
    let (generated, payload) = fixture();
    assert_eq!(
        serde_json::to_value(generated).unwrap(),
        expected["native_offer"]
    );
    assert_eq!(payload, bytes);
}

#[test]
fn one_launch_cannot_pin_two_alternative_native_startup_descriptors() {
    let (mut native, _) = fixture();
    let mut alternative = native.startup.clone();
    alternative.implicit_tokens_upper_bound += 1;
    let reference = alternative.artifact().unwrap();
    native
        .offered
        .offer
        .manifest
        .modules
        .push(reference.clone());
    native.offered.artifacts.push(RetainedArtifact {
        artifact: reference,
        kind: ArtifactKind::Definition,
        bytes: aq_session_protocol::canonical_bytes(&alternative)
            .unwrap()
            .len() as u64,
        retention_receipt: artifact("synthetic-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    });
    native.offered.offer.binding.launch_manifest_digest =
        canonical_digest(&native.offered.offer.manifest).unwrap();
    assert_eq!(
        native.validate().unwrap_err().0,
        "manifest pins exactly one startup descriptor"
    );
    native.startup = alternative;
    assert_eq!(
        native.validate().unwrap_err().0,
        "manifest pins exactly one startup descriptor"
    );
}
