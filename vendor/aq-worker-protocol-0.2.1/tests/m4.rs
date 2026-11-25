use aq_worker_protocol::*;
#[test]
fn frozen_bindings_are_strict_and_part_of_spec_digest() {
    let mut spec:ExecutionSpec=serde_json::from_value(serde_json::json!({"project_id":"11111111-1111-4111-8111-111111111111","harness":"deterministic-agent-v1","resolved_spec_digest":"","fixture_output":"","resources":{"slots":1,"cpu_millis":0,"memory_bytes":0,"disk_bytes":0},"required_capabilities":[]})).unwrap();
    let old = spec.clone().seal();
    assert!(
        !serde_json::to_value(&old)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("bindings")
    );
    spec.required_capabilities
        .push(GRAPH_ACCESS_CAPABILITY.into());
    spec.bindings = Some(ExecutionBindings {
        source: None,
        graph: Some(GraphBinding {
            workspace_id: uuid::Uuid::from_u128(1),
            profile_id: "coding".into(),
            profile_revision: "1".into(),
            profile_digest: "a".repeat(64),
            context_digest: "b".repeat(64),
        }),
    });
    let mut frozen = spec.seal();
    frozen.validate().unwrap();
    assert_ne!(old.resolved_spec_digest, frozen.resolved_spec_digest);
    frozen
        .bindings
        .as_mut()
        .unwrap()
        .graph
        .as_mut()
        .unwrap()
        .context_digest = "c".repeat(64);
    assert!(frozen.validate().is_err());
    assert!(serde_json::from_value::<SourceBinding>(serde_json::json!({"repository_id":"11111111-1111-4111-8111-111111111111","binding_generation":1,"logical_identity":"test","base":{"algorithm":"sha1","hex":"a".repeat(40)},"credentials":"secret"})).is_err());
}
#[test]
fn source_and_raw_chunk_bounds_are_validated() {
    let mut oid = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: "a".repeat(40),
    };
    oid.validate().unwrap();
    oid.hex = "A".repeat(40);
    assert!(oid.validate().is_err());
    let mut request = TransferChunkRequest {
        identity: NodeIdentity {
            node_id: uuid::Uuid::from_u128(1),
            boot_id: uuid::Uuid::from_u128(2),
        },
        operation_id: uuid::Uuid::from_u128(3),
        transfer_id: uuid::Uuid::from_u128(4),
        file: TransferFile::Bundle,
        offset: 0,
        length: MAX_CHUNK_BYTES as u32,
        digest: "a".repeat(64),
    };
    request.validate().unwrap();
    request.length += 1;
    assert!(request.validate().is_err());
}
