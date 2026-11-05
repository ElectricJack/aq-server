mod spec_common;
use aq_admission::{execution_spec::*, transport::task_lineage_id};
use serde_json::{Value, json};
#[test]
fn typed_worker_artifact_is_digest_bound_without_a_manifest_cycle() {
    let (spec, offer) = spec_common::fixture();
    spec.validate_against(&offer).unwrap();
    assert_eq!(
        spec.artifact().unwrap().digest,
        offer.binding.execution_spec_digest
    );
    let mut changed = spec.clone();
    changed.task.content_revision += 1;
    assert!(changed.validate_against(&offer).is_err());
    let mut changed = offer.clone();
    changed.binding.execution_spec_digest = aq_admission::Digest::of(b"arbitrary-server-json");
    assert!(spec.validate_against(&changed).is_err());
    let mut changed = spec.clone();
    changed.source.project_revision += 1;
    assert!(changed.validate().is_err());
    let mut changed = spec.clone();
    changed.lifecycle.lineage_id = task_lineage_id(
        changed.lifecycle.project_id,
        changed.task.workspace_id,
        uuid::Uuid::from_u128(99),
    )
    .unwrap();
    assert!(changed.validate().is_err());
}
#[test]
fn publication_is_independent_and_decode_rejects_caller_secret_unknown_duplicate_fields() {
    let fixture: Value =
        serde_json::from_str(include_str!("../artifacts/execution-spec.fixture.json")).unwrap();
    let schema: Value =
        serde_json::from_str(include_str!("../artifacts/execution-spec.schema.json")).unwrap();
    assert_eq!(execution_spec_schema(), schema);
    let bytes = serde_json::to_vec(&fixture["spec"]).unwrap();
    let spec = decode_profiled_spec(&bytes).unwrap();
    let offer = serde_json::from_value(fixture["offer"].clone()).unwrap();
    spec.validate_against(&offer).unwrap();
    let mut bad = fixture["spec"].clone();
    bad["provider_key"] = json!("never-allowed");
    assert!(decode_profiled_spec(&serde_json::to_vec(&bad).unwrap()).is_err());
    let raw = serde_json::to_string(&fixture["spec"]).unwrap();
    let duplicate = raw.replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert_ne!(raw, duplicate);
    assert!(decode_profiled_spec(duplicate.as_bytes()).is_err());
    assert!(decode_profiled_spec(&vec![b' '; 262145]).is_err());
}

#[test]
fn spec_observation_context_and_canonical_bytes_must_all_be_retained() {
    use aq_admission::{ArtifactKind, RetainedArtifact};
    let (spec, offer) = spec_common::fixture();
    let reference = |name: &str| aq_admission::ArtifactRef {
        id: aq_admission::Text::new(name).unwrap(),
        version: aq_admission::Text::new("1").unwrap(),
        digest: aq_admission::Digest::of(name.as_bytes()),
    };
    let mut descriptors: Vec<_> = [
        ("fixture", ArtifactKind::Definition),
        ("source", ArtifactKind::Source),
        ("outcome", ArtifactKind::Outcome),
        ("selection", ArtifactKind::Selection),
        ("task-context", ArtifactKind::Context),
        ("task-observation", ArtifactKind::Context),
    ]
    .into_iter()
    .map(|(name, kind)| RetainedArtifact {
        artifact: reference(name),
        kind,
        bytes: name.len() as u64,
        retention_receipt: reference("owner-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    })
    .collect();
    descriptors.push(RetainedArtifact {
        artifact: spec.artifact().unwrap(),
        kind: ArtifactKind::Composition,
        bytes: spec.canonical_bytes().unwrap().len() as u64,
        retention_receipt: reference("owner-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    });
    spec.validate_retained(&offer, &descriptors, 1500).unwrap();
    descriptors
        .last_mut()
        .unwrap()
        .verify(&spec.canonical_bytes().unwrap(), 1500)
        .unwrap();
    let mut wrong = descriptors.clone();
    wrong.last_mut().unwrap().bytes += 1;
    assert!(spec.validate_retained(&offer, &wrong, 1500).is_err());
    wrong = descriptors.clone();
    wrong.retain(|descriptor| descriptor.artifact != spec.task_observation);
    assert!(spec.validate_retained(&offer, &wrong, 1500).is_err());
    let mut optional = offer.clone();
    optional.manifest.context.references[0].required = false;
    optional.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&optional.manifest).unwrap();
    assert!(spec.validate_against(&optional).is_err());
}
