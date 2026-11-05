//! Startup policy structure/provenance only: no actual runtime is qualified here.
use aq_admission::{transport::*, *};
use aq_server::profiled_execution::config::*;
use serde_json::json;
use uuid::Uuid;
fn artifact(id: &str) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(id.as_bytes()),
    }
}
fn config() -> ProfiledConfig {
    ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: artifact("synthetic-runtime"),
            qualification: artifact("synthetic-qualification"),
            capabilities: vec![],
            features: vec![Feature::ArtifactTransferV1, Feature::DurableHostJournalV1],
            material_delivery: None,
        }],
        projects: vec![ProjectPolicy {
            project_id: Uuid::from_u128(1),
            enabled: true,
            priority: 0,
            source: SourceSelection::Immutable {
                object: ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: "a".repeat(40),
                },
            },
            retention_handle: Uuid::from_u128(2),
            placement: None,
        }],
    }
}
#[test]
fn policy_artifact_binds_global_qualification_and_exact_source_without_user_actor() {
    let cfg = config();
    cfg.validate().unwrap();
    let record = cfg.project_policy_record(Uuid::from_u128(1)).unwrap();
    assert_eq!(
        record.artifact().unwrap(),
        cfg.project_policy_ref(Uuid::from_u128(1)).unwrap()
    );
    let old = record.artifact().unwrap();
    let mut changed = cfg.clone();
    changed.qualified_runtimes[0].qualification = artifact("different-qualified-code");
    assert_ne!(old, changed.project_policy_ref(Uuid::from_u128(1)).unwrap());
    let mut changed = cfg.clone();
    changed.projects[0].source = SourceSelection::Immutable {
        object: ObjectId {
            format: ObjectFormat::Sha1,
            hex: "b".repeat(40),
        },
    };
    assert_ne!(old, changed.project_policy_ref(Uuid::from_u128(1)).unwrap());
    assert!(cfg.project_policy_record(Uuid::from_u128(3)).is_err());
    let mut wrong = serde_json::to_value(&cfg).unwrap();
    wrong["actor_id"] = json!(Uuid::from_u128(4));
    assert!(serde_json::from_value::<ProfiledConfig>(wrong).is_err());
}
#[test]
fn configuration_and_runtime_claims_refuse_duplicates_unsafe_refs_and_unqualified_artifacts() {
    let cfg = config();
    let mut duplicate = cfg.clone();
    duplicate.projects.push(duplicate.projects[0].clone());
    assert!(duplicate.validate().is_err());
    let mut bad = cfg.clone();
    bad.projects[0].source = SourceSelection::NamedRef {
        reference: Label::new("HEAD").unwrap(),
    };
    assert!(bad.validate().is_err());
    let mut request = Register {
        schema_version: 1,
        operation_id: Uuid::from_u128(5),
        identity: WorkerIdentity {
            node_id: Uuid::from_u128(6),
            boot_id: Uuid::from_u128(7),
        },
        runtime: cfg.qualified_runtimes[0].runtime.clone(),
        capabilities: vec![],
        features: cfg.qualified_runtimes[0].features.clone(),
    };
    cfg.qualified(&request).unwrap();
    request.runtime = artifact("arbitrary-worker-claim");
    assert!(cfg.qualified(&request).is_err());
    request.runtime = cfg.qualified_runtimes[0].runtime.clone();
    request.features.push(Feature::ServerMaterialV1);
    assert!(cfg.qualified(&request).is_err());
}
