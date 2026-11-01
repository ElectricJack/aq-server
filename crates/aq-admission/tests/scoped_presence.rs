mod scoped_presence_common;
use aq_admission::scoped_presence::*;
use scoped_presence_common::*;
use serde_json::json;
#[test]
fn actual_private_record_hash_links_exact_historical_installation_without_retrieval_extension() {
    let r = report();
    r.validate_against(&common::lease(), &record().receipt().unwrap())
        .unwrap();
    assert!(record().delivery.lease.expires_at_ms < 2200);
    assert_eq!(
        positive_expiry(2100, 2200, 30_000, 30_000, 61_000).unwrap(),
        32100
    );
    let Presence::Present { installation, .. } = r.presence else {
        panic!()
    };
    assert_eq!(
        installation.receipt().unwrap().private_binding,
        record().receipt().unwrap().private_binding
    );
}
#[test]
fn network_delay_cannot_turn_old_issuance_into_new_observation_age() {
    assert_eq!(
        positive_expiry(1000, 29_000, 30_000, 30_000, 61_000).unwrap(),
        31_000
    );
    assert!(positive_expiry(1000, 31_000, 30_000, 30_000, 61_000).is_err());
    assert!(positive_expiry(2000, 1999, 30_000, 30_000, 61_000).is_err());
    assert!(positive_expiry(u64::MAX - 5, u64::MAX - 4, 1000, 1000, u64::MAX).is_err());
    assert_eq!(
        positive_expiry(1000, 2000, 30_000, 30_000, 3000).unwrap(),
        3000
    );
}
#[test]
fn changed_private_binding_and_cross_boot_lease_receipt_refuse() {
    let mut r = report();
    r.observation.kernel_boot_id = common::id(500);
    assert!(r.validate().is_err());
    let mut r = report();
    r.binding.session = aq_admission::SessionIdentity::Central {
        authority_id: common::id(501),
        session_id: common::id(502),
        generation: 1,
    };
    assert!(r.validate().is_err());
    let mut receipt = record().receipt().unwrap();
    receipt.operation_id = common::id(503);
    assert!(
        report()
            .validate_against(&common::lease(), &receipt)
            .is_err()
    );
    let mut r = report();
    if let Presence::Present {
        observed_material_digest,
        ..
    } = &mut r.presence
    {
        *observed_material_digest = aq_admission::Digest::of(b"different actual bytes");
    }
    assert!(r.validate().is_err());
    let mut r = report();
    if let Presence::Present {
        observed_material_bytes,
        ..
    } = &mut r.presence
    {
        *observed_material_bytes += 1;
    }
    assert!(r.validate().is_err());
    let mut r = report();
    r.private_binding.digest = aq_admission::Digest::of(b"changed");
    assert!(r.validate().is_err());
}
#[test]
fn positive_requires_live_local_permit_and_later_private_read_but_negative_never_grants() {
    let mut r = report();
    r.observation.permit_deadline_ns = r.observation.sent_ns;
    assert!(r.validate().is_err());
    r.presence = Presence::Missing {
        reason: MissingReason::Missing,
    };
    r.validate().unwrap();
    assert!(!r.presence.is_present());
    r.presence = Presence::Unknown {
        reason: UnknownReason::Unreadable,
    };
    r.validate().unwrap();
    let mut r = report();
    r.observation.read_started_ns = r.observation.permit_acquired_ns - 1;
    assert!(r.validate().is_err());
    let mut r = report();
    r.observation.sent_ns = r.observation.read_started_ns + 2_000_000_001;
    assert!(r.validate().is_err());
}
#[test]
fn inventory_has_only_explicit_provisionability_and_bounds_distinct_profiles() {
    inventory().validate().unwrap();
    qualification().validate().unwrap();
    let mut i = inventory();
    i.provisionable_only.push(i.provisionable_only[0].clone());
    assert!(i.validate().is_err());
    let mut v = serde_json::to_value(inventory()).unwrap();
    v["available"] = json!(true);
    assert!(decode_inventory(&serde_json::to_vec(&v).unwrap()).is_err());
    let mut i = inventory();
    i.ttl_ms = 30_001;
    assert!(i.validate().is_err());
    let mut i = inventory();
    i.sequence = 0;
    assert!(i.validate().is_err());
}
#[test]
fn decoders_reject_unknown_duplicate_fields_protocol_and_raw_size() {
    let bytes = serde_json::to_vec(&report()).unwrap();
    decode_report(&bytes).unwrap();
    let mut v = serde_json::to_value(report()).unwrap();
    v["principal_id"] = json!("chosen");
    assert!(decode_report(&serde_json::to_vec(&v).unwrap()).is_err());
    let raw = String::from_utf8(bytes).unwrap();
    let duplicate = raw.replacen(
        "{",
        "{\"operation_id\":\"00000000-0000-0000-0000-00000000005d\",",
        1,
    );
    assert!(decode_report(duplicate.as_bytes()).is_err());
    let mut v = serde_json::to_value(report()).unwrap();
    v["negotiation"]["protocol"] = json!("unqualified");
    assert!(decode_report(&serde_json::to_vec(&v).unwrap()).is_err());
    assert!(decode_report(&vec![b' '; MAX_REQUEST_BYTES + 1]).is_err());
}
#[test]
fn receipt_replay_pins_original_observation_and_scope_without_fresh_authority() {
    let r = report();
    let ack = PresenceReceipt {
        schema_version: 1,
        operation_id: r.operation_id,
        identity: r.identity.clone(),
        negotiation: r.negotiation.clone(),
        binding: r.binding.clone(),
        grant_id: r.grant_id,
        lease_id: r.lease_id,
        sequence: r.sequence,
        request_digest: r.digest().unwrap(),
        inventory_at_record: r.inventory.clone(),
        recorded_at_ms: 2200,
        expires_at_ms: 32100,
        disposition: Disposition::Recorded,
    };
    ack.validate_for(&r).unwrap();
    let mut changed = r.clone();
    changed.sequence += 1;
    assert!(ack.validate_for(&changed).is_err());
    let mut changed = r;
    changed.observation.sent_ns += 1;
    assert!(ack.validate_for(&changed).is_err());
    assert_eq!(ack.recorded_at_ms, 2200);
    assert_eq!(ack.expires_at_ms, 32100);
}
#[test]
fn published_schemas_fixtures_and_private_record_have_independent_drift_checks() {
    let fixtures = [
        (
            inventory_schema(),
            include_str!("../artifacts/scoped-presence-inventory.schema.json"),
        ),
        (
            report_schema(),
            include_str!("../artifacts/scoped-presence-report.schema.json"),
        ),
        (
            receipt_schema(),
            include_str!("../artifacts/scoped-presence-receipt.schema.json"),
        ),
        (
            inventory_receipt_schema(),
            include_str!("../artifacts/scoped-presence-inventory-receipt.schema.json"),
        ),
        (
            qualification_schema(),
            include_str!("../artifacts/scoped-presence-qualification.schema.json"),
        ),
    ];
    for (actual, expected) in fixtures {
        assert_eq!(
            actual,
            serde_json::from_str::<serde_json::Value>(expected).unwrap()
        );
    }
    let r = decode_report(include_bytes!(
        "../artifacts/scoped-presence-report.fixture.json"
    ))
    .unwrap();
    assert_eq!(r.digest().unwrap(), report().digest().unwrap());
    let i = decode_inventory(include_bytes!(
        "../artifacts/scoped-presence-inventory.fixture.json"
    ))
    .unwrap();
    assert_eq!(i.digest().unwrap(), inventory().digest().unwrap());
    assert_eq!(
        serde_json::to_value(record()).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../artifacts/scoped-presence-private-record.fixture.json"
        ))
        .unwrap()
    );
    assert_eq!(
        serde_json::to_value(qualification()).unwrap(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../artifacts/scoped-presence-qualification.fixture.json"
        ))
        .unwrap()
    );
    let schema = report_schema();
    assert_eq!(schema["x-max-document-bytes"], MAX_REQUEST_BYTES);
    assert_eq!(schema["$defs"]["Digest"]["pattern"], "^[0-9a-f]{64}$");
    assert_eq!(schema["$defs"]["Text"]["x-utf8-byte-limit"], 512);
}
#[test]
fn references_are_bounded_by_actual_utf8_bytes_not_only_characters() {
    let mut r = serde_json::to_value(report()).unwrap();
    r["runtime"]["id"] = json!("🚀".repeat(128));
    decode_report(&serde_json::to_vec(&r).unwrap()).unwrap();
    r["runtime"]["id"] = json!("🚀".repeat(129));
    assert!(decode_report(&serde_json::to_vec(&r).unwrap()).is_err());
    let mut i = serde_json::to_value(inventory()).unwrap();
    i["qualifications"][0]["version"] = json!("🚀".repeat(129));
    assert!(decode_inventory(&serde_json::to_vec(&i).unwrap()).is_err());
}
