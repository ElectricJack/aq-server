//! Shared synthetic source ownership fixture; no real provider/Git qualification.
use super::profiled::{Fixture, NOW, fixture, insert_typed};
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, ObjectFormat, SourceAdmission, SourceSelection, Text,
};
use aq_server::{
    profiled_execution::{
        artifacts,
        config::{ProfiledConfig, ProjectPolicy, QualifiedRuntime},
        policies,
        source_exports::{self, Request},
    },
    source_retention::{Runtime, SourceRetentionConfig},
};
use std::sync::Arc;
use uuid::Uuid;
pub async fn setup() -> (Fixture, Request, tempfile::TempDir) {
    let mut f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let source = f.profiled.source.clone();
    let project = f.selection.lineage.project_id;
    let handle = Uuid::new_v4();
    let runtime_ref = super::profiled::artifact("synthetic-source-reader", b"runtime");
    let qualification =
        super::profiled::artifact("synthetic-source-reader-qualification", b"qualification");
    let features = vec![aq_admission::transport::Feature::ArtifactTransferV1];
    let config = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: runtime_ref.clone(),
            qualification,
            capabilities: vec![],
            features: features.clone(),
            material_delivery: None,
        }],
        context_qualifications: vec![],
        projects: vec![ProjectPolicy {
            project_id: project,
            enabled: true,
            priority: 0,
            source: SourceSelection::Immutable {
                object: aq_admission::ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: source.base.hex.clone(),
                },
            },
            retention_handle: handle,
            placement: None,
        }],
    };
    let policy = config.project_policy_record(project).unwrap();
    let mut cfg = (*f.state.config).clone();
    cfg.profiled_execution = Some(Arc::new(config));
    f.state.config = Arc::new(cfg);
    let max_bundle = 65536;
    f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            git_socket: dir.path().join("absent.sock"),
            git_root: dir.path().join("git"),
            staging_root: dir.path().join("stage"),
            max_bundle_bytes: max_bundle,
            max_staging_bytes: max_bundle + 1_048_576 + 4096,
            bulk_concurrency: 1,
            incomplete_ttl_secs: 60,
        })
        .unwrap(),
    ));
    let admission = SourceAdmission {
        schema_version: 1,
        project_id: project,
        project_revision: 1,
        repository_id: source.repository_id,
        repository_binding_generation: source.binding_generation,
        policy: policy.artifact().unwrap(),
        selection: policy.source,
        resolved_object: aq_admission::ObjectId {
            format: ObjectFormat::Sha1,
            hex: source.base.hex.clone(),
        },
        observed_at_ms: NOW - 10,
        expires_at_ms: NOW + 29990,
    };
    let mut value = serde_json::json!({"schema_version":1,"admission":admission,"retention_handle":handle,"evidence":{"commit_inspection":{"oid":{"algorithm":"sha1","hex":source.base.hex},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}},"retention_inspection":{"version":1,"id":handle,"repository_id":source.repository_id,"binding_generation":source.binding_generation,"generation":1,"state":"active","refs":[{"name":"refs/aq-git/checkpoints/synthetic","oid":source.base.hex}],"release_operation":null,"release_reason":null}}});
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).unwrap();
    let artifact = ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            Digest::of(&bytes).as_str()
        ))
        .unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&bytes),
    };
    let spec = typed_spec(&mut f, &admission, &artifact);
    policies::install(&f.state).await.unwrap();
    use tower::ServiceExt;
    let registration = aq_admission::transport::Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: aq_admission::transport::WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: runtime_ref,
        capabilities: vec![],
        features,
    };
    let response = aq_server::build_worker_router(f.state.clone())
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/worker/profiled/register")
                .header("authorization", format!("Bearer {}", f.credential))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::to_vec(&aq_admission::transport::TransportDocument::Register(
                        registration,
                    ))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success());
    insert_typed(&f, &spec).await;
    policies::install(&f.state).await.unwrap();
    artifacts::retain(
        &f.state,
        artifacts::Retain {
            artifact_id: Uuid::new_v4(),
            project_id: project,
            owner: artifacts::OwnerKind::SourceRetention,
            artifact: artifact.clone(),
            kind: ArtifactKind::Context,
            bytes: &bytes,
            retain_until_ms: NOW + 600000,
        },
    )
    .await
    .unwrap();
    let request = Request {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        project_id: project,
        project_revision: 1,
        repository_id: source.repository_id,
        logical_identity: source.logical_identity.clone(),
        binding_generation: source.binding_generation,
        base: source.base.clone(),
        source_retention_handle: handle,
        source_observation: artifact,
    };
    (f, request, dir)
}
pub fn typed_spec(
    f: &mut Fixture,
    admission: &SourceAdmission,
    artifact: &ArtifactRef,
) -> aq_admission::execution_spec::ProfiledExecutionSpec {
    let task_context =
        super::profiled::artifact("synthetic-source-owner-task-context", b"synthetic task");
    f.profiled
        .offer
        .manifest
        .context
        .references
        .push(aq_session_protocol::ContextRef {
            artifact: task_context.clone(),
            provenance: Text::new("synthetic-source-owner-component").unwrap(),
            delivery: aq_session_protocol::ContextDelivery::Startup,
            bytes: 14,
            tokens: 4,
            required: true,
        });
    f.profiled.offer.manifest.bindings.source = artifact.clone();
    let selection_digest = aq_selection::digest(&f.selection).unwrap();
    f.profiled.offer.manifest.selection_explanation = ArtifactRef {
        id: Text::new("sealed-selection").unwrap(),
        version: Text::new("1").unwrap(),
        digest: selection_digest.clone(),
    };
    f.profiled.offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&f.profiled.offer.manifest).unwrap();
    let spec = aq_admission::execution_spec::ProfiledExecutionSpec {
        schema_version: 1,
        lifecycle: aq_admission::transport::lifecycle_binding(&f.profiled.offer).unwrap(),
        project_revision: 1,
        task: aq_admission::TaskRevision {
            workspace_id: f.selection.lineage.workspace_id,
            task_id: f.selection.lineage.task_id,
            version: f.profiled.offer.manifest.bindings.task_revision,
            content_revision: 1,
        },
        source: admission.clone(),
        source_artifact: artifact.clone(),
        outcome_artifact: f.profiled.offer.manifest.bindings.outcome.clone(),
        graph_access_profile: f
            .profiled
            .offer
            .manifest
            .bindings
            .graph_access_profile
            .clone(),
        task_observation: super::profiled::artifact("synthetic-task-observation", b"graph-context"),
        task_context,
        selection_digest,
    };
    f.profiled.offer.binding.execution_spec_digest = spec.artifact().unwrap().digest;
    spec
}
pub fn synthetic_fact(
    token: &source_exports::ReservedExport,
) -> aq_server::source_retention::profiled_base::VerifiedExport {
    synthetic_fact_at(token, NOW)
}
pub fn synthetic_fact_at(
    token: &source_exports::ReservedExport,
    now: u64,
) -> aq_server::source_retention::profiled_base::VerifiedExport {
    let r = token.request();
    let producer = Uuid::new_v4();
    let bundle = b"synthetic component bundle";
    let manifest = serde_json::json!({"format_version":1,"repository_id":r.repository_id,"repository_identity":r.logical_identity,"binding_generation":r.binding_generation,"transfer_id":r.operation_id,"object_format":"sha1","base":r.base,"source":r.base,"prerequisites":[],"bundle_size":bundle.len(),"bundle_sha256":Digest::of(bundle),"producer_installation_id":producer,"reference_only":false,"caller_correlation":{"owner":"aq.profiled-base-v1","project_id":r.project_id,"project_revision":r.project_revision,"source_observation":r.source_observation}});
    let raw = serde_json::to_string(&manifest).unwrap();
    let retention = serde_json::json!({"version":1,"id":r.operation_id,"repository_id":r.repository_id,"binding_generation":r.binding_generation,"generation":1,"resource":"transfer","refs":[{"name":format!("refs/aq-git/transfers/{}",r.operation_id),"oid":r.base.hex}],"state":"active","dependencies":[],"release_operation":null,"release_reason":null});
    let mut value = serde_json::json!({"schema_version":1,"request_digest":token.request_digest(),"export_operation_id":r.operation_id,"manifest_json":raw,"manifest_digest":Digest::of(raw.as_bytes()),"bundle_bytes":bundle.len(),"bundle_digest":Digest::of(bundle),"producer_installation_id":producer,"export_response":{"artifact_handle":r.operation_id,"retention_handle":r.operation_id,"manifest":manifest},"retention_inspection":retention,"observed_at_ms":now,"expires_at_ms":now+30000});
    value.sort_all_objects();
    let raw = serde_json::to_vec(&value).unwrap();
    aq_server::source_retention::profiled_base::VerifiedExport::decode_stored(token, &raw).unwrap()
}
