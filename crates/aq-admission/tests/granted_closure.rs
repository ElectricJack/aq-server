mod granted_closure_common;
use aq_admission::{granted_closure::*, *};
use granted_closure_common::{context, requests};
use serde_json::{Value, json};
#[test]
fn synthetic_fixtures_schemas_and_independent_vectors_do_not_drift() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../artifacts/granted-closure.fixture.json")).unwrap();
    assert_eq!(fixtures, granted_closure_common::fixtures());
    let schema: Value =
        serde_json::from_str(include_str!("../artifacts/granted-closure.schema.json")).unwrap();
    assert_eq!(
        schema,
        json!({"request":request_schema(),"receipt":receipt_schema()})
    );
    let vectors: Value = serde_json::from_str(include_str!(
        "../artifacts/granted-closure.python-vectors.json"
    ))
    .unwrap();
    for (i, r) in requests().iter().enumerate() {
        let decoded = decode_request(&serde_json::to_vec(r).unwrap()).unwrap();
        decoded.validate_against(&context()).unwrap();
        assert_eq!(r.digest().unwrap().as_str(), vectors["requests"][i]);
        match &r.evidence {
            Evidence::Suppressed { record, .. } => {
                assert_eq!(record.digest().unwrap().as_str(), vectors["suppression"])
            }
            Evidence::HostStopped { proof, .. } => assert_eq!(
                proof.digest().unwrap().as_str(),
                vectors["host_proofs"][i - 1]
            ),
        }
        let receipt: Receipt =
            serde_json::from_value(fixtures["valid_receipts"][i].clone()).unwrap();
        decode_receipt(&serde_json::to_vec(&receipt).unwrap(), r).unwrap();
        assert!(receipt.recorded_at_ms > context().grant.grant.expires_at_ms);
    }
    let mut unicode = requests().remove(0);
    unicode.runtime.id = Text::new("runtime/雪").unwrap();
    assert_eq!(
        unicode.digest().unwrap().as_str(),
        vectors["unicode_request"]
    );
    for invalid in fixtures["invalid_shape"].as_array().unwrap() {
        assert!(
            decode_request(&serde_json::to_vec(&invalid["body"]).unwrap()).is_err(),
            "{}",
            invalid["case"]
        );
    }
}
#[test]
fn serialization_is_not_original_owner_or_current_authority() {
    let c = context();
    let mut r = requests().remove(0);
    let SessionIdentity::Remote { boot_id, .. } = &mut r.binding.session else {
        panic!()
    };
    *boot_id = uuid::Uuid::from_u128(9999);
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut r.evidence
    {
        record.binding = r.binding.clone();
        *record_digest = record.digest().unwrap();
    }
    r.validate().unwrap();
    assert!(r.validate_against(&c).is_err());
    let mut r = requests().remove(0);
    if let Evidence::Suppressed {
        record,
        record_digest,
    } = &mut r.evidence
    {
        record.previous.digest = Digest::of(b"wrong Prepared history");
        *record_digest = record.digest().unwrap();
    }
    r.validate().unwrap();
    assert!(r.validate_against(&c).is_err());
    let mut r = requests().remove(1);
    if let Evidence::HostStopped { driver_intent, .. } = &mut r.evidence {
        driver_intent.journal.revision = c.prepared.revision;
    }
    r.validate().unwrap();
    assert!(r.validate_against(&c).is_err());
}
#[test]
fn actual_host_optional_shape_does_not_weaken_the_post_started_contract() {
    let all = requests();
    let Evidence::HostStopped { proof, .. } = &all[3].evidence else {
        panic!()
    };
    assert!(proof.observation.pane.is_none());
    assert!(proof.observation.root_status.is_none());
    assert_eq!(proof.observation.reaped_count, 0);
    let raw = serde_json::to_vec(proof).unwrap();
    assert!(serde_json::from_slice::<stopped::HostStoppedProof>(&raw).is_err());
    assert!(
        serde_json::from_slice::<HostStoppedProof>(&raw)
            .unwrap()
            .digest()
            .is_ok()
    );
    let Evidence::HostStopped { proof, .. } = &all[2].evidence else {
        panic!()
    };
    assert!(proof.observation.pane.is_none());
    assert!(proof.observation.root_status.is_some());
    assert!(proof.observation.reaped_count > 0);
}
#[test]
fn strict_decode_rejects_duplicates_floats_oversize_and_release_semantics() {
    let r = requests().remove(0);
    let body = serde_json::to_string(&r).unwrap();
    let duplicate = format!("{{\"operation_id\":\"{}\",{}", r.operation_id, &body[1..]);
    assert!(decode_request(duplicate.as_bytes()).is_err());
    let mut value = serde_json::to_value(&r).unwrap();
    value["journal"]["revision"] = json!(1.5);
    assert!(decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(decode_request(&vec![b' '; MAX_BYTES + 1]).is_err());
    let mut host = serde_json::to_value(&requests()[3]).unwrap();
    host["evidence"]["proof"]["observation"]
        .as_object_mut()
        .unwrap()
        .remove("pane");
    assert!(decode_request(&serde_json::to_vec(&host).unwrap()).is_err());

    let fixture = granted_closure_common::fixtures();
    let mut receipt = fixture["valid_receipts"][0].clone();
    receipt["released"] = json!(true);
    assert!(decode_receipt(&serde_json::to_vec(&receipt).unwrap(), &r).is_err());
    receipt = fixture["valid_receipts"][0].clone();
    receipt["disposition"] = json!("never_granted");
    assert!(decode_receipt(&serde_json::to_vec(&receipt).unwrap(), &r).is_err());
    let mut changed = r.clone();
    changed.journal.digest = Digest::of(b"changed same operation");
    changed.validate_against(&context()).unwrap();
    assert_ne!(changed.digest().unwrap(), r.digest().unwrap());
}
#[test]
fn frozen_017_source_and_artifacts_are_preserved() {
    let v: Value = serde_json::from_str(include_str!(
        "../artifacts/granted-closure.compatibility-sha256.json"
    ))
    .unwrap();
    for file in v["files"].as_array().unwrap() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file["path"].as_str().unwrap());
        let bytes = std::fs::read(path).unwrap();
        let check = if file["prefix_only"] == true {
            &bytes[..file["bytes"].as_u64().unwrap() as usize]
        } else {
            &bytes
        };
        assert_eq!(
            Digest::of(check).as_str(),
            file["sha256"],
            "{}",
            file["path"]
        );
    }
}
