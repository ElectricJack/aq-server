use super::*;
#[test]
fn serialization_and_decode_are_bounded_and_unknown_fields_refuse() {
    let c = case();
    for d in documents(&c) {
        let bytes = serde_json::to_vec(&d).unwrap();
        decode(&bytes).unwrap();
    }
    assert!(decode(&vec![b'x'; MAX_BYTES + 1]).is_err());
    let mut v = serde_json::to_value(Document::Prepare(Box::new(c.prepare))).unwrap();
    v["body"]["eligible"] = json!(true);
    assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
}
#[test]
fn structural_claims_are_not_authenticated_owner_truth() {
    let c = case();
    let _: () = c
        .execution
        .validate_for(&c.prepare, &c.accepted, &c.accept, &c.proposal)
        .unwrap();
    assert!(
        c.proposal
            .owners
            .iter()
            .all(|o| o.authority.id.as_str().starts_with("synthetic-owner/"))
    );
    assert!(
        schema()["$comment"]
            .as_str()
            .unwrap()
            .contains("no authentication")
    );
}
#[test]
fn generated_artifacts_and_independent_canonical_vectors_match() {
    let c = case();
    let load = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
    assert_eq!(
        load(include_str!("../../artifacts/preparation-v2.fixture.json")),
        fixtures()
    );
    assert_eq!(
        load(include_str!("../../artifacts/preparation-v2.schema.json")),
        schema()
    );
    assert_eq!(
        load(include_str!("../../artifacts/preparation-v2.contract.json")),
        json!({"artifact":contract_artifact().unwrap(),"manifest":contract_manifest().unwrap()})
    );
    let v = load(include_str!(
        "../../artifacts/preparation-v2.python-vectors.json"
    ));
    for (key, d) in [
        ("proposal", c.proposal.digest().unwrap()),
        ("accept", c.accept.digest().unwrap()),
        ("accepted", c.accepted.digest().unwrap()),
        ("prepare", c.prepare.digest().unwrap()),
        ("execution", c.execution.digest().unwrap()),
        ("confirmation", c.execution.confirmation.digest().unwrap()),
    ] {
        assert_eq!(d.as_str(), v[key], "{key}");
    }
    for (i, f) in c.execution.confirmation.facts.iter().enumerate() {
        assert_eq!(
            OwnerFact::record_digest(&f.body).unwrap().as_str(),
            v["facts"][i]
        );
    }
    let mut f = c.execution.confirmation.facts[0].body.clone();
    f.authority.id = Text::new("synthetic-owner/雪").unwrap();
    assert_eq!(
        OwnerFact::record_digest(&f).unwrap().as_str(),
        v["unicode_fact"]
    );
    assert_eq!(contract_artifact().unwrap().digest.as_str(), v["contract"]);
    assert_eq!(contract_manifest().unwrap()["schema_digest"], v["schema"]);
}
#[test]
fn exact_qualified_018_source_and_artifacts_are_preserved() {
    let v: serde_json::Value = serde_json::from_str(include_str!(
        "../../artifacts/preparation-v2.compatibility-sha256.json"
    ))
    .unwrap();
    for f in v["files"].as_array().unwrap() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f["path"].as_str().unwrap());
        let b = std::fs::read(path).unwrap();
        let check = if f["prefix"] == true {
            &b[..f["bytes"].as_u64().unwrap() as usize]
        } else {
            &b
        };
        assert_eq!(Digest::of(check).as_str(), f["sha256"], "{}", f["path"]);
    }
}
#[test]
fn v2_decoder_never_falls_back_to_legacy_or_different_schema() {
    let c = case();
    assert!(
        decode(
            &serde_json::to_vec(&aq_admission::transport::TransportDocument::Prepare(
                c.prepare.original.clone()
            ))
            .unwrap()
        )
        .is_err()
    );
    for d in documents(&c) {
        let mut v = serde_json::to_value(d).unwrap();
        v["body"]["schema_version"] = json!(1);
        assert!(decode(&serde_json::to_vec(&v).unwrap()).is_err());
    }
}
