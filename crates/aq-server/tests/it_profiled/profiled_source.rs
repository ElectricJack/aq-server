//! Fixed public Git reader with a synthetic Unix peer; no retained source grant.
use crate::common;
use aq_admission::{Label, SourceSelection};
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
use std::sync::{Arc, atomic::Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
// With `delivery_playbook` a named ref never reads `repo.observation` or the
// configured handle (spec 0016 §4); the test below covers that path.
#[tokio::test]
async fn fixed_public_source_reads_preserve_original_age_and_refuse_released_retention() {
    use aq_admission::Digest;
    use common::profiled::{NOW, fixture, insert};
    use std::sync::atomic::AtomicBool;
    let mut f = fixture().await;
    insert(&f).await;
    let frozen = frozen_binding(&f.db.pool().await, &f).await;
    let handle = Uuid::new_v4();
    let policy = configure_main(&mut f, handle);
    let (_dir, _socket, listener) = install_git(&mut f);
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
    let observed = source::observe(&f.state, &frozen, &policy, false)
        .await
        .unwrap();
    assert_eq!(observed.admission.observed_at_ms, NOW - 100);
    assert_eq!(observed.admission.expires_at_ms, NOW - 100 + 30000);
    assert_eq!(observed.artifact.digest, Digest::of(&observed.bytes));
    assert_eq!(observed.retention_handle, handle);
    assert_eq!(requests.lock().await.len(), 3);
    released.store(true, Ordering::SeqCst);
    assert!(
        source::observe(&f.state, &frozen, &policy, false)
            .await
            .is_err()
    );
    task.abort();
}

/// Spec 0016 §4 (feature `delivery_playbook`): a project whose source is
/// `NamedRef refs/heads/main` follows the remote's `main` through heads
/// central aq-git synced and retained (`repo.retain_ref`), with no config
/// change. The synthetic aq-git peer holds the fixture remote's `main`.
#[cfg(feature = "delivery_playbook")]
#[tokio::test]
async fn named_main_follows_remote_pushes_through_synced_retained_heads() {
    use aq_delivery::main_sync::{self, Scope};
    use common::profiled::{NOW, fixture, insert};
    let mut f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let project_id = f.selection.lineage.project_id;
    let frozen = frozen_binding(&pool, &f).await;
    // The configured handle is never what a followed `main` starts from.
    let configured = Uuid::new_v4();
    let policy = configure_main(&mut f, configured);
    let (_dir, socket, listener) = install_git(&mut f);
    // The fixture remote's `main`, and the heads aq-git retained by handle.
    let (a, c) = ("a".repeat(40), "c".repeat(40));
    let repository = frozen.project.binding.repository_id;
    let peer = MovingMain::serve(listener, repository, a.clone(), f.time.clone());
    let time = f.time.clone();
    let clock = move || time.load(Ordering::SeqCst) as u64;
    let scope = Scope::new(project_id, repository, 1, "refs/heads/main");

    // The background pass syncs and records the remote's main.
    let projects = [(project_id, "refs/heads/main".to_owned())];
    assert_eq!(
        main_sync::sync_bound(&pool, &socket, &projects, clock.clone())
            .await
            .unwrap(),
        1
    );
    let first = main_sync::latest(&pool, &scope).await.unwrap().unwrap();
    assert_synced_head(&first, &a, NOW);

    // An offer within the freshness window starts from that record.
    let observed = source::observe(&f.state, &frozen, &policy, true)
        .await
        .unwrap();
    assert_eq!(observed.admission.resolved_object.hex, a);
    assert_eq!(observed.retention_handle, first.retention_handle);
    assert_eq!(peer.syncs(), 1, "a fresh record needs no inline sync");
    let body: Value = serde_json::from_slice(&observed.bytes).unwrap();
    assert_eq!(body["evidence"]["main_sync"]["head"], a);
    assert!(body["evidence"].get("reference_observation").is_none());
    source::validate_body(&observed.bytes, project_id, NOW).unwrap();

    // A commit is pushed to the remote's main. Settings make the record
    // stale, so the next offer syncs inline and starts from the new head,
    // with the same configuration.
    peer.push_after_window(&pool, &f, c.clone()).await;
    let moved = source::observe(&f.state, &frozen, &policy, true)
        .await
        .unwrap();
    assert_eq!(
        peer.syncs(),
        2,
        "a stale record syncs inline before offering"
    );
    assert_eq!(moved.admission.resolved_object.hex, c);
    assert_eq!(moved.admission.observed_at_ms, NOW + 1500);
    assert_ne!(moved.retention_handle, first.retention_handle);
    assert_ne!(moved.retention_handle, configured);
    assert_eq!(moved.admission.policy, observed.admission.policy);
    source::validate_body(&moved.bytes, project_id, NOW + 1500).unwrap();
    let latest = main_sync::latest(&pool, &scope).await.unwrap().unwrap();
    assert_synced_head(&latest, &c, NOW + 1500);
    assert_eq!(latest.retention_handle, moved.retention_handle);

    check_handles(&pool, &scope, configured, [&first, &latest]).await;
    let mut forged: Value = serde_json::from_slice(&moved.bytes).unwrap();
    forged["evidence"]["main_sync"]["retention_handle"] = json!(first.retention_handle);
    let forged = serde_json::to_vec(&forged).unwrap();
    assert!(source::validate_body(&forged, project_id, NOW + 1500).is_err());

    // While fresh again, the next offer reuses the new record.
    let again = source::observe(&f.state, &frozen, &policy, true)
        .await
        .unwrap();
    assert_eq!(peer.syncs(), 2);
    assert_eq!(again.retention_handle, moved.retention_handle);
    peer.task.abort();
}
async fn frozen_binding(
    pool: &sqlx::PgPool,
    f: &common::profiled::Fixture,
) -> FrozenProjectBinding {
    let row = sqlx::query(
        "SELECT binding,binding_observation FROM project_execution_bindings WHERE project_id=$1",
    )
    .bind(f.selection.lineage.project_id)
    .fetch_one(pool)
    .await
    .unwrap();
    let node_id = f.node.node_id;
    use common::profiled::NOW;
    let validation: aq_server::project_bindings::domain::BindingObservation =
        serde_json::from_value(row.get::<Value, _>("binding_observation")).unwrap();
    FrozenProjectBinding {
        project: ProjectRecord {
            revision: 1,
            binding: serde_json::from_value(row.get("binding")).unwrap(),
            validated_at_ms: NOW,
            validation: validation.clone(),
        },
        snapshot_digest: "synthetic-snapshot".into(),
        worker_membership_digest: "synthetic-membership".into(),
        binding_observation: validation,
        node_id,
        environment: "confined-worktree".into(),
        role: "developer".into(),
        intelligence_profile: "standard-medium".into(),
        execution_profile: "sandbox".into(),
    }
}
fn configure_main(
    f: &mut common::profiled::Fixture,
    handle: Uuid,
) -> aq_server::profiled_execution::config::ProjectPolicyRecord {
    let project_id = f.selection.lineage.project_id;
    let config = ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![ProjectPolicy {
            project_id,
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
    let policy = config.project_policy_record(project_id).unwrap();
    let mut state_config = (*f.state.config).clone();
    state_config.profiled_execution = Some(Arc::new(config));
    f.state.config = Arc::new(state_config);
    policy
}
fn install_git(
    f: &mut common::profiled::Fixture,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    tokio::net::UnixListener,
) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("git.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    f.state.source_retention = Some(Arc::new(
        Runtime::new(SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            git_socket: socket.clone(),
            git_root: dir.path().join("git"),
            staging_root: dir.path().join("stage"),
            max_staging_bytes: 1024 * 1024,
            max_bundle_bytes: 65536,
            bulk_concurrency: 1,
            incomplete_ttl_secs: 60,
            git_deadline_ms: 30_000,
        })
        .unwrap(),
    ));
    (dir, socket, listener)
}
#[cfg(feature = "delivery_playbook")]
struct MovingMain {
    remote: Arc<std::sync::Mutex<String>>,
    operations: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
#[cfg(feature = "delivery_playbook")]
impl MovingMain {
    fn serve(
        listener: tokio::net::UnixListener,
        repository: Uuid,
        initial: String,
        peer_time: Arc<std::sync::atomic::AtomicI64>,
    ) -> Self {
        use std::collections::BTreeMap;
        let remote = Arc::new(std::sync::Mutex::new(initial));
        let retained = Arc::new(std::sync::Mutex::new(BTreeMap::<Uuid, String>::new()));
        let operations = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let (peer_remote, peer_retained, peer_operations) =
            (remote.clone(), retained, operations.clone());
        let peer = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["repository_id"], repository.to_string());
                assert_eq!(request["expected_binding_generation"], 1);
                let operation = request["operation"].as_str().unwrap().to_owned();
                let result = match operation.as_str() {
                    "repo.retain_ref" => {
                        assert_eq!(request["params"], json!({"ref":"refs/heads/main"}));
                        assert_eq!(request["request_id"], request["operation_id"]);
                        let head = peer_remote.lock().unwrap().clone();
                        let mut handles = peer_retained.lock().unwrap();
                        // aq-git reuses the handle while the head is unchanged.
                        let handle = handles
                            .iter()
                            .find(|(_, h)| **h == head)
                            .map(|(id, _)| *id)
                            .unwrap_or_else(Uuid::new_v4);
                        handles.insert(handle, head.clone());
                        json!({"ref":"refs/heads/main","head":{"algorithm":"sha1","hex":head},
                        "retention_handle":handle,"synced_at_ms":peer_time.load(Ordering::SeqCst)})
                    }
                    "inspect" => {
                        let oid = request["params"]["oid"].clone();
                        json!({"oid":oid,"tree":{"algorithm":"sha1","hex":"b".repeat(40)}})
                    }
                    "retention.inspect" => {
                        let handle: Uuid =
                            serde_json::from_value(request["params"]["handle_id"].clone()).unwrap();
                        let head = peer_retained.lock().unwrap().get(&handle).cloned().unwrap();
                        json!({"version":1,"id":handle,"repository_id":repository,"binding_generation":1,
                        "generation":1,"state":"active",
                        "refs":[{"name":format!("refs/aq-git/retained/{handle}"),"oid":head}],
                        "release_operation":null,"release_reason":null})
                    }
                    other => panic!("unexpected public operation {other}"),
                };
                peer_operations.lock().unwrap().push(operation);
                let mut reply = serde_json::to_vec(&json!({"protocol_version":1,
                "request_id":request["request_id"],"operation_id":request["operation_id"],
                "ok":true,"result":result}))
                .unwrap();
                reply.push(b'\n');
                reader.get_mut().write_all(&reply).await.unwrap();
            }
        });
        Self {
            remote,
            operations,
            task: peer,
        }
    }
    async fn push_after_window(
        &self,
        pool: &sqlx::PgPool,
        f: &common::profiled::Fixture,
        head: String,
    ) {
        use common::profiled::NOW;
        *self.remote.lock().unwrap() = head;
        let limits = aq_role_settings::delivery::DeliveryLimits {
            main_freshness_ms: 1000,
            ..Default::default()
        };
        aq_role_settings::save_delivery_limits(
            aq_role_settings::Writer::new(pool, f.actor, NOW as i64).edit(0, Uuid::new_v4()),
            &limits,
        )
        .await
        .unwrap();
        f.time.store((NOW + 1500) as i64, Ordering::SeqCst);
    }
    fn syncs(&self) -> usize {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|o| *o == "repo.retain_ref")
            .count()
    }
}
#[cfg(feature = "delivery_playbook")]
async fn check_handles(
    pool: &sqlx::PgPool,
    scope: &aq_delivery::main_sync::Scope,
    configured: Uuid,
    heads: [&aq_delivery::main_sync::MainSync; 2],
) {
    use aq_delivery::main_sync;
    let [first, latest] = heads;
    // Handle verification accepts a synced handle for exactly its head.
    for (handle, head, accepted) in [
        (latest.retention_handle, &latest.head, true),
        (first.retention_handle, &first.head, true),
        (latest.retention_handle, &first.head, false),
        (first.retention_handle, &latest.head, false),
        (configured, &latest.head, false),
    ] {
        let verdict = main_sync::accepts(pool, scope, handle, head).await;
        assert_eq!(verdict.unwrap(), accepted, "{handle} for {head}");
    }
}
#[cfg(feature = "delivery_playbook")]
fn assert_synced_head(sync: &aq_delivery::main_sync::MainSync, expected: &str, at: u64) {
    assert_eq!((sync.head.as_str(), sync.synced_at_ms), (expected, at));
}
