//! Pure test-only configuration validation. This file launches no Graph/Git/AQ
//! process and makes no runtime/provider/model qualification claim.
mod common;
use aq_server::{
    agent_access::AgentAccessConfig,
    project_bindings::domain::{ProjectBinding, ProjectCeilings, Reference},
    source_retention::SourceRetentionConfig,
};
use common::selection::ActualSetup;
use uuid::Uuid;
fn input() -> ActualSetup {
    // Explicit immutable sample catalog bytes are used only for local schema/
    // digest vectors. The actual Codex caller must provide its actual recipe.
    let catalog = aq_profiles::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog"),
    )
    .unwrap();
    let workspace = Uuid::new_v4();
    ActualSetup {
        enrollment_worker_version: "0.4.0".into(),
        project: ProjectBinding {
            schema_version: 1,
            project_id: Uuid::new_v4(),
            name: "Pure configuration vector; no owner contacted".into(),
            enabled: true,
            repository_id: Uuid::new_v4(),
            repository_binding_generation: 1,
            repository_adapter_reference: "aq.git.configured".into(),
            graph_adapter_reference: "aq.graph.configured".into(),
            graph_workspace_id: workspace,
            catalog_digest: catalog.digest().into(),
            allowed_agent_profiles: vec!["implementer@1".into()],
            allowed_intelligence_profiles: vec!["engineering@1".into()],
            allowed_execution_profiles: vec!["confined-development@1".into()],
            allowed_node_ids: vec![],
            allowed_worker_groups: vec!["actual-fixture".into()],
            allowed_environments: vec!["confined-worktree".into()],
            policy: Reference {
                identity: "pure-policy".into(),
                revision: "1".into(),
                digest: "a".repeat(64),
            },
            context: vec![],
            ceilings: ProjectCeilings {
                permissions: vec![],
                context_bytes: 8192,
                wall_time_ms: 20_000,
                memory_bytes: 268435456,
                cpu_millis: 20_000,
                process_limit: 8,
                cost_limit_microunits: None,
                currency: None,
                require_hard_cost_enforcement: false,
            },
        },
        catalog_artifact_json: String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap(),
        graph: AgentAccessConfig {
            graph_url: "http://127.0.0.1:49152".into(),
            graph_token_file: "not-opened-by-pure-validation".into(),
            mutation_actor_id: Some("actual-caller-actor".into()),
            credential_ttl_ms: 10_000,
        },
        task: aq_admission::TaskRevision {
            workspace_id: workspace,
            task_id: Uuid::new_v4(),
            version: 1,
            content_revision: 1,
        },
        source: SourceRetentionConfig {
            staging_root: "not-created-staging".into(),
            git_root: "not-opened-git".into(),
            git_socket: "not-opened-socket".into(),
            max_staging_bytes: 1_048_576,
            max_bundle_bytes: 1_048_576,
            bulk_concurrency: 1,
            incomplete_ttl_secs: 60,
        },
    }
}
#[test]
fn explicit_canonical_catalog_scope_is_validated_without_owner_effects() {
    let input = input();
    let catalog = input.validate().unwrap();
    assert_eq!(catalog.digest(), input.project.catalog_digest);
    assert_eq!(input.task.version, 1);
    assert_eq!(input.task.content_revision, 1);
}
#[test]
fn actual_setup_refuses_mismatched_scope_digest_and_noncanonical_artifact() {
    let input = input();
    let mut changed = input.clone();
    changed.project.graph_workspace_id = Uuid::new_v4();
    assert!(changed.validate().is_err());
    let mut changed = input.clone();
    changed.project.catalog_digest = "0".repeat(64);
    assert!(changed.validate().is_err());
    let mut changed = input.clone();
    changed.catalog_artifact_json.push('\n');
    assert!(changed.validate().is_err());
    let mut changed = input;
    changed.task.version = 0;
    assert!(changed.validate().is_err());
}
#[test]
fn actual_setup_has_no_implicit_global_or_remote_source_fallback() {
    let input = input();
    for origin in [
        "http://localhost:49152",
        "https://127.0.0.1:49152",
        "http://127.0.0.1:49152/path",
        "http://127.0.0.1:49152/?scope=other",
    ] {
        let mut changed = input.clone();
        changed.graph.graph_url = origin.into();
        assert!(changed.validate().is_err());
    }
    let mut changed = input.clone();
    changed.graph.mutation_actor_id = None;
    assert!(changed.validate().is_err());
    let mut changed = input;
    changed.project.project_id = Uuid::from_u128(1);
    assert!(changed.validate().is_err());
}
