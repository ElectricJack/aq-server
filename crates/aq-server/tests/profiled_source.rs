//! Fixed public Git reader with a synthetic Unix peer; no retained source grant.
mod common;
use aq_admission::{Digest, Label, SourceSelection};
use aq_server::{
    profiled_execution::{
        config::{ProfiledConfig, ProjectPolicy},
        source,
    },
    project_bindings::domain::{FrozenProjectBinding, ProjectRecord},
    source_retention::{Runtime, SourceRetentionConfig},
};
use serde_json::{Value, json};
use sqlx::Row;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
#[tokio::test]
async fn fixed_public_source_reads_preserve_original_age_and_refuse_released_retention() {
    use common::profiled::{NOW, fixture, insert};
    let mut f = fixture().await;
    insert(&f).await;
    let row = sqlx::query(
        "SELECT binding,binding_observation FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(f.selection.lineage.project_id)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    let validation: aq_server::project_bindings::domain::BindingObservation =
        serde_json::from_value(row.get::<Value, _>("binding_observation")).unwrap();
    let frozen = FrozenProjectBinding {
        project: ProjectRecord {
            revision: 1,
            binding: serde_json::from_value(row.get("binding")).unwrap(),
            validated_at_ms: NOW,
            validation: validation.clone(),
        },
        catalog_activation_revision: 1,
        worker_membership_digest: "synthetic-membership".into(),
        binding_observation: validation,
        node_id: f.node.node_id,
        environment: "confined-worktree".into(),
        agent_profile: "implementer@2".into(),
        intelligence_profile: "engineering@2".into(),
        execution_profile: "confined-development@2".into(),
    };
    let handle = Uuid::new_v4();
    let config = ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![ProjectPolicy {
            project_id: f.selection.lineage.project_id,
            enabled: true,
            priority: 0,
            source: SourceSelection::NamedRef {
                reference: Label::new("refs/heads/main").unwrap(),
            },
            retention_handle: handle,
            placement: None,
        }],
    };
    let policy = config
        .project_policy_record(f.selection.lineage.project_id)
        .unwrap();
    let mut state_config = (*f.state.config).clone();
    state_config.profiled_execution = Some(Arc::new(config));
    f.state.config = Arc::new(state_config);
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("git.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            git_socket: socket,
            git_root: dir.path().join("git"),
            staging_root: dir.path().join("stage"),
            max_staging_bytes: 1024 * 1024,
            max_bundle_bytes: 65536,
            bulk_concurrency: 1,
            incomplete_ttl_secs: 60,
        })
        .unwrap(),
    ));
    let released = Arc::new(AtomicBool::new(false));
    let peer_released = released.clone();
    let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let peer_requests = requests.clone();
    let repository = frozen.project.binding.repository_id;
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["repository_id"], repository.to_string());
            assert_eq!(request["expected_binding_generation"], 1);
            let result = match request["operation"].as_str().unwrap() {
                "repo.observation" => {
                    assert_eq!(
                        request["params"],
                        json!({"refs":["refs/heads/main"],"max_age_ms":30000})
                    );
                    json!({"observation_id":Uuid::new_v4(),"binding_generation":1,"started_at_ms":NOW-100,"completed_at_ms":NOW-50,"refs":{"refs/heads/main":"a".repeat(40)},"covered_refs":["refs/heads/main"],"last_refresh_error":null})
                }
                "inspect" => {
                    assert_eq!(
                        request["params"],
                        json!({"oid":{"algorithm":"sha1","hex":"a".repeat(40)}})
                    );
                    json!({"oid":{"algorithm":"sha1","hex":"a".repeat(40)},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
                }
                "retention.inspect" => {
                    assert_eq!(request["params"], json!({"handle_id":handle,"limit":1}));
                    json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":1,"generation":1,"state":if peer_released.load(Ordering::SeqCst){"released"}else{"active"},"refs":[{"name":"refs/aq-git/checkpoints/test","oid":"a".repeat(40)}],"release_operation":null,"release_reason":null})
                }
                _ => panic!("unexpected public operation"),
            };
            peer_requests.lock().await.push(request.clone());
            let mut reply=serde_json::to_vec(&json!({"protocol_version":1,"request_id":request["request_id"],"ok":true,"result":result})).unwrap();
            reply.push(b'\n');
            reader.get_mut().write_all(&reply).await.unwrap();
        }
    });
    let observed = source::observe(&f.state, &frozen, &policy).await.unwrap();
    assert_eq!(observed.admission.observed_at_ms, NOW - 100);
    assert_eq!(observed.admission.expires_at_ms, NOW - 100 + 30000);
    assert_eq!(observed.artifact.digest, Digest::of(&observed.bytes));
    assert_eq!(observed.retention_handle, handle);
    assert_eq!(requests.lock().await.len(), 3);
    released.store(true, Ordering::SeqCst);
    assert!(source::observe(&f.state, &frozen, &policy).await.is_err());
    task.abort();
}
