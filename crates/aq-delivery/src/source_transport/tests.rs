use super::*;

#[test]
fn release_must_advance_the_exact_guarded_handle() {
    let handle = Uuid::new_v4();
    let repository = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let refs = json!([{"name": "refs/aq-git/transfers/checkpoint", "oid": "a".repeat(40)}]);
    let request = json!({
        "operation_id": operation, "repository_id": repository, "expected_binding_generation": 3,
        "params": {"handle_id": handle, "expected_generation": 7, "refs": refs, "reason": "source_retention.max_age"}
    });
    let valid = json!({
        "version": 1, "id": handle, "repository_id": repository, "binding_generation": 3,
        "generation": 8, "state": "released", "refs": refs,
        "release_operation": operation, "release_reason": "source_retention.max_age"
    });
    assert!(release_confirmed(&request, &valid));
    for (field, wrong) in [
        ("version", json!(2)),
        ("id", json!(Uuid::new_v4())),
        ("repository_id", json!(Uuid::new_v4())),
        ("binding_generation", json!(2)),
        ("generation", json!(7)),
        ("state", json!("active")),
        ("refs", json!([])),
        ("release_operation", Value::Null),
        ("release_reason", json!("another reason")),
    ] {
        let mut result = valid.clone();
        result[field] = wrong;
        assert!(!release_confirmed(&request, &result), "{field}");
    }
}

#[test]
fn an_import_can_be_historical_but_must_bind_the_artifact_and_receiver() {
    let receipt = CentralRetentionReceipt {
        receipt_id: Uuid::new_v4(),
        transfer_id: Uuid::new_v4(),
        repository_id: Uuid::new_v4(),
        binding_generation: 3,
        source: ObjectId {
            algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
            hex: "b".repeat(40),
        },
        retention_handle: Uuid::new_v4(),
        manifest_digest: "d".repeat(64),
    };
    let mut value = json!({
        "repository_id": receipt.repository_id, "binding_generation": 3,
        "transfer_id": receipt.retention_handle, "retention_handle": receipt.retention_handle,
        "manifest_digest": receipt.manifest_digest, "source": receipt.source,
        "retention_state": "active", "historical": false, "receiver_installation_id": Uuid::new_v4()
    });
    assert!(matches_import_receipt(&value, &receipt).unwrap());
    value["historical"] = json!(true);
    assert!(matches_import_receipt(&value, &receipt).unwrap());
    for (field, wrong) in [
        ("transfer_id", json!(receipt.transfer_id)),
        ("retention_handle", json!(receipt.transfer_id)),
        ("source", Value::Null),
        ("historical", json!("true")),
        ("receiver_installation_id", json!("invalid")),
    ] {
        let mut result = value.clone();
        result[field] = wrong;
        assert!(
            !matches_import_receipt(&result, &receipt).unwrap(),
            "{field}"
        );
    }
}
