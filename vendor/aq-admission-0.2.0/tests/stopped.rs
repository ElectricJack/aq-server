use aq_admission::{Digest, Text, started, stopped::*, transport::*};
use serde_json::json;
fn request() -> Request {
    serde_json::from_str(include_str!("../artifacts/stopped.fixture.json")).unwrap()
}
fn started() -> started::Request {
    serde_json::from_str(include_str!("../artifacts/started.fixture.json")).unwrap()
}
fn seal(r: &mut Request) {
    if let StopDisposition::Stopped {
        stop_receipt_digest,
        ..
    } = &mut r.evidence.disposition
    {
        *stop_receipt_digest = r.proof.digest().unwrap();
    }
}
#[test]
fn public_schema_and_independent_canonical_vectors_match() {
    let r = request();
    r.validate_after(&started()).unwrap();
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/stopped.python-vectors.json")).unwrap();
    assert_eq!(r.digest().unwrap().as_str(), vectors["request"]);
    assert_eq!(r.proof.digest().unwrap().as_str(), vectors["proof"]);
    assert_eq!(
        request_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!("../artifacts/stopped.schema.json"))
            .unwrap()
    );
    let mut unicode = r;
    unicode.runtime.id = Text::new("runtime/雪").unwrap();
    assert_eq!(
        unicode.digest().unwrap().as_str(),
        vectors["unicode_request"]
    );
}
#[test]
fn expired_stop_is_valid_but_unknown_or_digest_only_is_not_proof() {
    let r = request();
    assert!(r.proof.observation.observed_boot_ms > r.proof.observation.permit_deadline_boot_ms);
    r.validate_after(&started()).unwrap();
    for phase in [
        "waiting",
        "starting",
        "running",
        "stopping",
        "stop_uncertain",
    ] {
        let mut v = serde_json::to_value(&r).unwrap();
        v["proof"]["observation"]["phase"] = json!(phase);
        assert!(decode_request(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    for (field, value) in [
        ("kernel_wait_echild", json!(false)),
        ("root_status", json!(null)),
        ("pane", json!(null)),
    ] {
        let mut v = serde_json::to_value(&r).unwrap();
        v["proof"]["observation"][field] = value;
        assert!(decode_request(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    let mut missing = serde_json::to_value(&r).unwrap();
    missing.as_object_mut().unwrap().remove("proof");
    assert!(decode_request(&serde_json::to_vec(&missing).unwrap()).is_err());
}
#[test]
fn exact_physical_owner_cannot_be_substituted_or_regressed() {
    let original = started();
    for mutate in [
        |r: &mut Request| r.proof.observation.guardian.nonce = uuid::Uuid::from_u128(999),
        |r: &mut Request| {
            r.proof.observation.guardian.intent.configuration_digest = Digest::of(b"other")
        },
        |r: &mut Request| r.proof.observation.pane.pane.start_ticks += 1,
        |r: &mut Request| r.proof.observation.guardian.machine_boot_id = uuid::Uuid::from_u128(998),
        |r: &mut Request| r.proof.observation.observed_boot_ms = 1,
        |r: &mut Request| r.evidence.guardian_id = uuid::Uuid::from_u128(997),
        |r: &mut Request| r.reservation_id = uuid::Uuid::from_u128(996),
        |r: &mut Request| {
            r.evidence.journal = JournalEvidence::Recorded {
                revision: 3,
                digest: Digest::of(b"older"),
            }
        },
    ] {
        let mut r = request();
        mutate(&mut r);
        seal(&mut r);
        assert!(r.validate_after(&original).is_err());
    }
    // A later permit may shorten the deadline. It must not prevent recovery of
    // the real stopped proof. Reusing an old sequence with changed expiry fails.
    let mut r = request();
    r.proof.observation.permit_deadline_boot_ms -= 1;
    seal(&mut r);
    assert!(r.validate_after(&original).is_err());
    r.proof.observation.permit_sequence += 1;
    seal(&mut r);
    r.validate_after(&original).unwrap();
}
#[test]
fn acknowledgement_is_historical_and_bound_to_the_original_request() {
    let r = request();
    let receipt = Receipt {
        schema_version: 1,
        operation_id: r.evidence.stop_operation_id,
        binding: r.evidence.binding.clone(),
        grant_id: r.grant_id().unwrap(),
        request_digest: r.digest().unwrap(),
        recorded_at_ms: 90_000,
        authority_revision_at_record: 9,
    };
    let bytes = serde_json::to_vec(&receipt).unwrap();
    decode_receipt(&bytes, &r).unwrap();
    let mut changed = r.clone();
    changed.evidence.reason = StopReason::Completed;
    assert!(decode_receipt(&bytes, &changed).is_err());
    assert_eq!(receipt.artifact().unwrap().digest, {
        let mut v = serde_json::to_value(&receipt).unwrap();
        v.sort_all_objects();
        Digest::of(&serde_json::to_vec(&v).unwrap())
    });
    let mut v = serde_json::to_value(&r).unwrap();
    v["release_task"] = json!(true);
    assert!(decode_request(&serde_json::to_vec(&v).unwrap()).is_err());
    assert!(decode_request(&vec![b' '; MAX_BYTES + 1]).is_err());
}
