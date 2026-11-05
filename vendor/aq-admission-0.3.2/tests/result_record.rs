use aq_admission::{
    Digest, result_record as record,
    transport::{ResultReport, StopDisposition},
};
fn fixture() -> (ResultReport, record::Receipt) {
    let raw: serde_json::Value =
        serde_json::from_str(include_str!("../artifacts/result-record.fixture.json")).unwrap();
    (
        record::decode_report(&serde_json::to_vec(&raw["report"]).unwrap()).unwrap(),
        serde_json::from_value(raw["receipt"].clone()).unwrap(),
    )
}
#[test]
fn recorded_ack_matches_independent_vectors_without_acceptance_authority() {
    let (report, receipt) = fixture();
    receipt.validate_for(&report).unwrap();
    let vectors: serde_json::Value = serde_json::from_str(include_str!(
        "../artifacts/result-record.python-vectors.json"
    ))
    .unwrap();
    assert_eq!(
        record::report_digest(&report).unwrap().as_str(),
        vectors["report_digest"]
    );
    assert_eq!(
        receipt.artifact(&report).unwrap().digest.as_str(),
        vectors["receipt_digest"]
    );
    assert_eq!(
        record::receipt_schema(),
        serde_json::from_str::<serde_json::Value>(include_str!(
            "../artifacts/result-record.schema.json"
        ))
        .unwrap()
    );
    record::decode_receipt(&serde_json::to_vec(&receipt).unwrap(), &report).unwrap();
    for disposition in ["accepted", "eligible", "released", "completed"] {
        let mut value = serde_json::to_value(&receipt).unwrap();
        value["disposition"] = serde_json::json!(disposition);
        assert!(record::decode_receipt(&serde_json::to_vec(&value).unwrap(), &report).is_err());
    }
}
#[test]
fn changed_report_and_foreign_owner_reference_never_reuse_ack() {
    let (report, receipt) = fixture();
    for field in ["operation", "report", "conclusion", "source", "supersedes"] {
        let mut changed = report.clone();
        match field {
            "operation" => changed.operation_id = uuid::Uuid::from_u128(999),
            "report" => changed.report_id = uuid::Uuid::from_u128(999),
            "conclusion" => {
                changed.conclusion = aq_admission::Label::new("Different claim").unwrap()
            }
            "source" => changed.source_receipt_ids.clear(),
            "supersedes" => changed.supersedes = Some(uuid::Uuid::from_u128(999)),
            _ => unreachable!(),
        }
        assert!(receipt.validate_for(&changed).is_err(), "{field}");
    }
    let mut changed = receipt;
    changed.stopped_receipt.id = aq_admission::Text::new("aq.native-stopped/foreign").unwrap();
    assert!(changed.validate_for(&report).is_err());
}
#[test]
fn no_stop_unknown_fields_duplicate_ids_and_byte_overflow_refuse() {
    let (mut report, _) = fixture();
    report.stop.disposition = StopDisposition::Unknown {
        grant_id: Some(uuid::Uuid::from_u128(1)),
    };
    assert!(record::validate_report(&report).is_err());
    let (mut report, _) = fixture();
    report.source_receipt_ids.push(report.source_receipt_ids[0]);
    assert!(record::validate_report(&report).is_err());
    let (report, _) = fixture();
    let mut raw = serde_json::to_value(&report).unwrap();
    raw["task_complete"] = serde_json::json!(true);
    assert!(record::decode_report(&serde_json::to_vec(&raw).unwrap()).is_err());
    let mut bytes = serde_json::to_vec(&report).unwrap();
    bytes.resize(record::MAX_BYTES + 1, b' ');
    assert!(record::decode_report(&bytes).is_err());
    let (report, mut receipt) = fixture();
    receipt.report_digest = Digest::of(b"other");
    assert!(receipt.validate_for(&report).is_err());
}
