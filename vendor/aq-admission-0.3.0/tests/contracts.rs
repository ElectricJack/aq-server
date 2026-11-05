mod common;
use aq_admission::*;
use common::*;
use serde_json::{Value, json};
#[test]
fn published_schema_and_fixtures_drift_test_packaged_artifacts() {
    let schema: Value =
        serde_json::from_str(include_str!("../artifacts/contract.schema.json")).unwrap();
    assert_eq!(contract_schema(), schema);
    let bytes = include_bytes!("../artifacts/fixtures.json");
    let values: Vec<Value> = serde_json::from_slice(bytes).unwrap();
    assert_eq!(values.len(), documents().len());
    for value in values {
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(value, serde_json::to_value(decoded).unwrap());
    }
}
#[test]
fn mutations_reject_unknown_duplicate_nil_unbounded_and_wrong_versions() {
    let good = serde_json::to_value(ContractDocument::MaterialLease(lease())).unwrap();
    for patch in [
        json!({"schema_version":2}),
        json!({"lease_id":id(0)}),
        json!({"credential_revision":0}),
        json!({"material_bytes":65537}),
        json!({"expires_at_ms":121001}),
        json!({"issued_at_ms":0}),
        json!({"chosen_principal":id(1)}),
    ] {
        let mut invalid = good.clone();
        for (k, v) in patch.as_object().unwrap() {
            invalid["body"][k] = v.clone();
        }
        assert!(
            decode(&serde_json::to_vec(&invalid).unwrap()).is_err(),
            "{patch}"
        );
    }
    let duplicate = serde_json::to_string(&good).unwrap().replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    assert!(decode(duplicate.as_bytes()).is_err());
    let mut invalid = good;
    invalid["body"]["provider"] = json!("é".repeat(257));
    assert!(decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
    assert!(decode(&vec![b' '; 524_289]).is_err());
}
#[test]
fn material_bytes_and_receipts_are_exact_attempt_binding_not_presence_claims() {
    let lease = lease();
    lease
        .verify_material(&binding(), b"synthetic-key", 1500)
        .unwrap();
    assert!(
        lease
            .verify_material(&binding(), b"different-key", 1500)
            .is_err()
    );
    assert!(
        lease
            .verify_material(&binding(), b"synthetic-key", 2000)
            .is_err()
    );
    let mut foreign = binding();
    foreign.graph_access_profile_digest = Digest::of(b"other-graph-profile");
    assert!(
        lease
            .verify_material(&foreign, b"synthetic-key", 1500)
            .is_err()
    );
    foreign = binding();
    if let SessionIdentity::Remote { boot_id, .. } = &mut foreign.session {
        *boot_id = id(99);
    }
    assert!(
        lease
            .verify_material(&foreign, b"synthetic-key", 1500)
            .is_err()
    );
    let mut receipt = MaterialReceipt {
        schema_version: 1,
        operation_id: id(17),
        lease_id: lease.lease_id,
        binding: binding(),
        material_digest: lease.material_digest.clone(),
        private_binding: artifact("private-credentials"),
        received_at_ms: 1500,
    };
    receipt.validate_against(&lease, 1600).unwrap();
    receipt.material_digest = Digest::of(b"other");
    assert!(receipt.validate_against(&lease, 1600).is_err());
    receipt.material_digest = lease.material_digest.clone();
    receipt.received_at_ms = 1700;
    assert!(receipt.validate_against(&lease, 1600).is_err());
    let mut central = lease.clone();
    central.binding.session = SessionIdentity::Central {
        authority_id: id(90),
        session_id: id(14),
        generation: 1,
    };
    assert!(central.validate().is_err());
}
#[test]
fn source_requires_explicit_safe_ref_or_matching_immutable_object_and_freshness() {
    let source = source();
    source.at(1500).unwrap();
    assert!(source.at(2000).is_err());
    for reference in [
        "main",
        "HEAD",
        "-upload-pack=evil",
        "refs/heads/a..b",
        "refs/heads/.hidden",
        "refs/heads/a.lock",
        "refs/heads/a@{1}",
        "refs/heads/a\nb",
    ] {
        let mut bad = source.clone();
        bad.selection = SourceSelection::NamedRef {
            reference: Label::new(reference).unwrap_or_else(|_| Label::new("HEAD").unwrap()),
        };
        assert!(bad.validate().is_err(), "{reference}");
    }
    let mut immutable = source.clone();
    immutable.selection = SourceSelection::Immutable {
        object: immutable.resolved_object.clone(),
    };
    immutable.validate().unwrap();
    immutable.resolved_object.hex = "b".repeat(40);
    assert!(immutable.validate().is_err());
    immutable.resolved_object.format = ObjectFormat::Sha256;
    assert!(immutable.validate().is_err());
}
#[test]
fn artifact_requires_real_matching_bytes_and_current_retention_metadata() {
    let artifact = retained();
    artifact.verify(b"actual-artifact", 1500).unwrap();
    assert!(artifact.verify(b"changed-artifact", 1500).is_err());
    assert!(artifact.verify(b"actual-artifact", 2000).is_err());
    let mut oversized = artifact;
    oversized.bytes = 2_097_153;
    assert!(oversized.validate().is_err());
    let offer: ProfiledOffer =
        serde_json::from_str(include_str!("../artifacts/shared-launch.json")).unwrap();
    assert!(verify_offer_artifacts(&offer, &[], 1500).is_err());
}
#[test]
fn shipped_shared_offer_uses_exact_existing_launch_binding_bridge() {
    use aq_session_protocol::Validate;
    let offer: ProfiledOffer =
        serde_json::from_str(include_str!("../artifacts/shared-launch.json")).unwrap();
    offer.validate().unwrap();
    let mut material = lease();
    material.binding = offer.binding.clone();
    material.provider = Label::new(offer.manifest.model.provider.as_str()).unwrap();
    // Shared baseline offer requests no secret. A material owner may not inject
    // an unrelated credential merely because a valid node/attempt exists.
    material.issued_at_ms = 1000;
    material.expires_at_ms = 2000;
    if offer.manifest.credentials.is_empty() {
        assert!(material.validate_offer(&offer, 1500).is_err());
    }
    let mut changed = offer;
    changed.binding.launch_manifest_digest = Digest::of(b"other-manifest");
    assert!(changed.validate().is_err());
}
#[test]
fn retained_role_and_byte_coverage_validates_against_resealed_public_offer() {
    use aq_session_protocol::{Validate, canonical_digest};
    let mut offer: ProfiledOffer =
        serde_json::from_str(include_str!("../artifacts/shared-launch.json")).unwrap();
    offer.manifest.bindings.source = artifact("source");
    offer.manifest.bindings.outcome = artifact("outcome");
    offer.manifest.selection_explanation = artifact("selection");
    offer.binding.launch_manifest_digest = canonical_digest(&offer.manifest).unwrap();
    offer.validate().unwrap();
    let descriptors: Vec<_> = [
        ("fixture", ArtifactKind::Definition),
        ("source", ArtifactKind::Source),
        ("outcome", ArtifactKind::Outcome),
        ("selection", ArtifactKind::Selection),
    ]
    .into_iter()
    .map(|(name, kind)| RetainedArtifact {
        artifact: artifact(name),
        kind,
        bytes: name.len() as u64,
        retention_receipt: artifact("synthetic-owner-receipt"),
        retained_at_ms: 1000,
        retain_until_ms: 2000,
    })
    .collect();
    for descriptor in &descriptors {
        descriptor
            .verify(descriptor.artifact.id.as_str().as_bytes(), 1500)
            .unwrap();
    }
    verify_offer_artifacts(&offer, &descriptors, 1500).unwrap();
    let mut wrong = descriptors.clone();
    wrong[1].kind = ArtifactKind::Definition;
    assert!(verify_offer_artifacts(&offer, &wrong, 1500).is_err());
    wrong = descriptors;
    wrong[0].bytes = 8;
    assert!(verify_offer_artifacts(&offer, &wrong, 1500).is_err());
}
