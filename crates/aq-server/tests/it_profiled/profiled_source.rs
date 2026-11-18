//! Fixed public Git reader with a synthetic Unix peer; no retained source grant.
use crate::common;
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
        snapshot_digest: "synthetic-snapshot".into(),
        worker_membership_digest: "synthetic-membership".into(),
        binding_observation: validation,
        node_id: f.node.node_id,
        environment: "confined-worktree".into(),
        role: "developer".into(),
        intelligence_profile: "standard-medium".into(),
        execution_profile: "sandbox".into(),
    };
    let handle = Uuid::new_v4();
    let config = ProfiledConfig {
        definition_bodies: vec![],
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
            agent_grants: None,
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

/// Spec 0016 §4: a named ref follows what `repo.retain_ref` last synced from
/// the remote. A commit pushed to the remote's `main` is offered with no
/// config change; a stale record is synced inline; the synced handle is the
/// one retention is verified against, and source export accepts it.
#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn named_ref_follows_synced_main_without_a_config_change() {
    use aq_server::profiled_execution::main_sync::{self, Limits, Scope};
    use common::profiled::{NOW, fixture, insert};
    use std::collections::HashMap;
    let mut f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let row = sqlx::query(
        "SELECT binding,binding_observation FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(f.selection.lineage.project_id)
    .fetch_one(&pool)
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
        snapshot_digest: "synthetic-snapshot".into(),
        worker_membership_digest: "synthetic-membership".into(),
        binding_observation: validation,
        node_id: f.node.node_id,
        environment: "confined-worktree".into(),
        role: "developer".into(),
        intelligence_profile: "standard-medium".into(),
        execution_profile: "sandbox".into(),
    };
    let project = f.selection.lineage.project_id;
    // The configured handle is never used: the ref moves by sync alone.
    let configured_handle = Uuid::new_v4();
    let config = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![ProjectPolicy {
            project_id: project,
            enabled: true,
            priority: 0,
            source: SourceSelection::NamedRef {
                reference: Label::new("refs/heads/main").unwrap(),
            },
            retention_handle: configured_handle,
            placement: None,
            agent_grants: None,
        }],
    };
    let policy = config.project_policy_record(project).unwrap();
    let limits = Limits {
        main_sync_interval_ms: 60_000,
        main_freshness_ms: 5_000,
        main_sync_deadline_ms: 5_000,
    };
    let mut state_config = (*f.state.config).clone();
    state_config.profiled_execution = Some(Arc::new(config));
    state_config.delivery_limits = Some(limits.clone());
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
    // The fixture remote: `main` points at `remote`; aq-git retains each
    // synced head under one handle and reuses it while the head is unchanged.
    let (first, second) = ("a".repeat(40), "c".repeat(40));
    let remote = Arc::new(std::sync::Mutex::new(first.clone()));
    let handles = Arc::new(std::sync::Mutex::new(HashMap::<String, Uuid>::new()));
    let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (peer_remote, peer_handles, peer_calls) = (remote.clone(), handles.clone(), calls.clone());
    let time = f.time.clone();
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
            let operation = request["operation"].as_str().unwrap().to_owned();
            peer_calls.lock().unwrap().push(operation.clone());
            let result = match operation.as_str() {
                "repo.retain_ref" => {
                    assert_eq!(request["params"], json!({"ref":"refs/heads/main"}));
                    assert!(request["operation_id"].as_str().is_some());
                    let head = peer_remote.lock().unwrap().clone();
                    let handle = *peer_handles
                        .lock()
                        .unwrap()
                        .entry(head.clone())
                        .or_insert_with(Uuid::new_v4);
                    json!({"ref":"refs/heads/main","head":{"algorithm":"sha1","hex":head},
                        "retention_handle":handle,"synced_at_ms":time.load(Ordering::SeqCst)})
                }
                "inspect" => {
                    let hex = request["params"]["oid"]["hex"].clone();
                    json!({"oid":{"algorithm":"sha1","hex":hex},"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
                }
                "retention.inspect" => {
                    let id = request["params"]["handle_id"].as_str().unwrap();
                    let head = peer_handles
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(_, h)| h.to_string() == id)
                        .map(|(head, _)| head.clone())
                        .expect("only a synced handle is inspected");
                    json!({"version":1,"id":id,"repository_id":repository,"binding_generation":1,"generation":1,"state":"active",
                        "refs":[{"name":format!("refs/aq-git/retained/{id}"),"oid":head}],"release_operation":null,"release_reason":null})
                }
                other => panic!("unexpected aq-git operation {other}"),
            };
            let mut reply = serde_json::to_vec(
                &json!({"protocol_version":1,"request_id":request["request_id"],
                "operation_id":request.get("operation_id"),"ok":true,"result":result}),
            )
            .unwrap();
            reply.push(b'\n');
            reader.get_mut().write_all(&reply).await.unwrap();
        }
    });
    let retain_calls = || {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "repo.retain_ref")
            .count()
    };
    let scope = Scope {
        project_id: project,
        repository_id: repository,
        binding_generation: 1,
        reference: "refs/heads/main",
    };

    // The background pass syncs and records the project's main.
    assert_eq!(main_sync::tick(&f.state, &limits).await, 1);
    let synced = main_sync::latest(&f.state, scope).await.unwrap().unwrap();
    assert_eq!(
        (synced.head.as_str(), synced.synced_at_ms),
        (first.as_str(), NOW)
    );
    let first_handle = synced.retention_handle;

    // A fresh record is offered as is, with no inline sync and no
    // `repo.observation`.
    f.time.store((NOW + 1_000) as i64, Ordering::SeqCst);
    let observed = source::observe(&f.state, &frozen, &policy).await.unwrap();
    assert_eq!(retain_calls(), 1);
    assert_eq!(observed.admission.resolved_object.hex, first);
    assert_eq!(observed.retention_handle, first_handle);
    assert_eq!(observed.admission.observed_at_ms, NOW);
    source::validate_body(&observed.bytes, project, NOW + 1_000).unwrap();

    // A commit is pushed to the remote's main. Once the record is older than
    // `main_freshness_ms`, the offer syncs inline and offers the new head,
    // retained under the new handle, with the same configuration.
    *remote.lock().unwrap() = second.clone();
    f.time.store((NOW + 7_000) as i64, Ordering::SeqCst);
    let moved = source::observe(&f.state, &frozen, &policy).await.unwrap();
    assert_eq!(retain_calls(), 2);
    assert_eq!(moved.admission.resolved_object.hex, second);
    let second_handle = handles.lock().unwrap()[&second];
    assert_ne!(second_handle, first_handle);
    assert_ne!(second_handle, configured_handle);
    assert_eq!(moved.retention_handle, second_handle);
    assert_eq!(moved.admission.observed_at_ms, NOW + 7_000);
    let body: Value = serde_json::from_slice(&moved.bytes).unwrap();
    assert_eq!(body["evidence"]["main_sync"]["head"], json!(second));
    assert!(body["evidence"].get("reference_observation").is_none());
    assert!(body["evidence"].get("publication_head").is_none());
    source::validate_body(&moved.bytes, project, NOW + 7_000).unwrap();

    // Handle verification accepts a synced handle only for its own head; the
    // older handle still verifies the head it retains.
    assert!(
        main_sync::handle_for(&pool, scope, second_handle, &second)
            .await
            .unwrap()
    );
    assert!(
        main_sync::handle_for(&pool, scope, first_handle, &first)
            .await
            .unwrap()
    );
    assert!(
        !main_sync::handle_for(&pool, scope, first_handle, &second)
            .await
            .unwrap()
    );
    assert!(
        !main_sync::handle_for(&pool, scope, configured_handle, &second)
            .await
            .unwrap()
    );

    // An unchanged head reuses its handle and adds no record.
    assert_eq!(main_sync::tick(&f.state, &limits).await, 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM main_sync_heads WHERE project_id=$1")
        .bind(project)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 2);
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "repo.observation")
    );
    task.abort();
}
