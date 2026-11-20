use aq_admission::{
    ArtifactRef, Digest, ObjectFormat, ObjectId, SourceAdmission, SourceSelection, Text,
};
use aq_server::profiled_execution::source::validate_body;
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn named_ref_follows_publication_head_only_for_its_exact_commit_and_handle() {
    let (project, repository, handle) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let reference = "refs/heads/main";
    let oid = "a".repeat(40);
    let head = json!({"schema_version":1,"project_id":project,"repository_id":repository,"binding_generation":2,
        "target":reference,"sequence":1,"oid":oid,"retention_handle":handle,"candidate_operation_id":handle,
        "publication_operation_id":Uuid::new_v4(),"previous":"b".repeat(40)});
    let admission = SourceAdmission {
        schema_version: 1,
        project_id: project,
        project_revision: 1,
        repository_id: repository,
        repository_binding_generation: 2,
        policy: ArtifactRef {
            id: Text::new("aq.project-source-policy/test").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"policy"),
        },
        selection: SourceSelection::NamedRef {
            reference: aq_admission::Label::new(reference).unwrap(),
        },
        resolved_object: ObjectId {
            format: ObjectFormat::Sha1,
            hex: oid.clone(),
        },
        observed_at_ms: 1000,
        expires_at_ms: 31_000,
    };
    let body = |head: Value, handle: Uuid| {
        serde_json::to_vec(&json!({"schema_version":1,"admission":admission,"retention_handle":handle,
            "evidence":{"publication_head":head,
                "commit_inspection":{"oid":{"algorithm":"sha1","hex":oid},"tree":{"algorithm":"sha1","hex":"c".repeat(40)}},
                "retention_inspection":{"version":1,"id":handle,"repository_id":repository,"binding_generation":2,
                    "generation":1,"state":"active","refs":[{"name":"refs/aq-git/candidates/x/prefix/0","oid":oid}],
                    "release_operation":null,"release_reason":null}}}))
        .unwrap()
    };
    assert!(validate_body(&body(head.clone(), handle), project, 2000).is_ok());
    for (field, bad) in [
        ("target", json!("refs/heads/other")),
        ("oid", json!("d".repeat(40))),
        ("retention_handle", json!(Uuid::new_v4())),
        ("repository_id", json!(Uuid::new_v4())),
        ("project_id", json!(Uuid::new_v4())),
        ("binding_generation", json!(3)),
        ("sequence", json!(0)),
    ] {
        let mut value = head.clone();
        value[field] = bad;
        assert!(
            validate_body(&body(value, handle), project, 2000).is_err(),
            "{field}"
        );
    }
    // Another handle than the head's is never followed.
    let other = Uuid::new_v4();
    let mut value = head.clone();
    value["retention_handle"] = json!(other);
    value["candidate_operation_id"] = json!(other);
    assert!(validate_body(&body(value, handle), project, 2000).is_err());
}
