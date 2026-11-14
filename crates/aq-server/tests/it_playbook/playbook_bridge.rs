//! Explicit candidate0023 schema component tests on disposable PostgreSQL.
//! No normal migration registration, native credential issuance, production
//! mount or provider qualification is claimed by these synthetic known bindings.
use crate::common;
use aq_playbook_host::runner::{CancellationResult, Intent, IntentStatus};
use aq_server::{
    playbooks::{domain::*, service::*},
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use common::profiled::{Fixture, NOW, fixture, insert};
use serde_json::json;
use sqlx::Row;
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use uuid::Uuid;
struct Env {
    f: Fixture,
    service: Service,
    grant: Uuid,
    runtime: RuntimeOwner,
    recipient: Recipient,
    attempt: Uuid,
}
async fn setup() -> Env {
    setup_with(false).await
}
async fn setup_with(graph_events: bool) -> Env {
    let f = fixture().await;
    let attempt = insert(&f).await;
    let pool = f.db.pool().await;
    let registered: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.playbook_namespaces')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    if aq_server::STATE_FORMAT_VERSION >= 23 {
        // Ordered integration must install the real migration. Never repair a
        // missing registered table or silently accept a migration gap here.
        assert!(registered.is_some(), "registered023 tables missing");
    } else {
        // Isolated candidate profile only: explicit table install, no meta bump.
        assert!(
            registered.is_none(),
            "unexpected candidate tables before install"
        );
        let before: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::raw_sql(include_str!("../../migrations/0023_playbook_bridge.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let after: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            before, after,
            "candidate fixture changed registered version"
        );
    }
    let p = &f.profiled.offer.binding;
    let aq_session_protocol::SessionIdentity::Remote {
        session_id,
        assignment_generation: generation,
        ..
    } = p.session
    else {
        panic!("remote fixture")
    };
    let binding = json!({"workspace_id":f.selection.lineage.workspace_id,"project_id":f.selection.lineage.project_id,"task_id":f.selection.lineage.task_id,"session_id":session_id,"attempt_id":attempt,"generation":generation});
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')").bind(attempt).bind(f.node.node_id).bind(binding).execute(&pool).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Keyring::load(&ProtectionConfig {
        active_key_id: "test".into(),
        keys: vec![KeyFile {
            id: "test".into(),
            file: path,
        }],
    })
    .unwrap();
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: format!("fixture-{}", Uuid::new_v4()),
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        owner_user_id: f.actor,
    };
    let service = if graph_events {
        Service::new_profile3(&f.state, namespace, Arc::new(keys)).await
    } else {
        Service::new(&f.state, namespace, Arc::new(keys)).await
    }
    .unwrap();
    service.initialize(f.actor).await.unwrap();
    let recipient = Recipient::Task {
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        task_id: f.selection.lineage.task_id,
    };
    let grant = Uuid::new_v4();
    service
        .grant(
            f.actor,
            grant,
            GrantScope {
                workspace_id: f.selection.lineage.workspace_id,
                project_id: f.selection.lineage.project_id,
                recipients: vec![recipient.clone()],
                capabilities: BTreeSet::from(["message.send".into()]),
            },
            NOW as i64 + 10_000,
        )
        .await
        .unwrap();
    let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
    Env {
        f,
        service,
        grant,
        runtime,
        recipient,
        attempt,
    }
}
fn intent(e: &Env, id: &str) -> Intent {
    let input = json!(MessageInput {
        recipient: e.recipient.clone(),
        thread_id: Uuid::from_u128(42),
        body: "synthetic bounded mailbox content".into()
    });
    Intent {
        id: id.into(),
        run: "synthetic-run".into(),
        step: "send".into(),
        invocation: 1,
        deadline_at_ms: NOW as i64 + 5000,
        command: message_contract(),
        input_digest: digest(&input).unwrap(),
        input,
        authority: authority(e.grant, 1),
        status: IntentStatus::Dispatched,
        cancel_requested: false,
        cancel_dispatched: false,
        handle: None,
        result: None,
        reason: None,
        late_results: vec![],
        completion_event: None,
        dispatch_sequence: None,
    }
}
async fn count(e: &Env, table: &str) -> i64 {
    assert!(
        [
            "mailbox_messages",
            "playbook_effects",
            "playbook_outbox",
            "playbook_cancellations"
        ]
        .contains(&table)
    );
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&e.f.db.pool().await)
        .await
        .unwrap()
}
#[tokio::test]
async fn mailbox_and_outbox_commit_once_and_reconcile_after_revocation() {
    let e = setup().await;
    let r = intent(&e, "one");
    let MessageResult::Queued(first) = e
        .service
        .send_message(&e.runtime, e.grant, 1, &r)
        .await
        .unwrap()
    else {
        panic!("queued")
    };
    assert_eq!(first["disposition"], "queued");
    assert_eq!(first["harness_delivery"], "not_attempted");
    e.service.revoke(e.f.actor, e.grant).await.unwrap();
    e.f.time
        .store((NOW + 20_000) as i64, std::sync::atomic::Ordering::SeqCst);
    let Some(MessageResult::Queued(replayed)) = e.service.reconcile(e.grant, 1, &r).await.unwrap()
    else {
        panic!("historical receipt")
    };
    assert_eq!(first, replayed);
    let MessageResult::Queued(replayed) = e
        .service
        .send_message(&e.runtime, e.grant, 1, &r)
        .await
        .unwrap()
    else {
        panic!("historical exact replay")
    };
    assert_eq!(first, replayed);
    for table in ["mailbox_messages", "playbook_effects", "playbook_outbox"] {
        assert_eq!(count(&e, table).await, 1);
    }
    let mut changed = r.clone();
    changed.input["body"] = json!("changed immutable bytes");
    changed.input_digest = digest(&changed.input).unwrap();
    assert_eq!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &changed)
            .await
            .unwrap_err(),
        Error::Conflict
    );
    assert_eq!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &intent(&e, "new-after-revoke"))
            .await
            .unwrap_err(),
        Error::Forbidden
    );
    let page = e.service.outbox(&e.runtime, 0, 16).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].2.payload["message_id"], first["message_id"]);
}
#[tokio::test]
async fn cancel_before_owner_admission_prevents_effect_and_after_queue_is_too_late() {
    let e = setup().await;
    let before = intent(&e, "before");
    assert!(matches!(
        e.service
            .cancel(&e.runtime, e.grant, 1, &before)
            .await
            .unwrap(),
        CancellationResult::Confirmed
    ));
    assert!(matches!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &before)
            .await
            .unwrap(),
        MessageResult::Cancelled
    ));
    assert_eq!(count(&e, "mailbox_messages").await, 0);
    assert!(matches!(
        e.service.reconcile(e.grant, 1, &before).await.unwrap(),
        Some(MessageResult::Cancelled)
    ));
    let queued = intent(&e, "after");
    e.service
        .send_message(&e.runtime, e.grant, 1, &queued)
        .await
        .unwrap();
    assert!(matches!(
        e.service
            .cancel(&e.runtime, e.grant, 1, &queued)
            .await
            .unwrap(),
        CancellationResult::TooLate
    ));
    assert_eq!(count(&e, "mailbox_messages").await, 1);
    let mut changed = before;
    changed.input["body"] = json!("changed cancellation bytes");
    changed.input_digest = digest(&changed.input).unwrap();
    assert_eq!(
        e.service
            .cancel(&e.runtime, e.grant, 1, &changed)
            .await
            .unwrap_err(),
        Error::Conflict
    );
}
#[tokio::test]
async fn runtime_adoption_fences_old_sender_and_delayed_old_initializer() {
    let e = setup().await;
    let second_id = Uuid::new_v4();
    let second = e.service.adopt_runtime(second_id, 1).await.unwrap();
    assert_eq!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &intent(&e, "old"))
            .await
            .unwrap_err(),
        Error::Fenced
    );
    assert_eq!(
        e.service
            .adopt_runtime(Uuid::new_v4(), 0)
            .await
            .unwrap_err(),
        Error::Fenced
    );
    e.service
        .send_message(&second, e.grant, 1, &intent(&e, "current"))
        .await
        .unwrap();
    e.service.retire_runtime(&second).await.unwrap();
    assert_eq!(
        e.service.authorize(&second, e.grant, 1).await.unwrap_err(),
        Error::Fenced
    );
    assert_eq!(count(&e, "mailbox_messages").await, 1);
}
#[tokio::test]
async fn expiry_after_mailbox_lock_wait_rolls_back_new_message_and_outbox() {
    let e = setup().await;
    e.service
        .send_message(&e.runtime, e.grant, 1, &intent(&e, "head"))
        .await
        .unwrap();
    let pool = e.f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT mailbox_key FROM mailbox_heads FOR UPDATE")
        .fetch_all(&mut *held)
        .await
        .unwrap();
    let service = e.service.clone();
    let owner = e.runtime.clone();
    let grant = e.grant;
    let r = intent(&e, "waiter");
    let task = tokio::spawn(async move { service.send_message(&owner, grant, 1, &r).await });
    tokio::time::timeout(Duration::from_secs(2),async{loop {let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-playbook-bridge' AND wait_event_type='Lock')").fetch_one(&pool).await.unwrap();if waiting{break}tokio::task::yield_now().await;}}).await.unwrap();
    e.f.time
        .store((NOW + 20_000) as i64, std::sync::atomic::Ordering::SeqCst);
    held.commit().await.unwrap();
    assert_eq!(task.await.unwrap().unwrap_err(), Error::Forbidden);
    for table in ["mailbox_messages", "playbook_effects", "playbook_outbox"] {
        assert_eq!(count(&e, table).await, 1);
    }
}
#[tokio::test]
async fn protected_admission_conflicts_and_engine_receipt_ack_are_separate() {
    let e = setup().await;
    let r = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: e.grant,
        grant_revision: 1,
        binding: "pending-policy".into(),
        expected_activation_revision: 3,
        entry: "main".into(),
        input: json!({"secret":"synthetic input stored protected"}),
    };
    let first = e.service.prepare(e.f.actor, r.clone()).await.unwrap();
    assert_eq!(first.status, "pending");
    assert!(first.receipt.is_none());
    let replay = e.service.prepare(e.f.actor, r.clone()).await.unwrap();
    assert_eq!(replay.engine_key, first.engine_key);
    let mut changed = r.clone();
    changed.input = json!("changed");
    assert_eq!(
        e.service.prepare(e.f.actor, changed).await.unwrap_err(),
        Error::Conflict
    );
    let retained = e.service.admission(e.f.actor, r.request_id).await.unwrap();
    assert_eq!(retained.status.engine_key, first.engine_key);
    let row = sqlx::query(
        "SELECT request_digest,nonce,ciphertext FROM playbook_admissions WHERE request_id=$1",
    )
    .bind(r.request_id)
    .fetch_one(&e.f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(row.get::<Vec<u8>, _>("nonce").len(), 12);
    assert!(!row.get::<Vec<u8>, _>("ciphertext").is_empty());
    let receipt = aq_playbook_host::runner::KeyedAdmissionReceipt {
        key: first.engine_key,
        request_digest: row.get("request_digest"),
        run_id: "synthetic-receipt-validation-fixture".into(),
        activation_revision: 3,
        catalog_revision: 7,
        accepted_at_ms: NOW as i64,
    };
    let ack = e
        .service
        .acknowledge_admission(&e.runtime, r.request_id, receipt.clone())
        .await
        .unwrap();
    assert_eq!(ack.status, "engine_accepted");
    e.service
        .acknowledge_admission(&e.runtime, r.request_id, receipt)
        .await
        .unwrap();
    assert!(
        e.service
            .pending_admissions(&e.runtime, 16)
            .await
            .unwrap()
            .is_empty()
    );
}

fn source_url(e: &Env) -> String {
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    // Disposable loopback test PostgreSQL only (scripts/test-db or CI's localhost).
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    url.set_path(e.f.db.options().get_database().unwrap());
    url.to_string()
}
fn protected_factory(
    url: String,
) -> impl FnOnce() -> std::result::Result<
    aq_playbook_host::postgres_storage::PostgresStorage,
    aq_playbook_host::Error,
> + Send
+ 'static {
    move || {
        let mut config: aq_playbook_host::postgres_client::Config =
            url.parse().map_err(|_| aq_playbook_host::Error::Invalid)?;
        config.connect_timeout(Duration::from_secs(2));
        let client = config
            .connect(aq_playbook_host::postgres_client::NoTls)
            .map_err(|_| aq_playbook_host::Error::Unknown)?;
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "fixture",
            std::collections::BTreeMap::from([("fixture".into(), [11_u8; 32])]),
        )
        .unwrap();
        aq_playbook_host::protected::storage(
            client,
            keys,
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
    }
}
fn pure_artifact() -> aq_playbook_host::model::Artifact {
    use aq_playbook_host::model::*;
    use std::collections::BTreeMap;
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "synthetic AQ13 pure admission fixture".into(),
        source: SourceBinding {
            digest: hash_bytes(b"synthetic immutable pure source"),
            header: SourceHeader {
                format_version: 1,
                id: "pure".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "pure".into(),
            purpose: "atomic admission proof, no host effects".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "done".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "done".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(serde_json::Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
#[tokio::test]
async fn actual_engine_commit_lost_aq_ack_recovers_same_run_after_owner_restart_and_revocation() {
    use aq_playbook_host::{
        Owner, Request,
        runner::{KeyedAdmissionDisposition, KeyedAdmissionRequest},
    };
    let e = setup().await;
    let engine_namespace = e.service.namespace.engine_namespace();
    // Simulate the driver process boundary directly: engine commit succeeds, then
    // its response is deliberately not ACKed in AQ. No synthetic receipt is used.
    let owner = Owner::start(
        engine_namespace.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let a = pure_artifact();
    let hash = a.digest.clone();
    owner
        .call(
            Request::Install {
                artifact: Box::new(a),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Activate {
                binding: "pure".into(),
                artifact: hash,
                authority: authority(e.grant, 1),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let binding = owner
        .call(
            Request::Binding {
                binding: "pure".into(),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    let prepare = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: e.grant,
        grant_revision: 1,
        binding: "pure".into(),
        expected_activation_revision: binding["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    let pending = e.service.prepare(e.f.actor, prepare.clone()).await.unwrap();
    let committed: KeyedAdmissionDisposition = serde_json::from_value(
        owner
            .call(
                Request::AdmitOnce {
                    request: Box::new(KeyedAdmissionRequest {
                        key: pending.engine_key,
                        binding: "pure".into(),
                        expected_activation_revision: prepare.expected_activation_revision,
                        entry: "main".into(),
                        input: serde_json::Value::Null,
                        authority: authority(e.grant, 1),
                    }),
                    requested_at_ms: Some(NOW as i64),
                    now_ms: NOW as i64,
                },
                Duration::from_secs(5),
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(!committed.duplicate);
    assert_eq!(
        e.service
            .admission(e.f.actor, prepare.request_id)
            .await
            .unwrap()
            .status
            .status,
        "pending"
    );
    owner
        .call(Request::Stop, Duration::from_secs(5))
        .await
        .unwrap();
    drop(owner);
    e.service.revoke(e.f.actor, e.grant).await.unwrap();
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let recovered = driver
        .admission(e.f.actor, prepare.request_id, true)
        .await
        .unwrap();
    assert_eq!(recovered.receipt.unwrap(), committed.receipt);
    driver.stop().await.unwrap();
}

#[tokio::test]
async fn actual_protected_engine_host_queues_one_message_and_replays_fixed_event_outbox() {
    use aq_playbook_host::model::*;
    use std::collections::BTreeMap;
    let e = setup().await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let mut a = pure_artifact();
    let command = message_contract();
    a.content.registry = registry();
    a.content.playbook.required_capabilities = BTreeSet::from(["message.send".into()]);
    a.content.playbook.entries.get_mut("main").unwrap().start = "send".into();
    let input = json!(MessageInput {
        recipient: e.recipient.clone(),
        thread_id: Uuid::new_v4(),
        body: "actual AQ11 owner service via qualified protected engine".into()
    });
    a.content.playbook.steps.insert(
        "send".into(),
        Step {
            op: Operation::Invoke {
                command: ContractRef {
                    name: command.name.clone(),
                    fingerprint: command.fingerprint().unwrap(),
                },
                input: Expr::literal(input),
                bind: "sent".into(),
                retry: None,
            },
            edges: BTreeMap::from([("queued".into(), "done".into())]),
            on_error: Some("done".into()),
        },
    );
    let a = Artifact::seal(a.content).unwrap();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let binding = driver
        .activate("mail-policy".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let request = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: e.grant,
        grant_revision: 1,
        binding: "mail-policy".into(),
        expected_activation_revision: binding["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    e.service.prepare(e.f.actor, request.clone()).await.unwrap();
    let admitted = driver
        .admission(e.f.actor, request.request_id, false)
        .await
        .unwrap();
    assert!(admitted.receipt.is_some());
    driver.quantum().await.unwrap(); // message committed; new outbox has not yet been ACKed
    let first = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(!first.duplicate);
    let replay = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(first.key, replay.key);
    for _ in 0..4 {
        driver.quantum().await.unwrap();
    }
    for table in ["mailbox_messages", "playbook_effects", "playbook_outbox"] {
        assert_eq!(count(&e, table).await, 1);
    }
    let row = sqlx::query("SELECT status,fanout_complete,engine_receipt FROM playbook_outbox")
        .fetch_one(&e.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "engine_accepted");
    assert!(row.get::<bool, _>("fanout_complete"));
    assert_eq!(
        row.get::<serde_json::Value, _>("engine_receipt")["destinations"],
        0
    );
    // Repeated driver quanta are execution of the admitted policy, never new roots
    // or new mailbox operations after an already accepted command result.
    driver.quantum().await.unwrap();
    assert_eq!(count(&e, "mailbox_messages").await, 1);
    driver.stop().await.unwrap();
}

async fn api(
    app: axum::Router,
    method: &str,
    path: &str,
    csrf: bool,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if csrf {
        request = request.header("x-aq-request", "1");
    }
    let response = app
        .oneshot(
            request
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn optional_http_mount_requires_owner_csrf_and_only_finite_controls_survive_maintenance() {
    use aq_server::{build_admin_router, build_admin_router_with_playbooks, build_worker_router};
    let e = setup().await;
    let driver = Arc::new(
        aq_server::playbooks::driver::Driver::start(
            e.service.clone(),
            aq_playbook_host::Limits::default(),
            protected_factory(source_url(&e)),
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        api(
            build_admin_router(e.f.state.clone()),
            "GET",
            "/api/playbooks/status",
            false,
            json!(null)
        )
        .await
        .0,
        404
    );
    assert_eq!(
        api(
            build_worker_router(e.f.state.clone()),
            "GET",
            "/api/playbooks/status",
            false,
            json!(null)
        )
        .await
        .0,
        404
    );
    let artifact = pure_artifact();
    let hash = artifact.digest.clone();
    driver.install(artifact).await.unwrap();
    let binding = driver
        .activate("pure".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let r = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: e.grant,
        grant_revision: 1,
        binding: "pure".into(),
        expected_activation_revision: binding["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    e.service.prepare(e.f.actor, r.clone()).await.unwrap();
    let accepted = driver
        .admission(e.f.actor, r.request_id, false)
        .await
        .unwrap();
    let run = accepted.receipt.unwrap().run_id;
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("status")).unwrap();
    std::fs::write(
        directory.path().join("status/maintenance"),
        "synthetic maintenance",
    )
    .unwrap();
    let mut config = (*e.f.state.config).clone();
    config.updates = Some(aq_server::config::UpdatesConfig {
        spool_dir: directory.path().to_path_buf(),
    });
    let mut state = e.f.state.clone();
    state.config = Arc::new(config);
    let app = build_admin_router_with_playbooks(state, Some(driver.clone()));
    assert_eq!(
        api(
            app.clone(),
            "GET",
            "/api/playbooks/status",
            false,
            json!(null)
        )
        .await
        .0,
        200
    );
    for path in [
        "/api/playbooks/drive",
        "/api/playbooks/recover",
        "/api/playbooks/admissions",
        "/api/playbooks/grants",
        "/api/playbooks/grants/not-a-uuid/revoke",
        "/api/playbooks/runs/not-a-run/cancel",
    ] {
        let (status, body) = api(app.clone(), "POST", path, true, json!({})).await;
        assert_eq!(status, 503, "{path}: {body}");
        assert_eq!(body["error"], "maintenance");
    }
    let revoke = format!("/api/playbooks/grants/{}/revoke", e.grant);
    assert_eq!(
        api(app.clone(), "POST", &revoke, false, json!({})).await.0,
        403
    );
    let cancel = format!("/api/playbooks/runs/{run}/cancel");
    assert_eq!(
        api(app.clone(), "POST", &cancel, true, json!({})).await.0,
        200
    );
    assert_eq!(
        api(app.clone(), "POST", &revoke, true, json!({})).await.0,
        200
    );
    let pool = e.f.db.pool().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(e.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        api(
            app.clone(),
            "GET",
            "/api/playbooks/status",
            false,
            json!(null)
        )
        .await
        .0,
        403
    );
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(e.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        api(app.clone(), "POST", "/api/playbooks/stop", true, json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        api(app, "POST", "/api/playbooks/stop", true, json!({}))
            .await
            .0,
        200
    );
    assert!(matches!(
        aq_server::admission::classify(&cancel),
        aq_server::admission::Class::Control
    ));
    assert!(matches!(
        aq_server::admission::classify("/api/playbooks/drive"),
        aq_server::admission::Class::General
    ));
}
#[tokio::test(flavor = "current_thread")]
async fn actual_http_renew_cancel_progress_while_real_mail_host_exhausts_bridge_work_permits() {
    use aq_playbook_host::model::*;
    use aq_worker_protocol::*;
    use std::collections::BTreeMap;
    let e = setup().await;
    e.service
        .send_message(&e.runtime, e.grant, 1, &intent(&e, "existing-head"))
        .await
        .unwrap();
    let driver = Arc::new(
        aq_server::playbooks::driver::Driver::start(
            e.service.clone(),
            aq_playbook_host::Limits::default(),
            protected_factory(source_url(&e)),
        )
        .await
        .unwrap(),
    );
    let mut artifact = pure_artifact();
    let command = message_contract();
    artifact.content.registry = registry();
    artifact.content.playbook.required_capabilities = BTreeSet::from(["message.send".into()]);
    artifact
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .start = "send".into();
    artifact.content.playbook.steps.insert(
        "send".into(),
        Step {
            op: Operation::Invoke {
                command: ContractRef {
                    name: command.name.clone(),
                    fingerprint: command.fingerprint().unwrap(),
                },
                input: Expr::literal(json!(MessageInput {
                    recipient: e.recipient.clone(),
                    thread_id: Uuid::new_v4(),
                    body: "held actual owner-service dispatch".into()
                })),
                bind: "sent".into(),
                retry: None,
            },
            edges: BTreeMap::from([("queued".into(), "done".into())]),
            on_error: Some("done".into()),
        },
    );
    let artifact = Artifact::seal(artifact.content).unwrap();
    let hash = artifact.digest.clone();
    driver.install(artifact).await.unwrap();
    let activation = driver
        .activate("held-mail".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let r = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: e.grant,
        grant_revision: 1,
        binding: "held-mail".into(),
        expected_activation_revision: activation["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    let spec = ExecutionSpec {
        bindings: None,
        project_id: Uuid::new_v4(),
        harness: FAKE_HARNESS_CAPABILITY.into(),
        resolved_spec_digest: String::new(),
        fixture_output: "synthetic control fixture, not native launch".into(),
        resources: Resources {
            slots: 1,
            cpu_millis: 100,
            memory_bytes: 1024,
            disk_bytes: 1024,
        },
        required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into()],
    }
    .seal();
    let offer = Offer {
        offer_id: Uuid::new_v4(),
        identity: e.f.node.clone(),
        attempt: AttemptRef {
            attempt_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            generation: 1,
        },
        spec,
        offer_expires_at_ms: NOW + 30_000,
        reservation_ttl_ms: 30_000,
    };
    e.f.state
        .store
        .execution_offer(Uuid::new_v4(), &offer)
        .await
        .unwrap();
    e.f.state
        .store
        .execution_accept(
            &AcceptRequest {
                identity: e.f.node.clone(),
                operation_id: Uuid::new_v4(),
                offer_id: offer.offer_id,
                attempt: offer.attempt.clone(),
                resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
                reservation_id: Uuid::new_v4(),
                reservation_deadline_ms: 500_000,
                reservation_remaining_ms: 20_000,
            },
            NOW,
        )
        .await
        .unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = admin.local_addr().unwrap();
    let w = worker.local_addr().unwrap();
    let ar = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    let wr = aq_server::build_worker_router(e.f.state.clone());
    let aserver = tokio::spawn(async move {
        axum::serve(
            admin,
            ar.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let wserver = tokio::spawn(async move {
        axum::serve(
            worker,
            wr.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let response = client
        .post(format!("http://{a}/api/playbooks/admissions"))
        .header("x-aq-request", "1")
        .json(&r)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let pool = e.f.db.pool().await;
    let mut held = pool.begin().await.unwrap();
    sqlx::query("SELECT mailbox_key FROM mailbox_heads FOR UPDATE")
        .fetch_all(&mut *held)
        .await
        .unwrap();
    let c = client.clone();
    let driving = tokio::spawn(async move {
        c.post(format!("http://{a}/api/playbooks/drive"))
            .header("x-aq-request", "1")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
    });
    async fn wait_locks(pool: &sqlx::PgPool, count: i64) {
        tokio::time::timeout(Duration::from_secs(2),async{loop {let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-playbook-bridge' AND wait_event_type='Lock'").fetch_one(pool).await.unwrap();if waiting>=count {break}tokio::task::yield_now().await;}}).await.unwrap();
    }
    wait_locks(&pool, 1).await;
    let c = client.clone();
    let waiting = tokio::spawn(async move {
        c.get(format!(
            "http://{a}/api/playbooks/admissions/{}",
            r.request_id
        ))
        .send()
        .await
        .unwrap()
    });
    wait_locks(&pool, 2).await;
    assert_eq!(
        client
            .get(format!("http://{a}/api/playbooks/status"))
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    let renewed = client
        .post(format!("http://{w}/v2/worker/renew"))
        .bearer_auth(&e.f.credential)
        .json(&RenewRequest {
            identity: e.f.node.clone(),
            operation_id: Uuid::new_v4(),
            attempt: offer.attempt.clone(),
            grant_sequence: 1,
        })
        .send()
        .await
        .unwrap();
    let status = renewed.status();
    let body = renewed.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let cancelled=client.post(format!("http://{a}/api/execution/attempts/{}/cancel",offer.attempt.attempt_id)).header("x-aq-request","1").json(&json!({"operation_id":Uuid::new_v4(),"reason":"control remains available during real policy Host hold"})).send().await.unwrap();
    let status = cancelled.status();
    let body = cancelled.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    held.commit().await.unwrap();
    assert_eq!(driving.await.unwrap().status(), 200);
    assert_eq!(waiting.await.unwrap().status(), 200);
    driver.stop().await.unwrap();
    aserver.abort();
    wserver.abort();
}

#[tokio::test]
async fn expired_admission_page_is_held_without_starving_later_current_grant() {
    let e = setup().await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let a = pure_artifact();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let old = driver
        .activate("expired".into(), hash.clone(), e.grant, 1)
        .await
        .unwrap();
    let live_grant = Uuid::new_v4();
    e.service
        .grant(
            e.f.actor,
            live_grant,
            GrantScope {
                workspace_id: e.service.namespace.workspace_id,
                project_id: e.service.namespace.project_id,
                recipients: vec![e.recipient.clone()],
                capabilities: BTreeSet::from(["message.send".into()]),
            },
            NOW as i64 + 25_000,
        )
        .await
        .unwrap();
    let live = driver
        .activate("live".into(), hash, live_grant, 1)
        .await
        .unwrap();
    for id in [1, 2] {
        e.service
            .prepare(
                e.f.actor,
                PrepareRequest {
                    request_id: Uuid::from_u128(id),
                    grant_id: e.grant,
                    grant_revision: 1,
                    binding: "expired".into(),
                    expected_activation_revision: old["revision"].as_u64().unwrap(),
                    entry: "main".into(),
                    input: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
    }
    let id = Uuid::from_u128(3);
    e.service
        .prepare(
            e.f.actor,
            PrepareRequest {
                request_id: id,
                grant_id: live_grant,
                grant_revision: 1,
                binding: "live".into(),
                expected_activation_revision: live["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    e.f.time
        .store((NOW + 15_000) as i64, std::sync::atomic::Ordering::SeqCst);
    driver.quantum().await.unwrap();
    driver.quantum().await.unwrap();
    assert_eq!(
        e.service
            .admission(e.f.actor, Uuid::from_u128(1))
            .await
            .unwrap()
            .status
            .status,
        "unknown"
    );
    assert_eq!(
        e.service
            .admission(e.f.actor, Uuid::from_u128(2))
            .await
            .unwrap()
            .status
            .status,
        "unknown"
    );
    assert_eq!(
        e.service
            .admission(e.f.actor, id)
            .await
            .unwrap()
            .status
            .status,
        "engine_accepted"
    );
    driver.stop().await.unwrap();
}

/// D3: a confirmed graph operation reaches the engine as one typed event.
/// The engine commit without AQ ACK is the crash cut; replay dedups on the
/// stable (producer, epoch, event_id) key and `delivered` is set only after
/// durable engine acceptance.
#[tokio::test]
async fn outbox_replay_delivers_each_event_once() {
    let e = setup_with(true).await;
    let pool = e.f.db.pool().await;
    let (operation, event) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,retained_bytes) VALUES($1,$2,$3,'{}','d3','{}','{}','{}','confirmed',0,0)")
        .bind(operation).bind(e.attempt).bind(e.f.node.node_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO agent_graph_outbox(event_id,operation_id,kind,payload) VALUES($1,$2,'graph.operation_confirmed','{}')")
        .bind(event).bind(operation).execute(&pool).await.unwrap();
    let delivered = || async {
        sqlx::query_scalar::<_, bool>("SELECT delivered FROM agent_graph_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&e.f.db.pool().await)
            .await
            .unwrap()
    };
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    // The engine admits only events an installed artifact declares; installing
    // the consumer is what enables collection.
    driver.quantum().await.unwrap();
    assert_eq!(count(&e, "playbook_outbox").await, 0);
    let mut a = pure_artifact();
    a.content.registry =
        aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    driver
        .install(aq_playbook_host::model::Artifact::seal(a.content).unwrap())
        .await
        .unwrap();
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 0);
    let first = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(!first.duplicate);
    assert!(
        !delivered().await,
        "engine commit without AQ ACK is not delivery"
    );
    let replay = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(first.key, replay.key);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert!(delivered().await);
    assert_eq!(count(&e, "playbook_outbox").await, 1);
    let row = sqlx::query("SELECT producer,event_id,status,fanout_complete FROM playbook_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("producer"), "aq.graph.operations@1");
    assert_eq!(row.get::<Uuid, _>("event_id"), event);
    assert_eq!(row.get::<String, _>("status"), "engine_accepted");
    assert!(row.get::<bool, _>("fanout_complete"));
    driver.quantum().await.unwrap();
    assert_eq!(count(&e, "playbook_outbox").await, 1);
    driver.stop().await.unwrap();
}

/// Older than the engine's default windows (24 h for AQ events, 7 days for
/// keyed admissions) at the fixture clock.
const EIGHT_DAYS: i64 = 8 * 86_400_000;
/// One confirmed graph operation with its pending graph outbox row.
async fn graph_event(e: &Env) -> Uuid {
    let pool = e.f.db.pool().await;
    let (operation, event) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,retained_bytes) VALUES($1,$2,$3,'{}','d3','{}','{}','{}','confirmed',0,0)")
        .bind(operation).bind(e.attempt).bind(e.f.node.node_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO agent_graph_outbox(event_id,operation_id,kind,payload) VALUES($1,$2,'graph.operation_confirmed','{}')")
        .bind(event).bind(operation).execute(&pool).await.unwrap();
    event
}
async fn outbox_status(e: &Env, event: Uuid) -> (String, bool) {
    sqlx::query_as("SELECT status,fanout_complete FROM playbook_outbox WHERE event_id=$1")
        .bind(event)
        .fetch_one(&e.f.db.pool().await)
        .await
        .unwrap()
}
/// The server sends no request time: a row is delivered on its first
/// delivery however old it is, and after a forward clock jump has pinned the
/// engine's retention clock.
#[tokio::test]
async fn an_old_event_and_one_after_a_clock_jump_are_accepted_on_first_delivery() {
    let e = setup_with(true).await;
    let pool = e.f.db.pool().await;
    let old = graph_event(&e).await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let mut a = pure_artifact();
    a.content.registry =
        aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    driver
        .install(aq_playbook_host::model::Artifact::seal(a.content).unwrap())
        .await
        .unwrap();
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    sqlx::query("UPDATE playbook_outbox SET created_at_ms=$2 WHERE event_id=$1")
        .bind(old)
        .bind(NOW as i64 - EIGHT_DAYS)
        .execute(&pool)
        .await
        .unwrap();
    driver.quantum().await.unwrap();
    assert_eq!(
        outbox_status(&e, old).await,
        ("engine_accepted".into(), true)
    );
    // One delivery at a host time eight days ahead pins the retention clock.
    let ahead = graph_event(&e).await;
    let clock = &e.f.time;
    clock.store(NOW as i64 + EIGHT_DAYS, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    driver.quantum().await.unwrap();
    assert_eq!(
        outbox_status(&e, ahead).await,
        ("engine_accepted".into(), true)
    );
    clock.store(NOW as i64, std::sync::atomic::Ordering::SeqCst);
    let after = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    driver.quantum().await.unwrap();
    assert_eq!(
        outbox_status(&e, after).await,
        ("engine_accepted".into(), true)
    );
    driver.stop().await.unwrap();
}

/// A graph-event consumer started by every event. A `blocked` one takes the
/// whole event payload as its input but allows 8 payload bytes, so every
/// destination of it is blocked by its input.
fn trigger_artifact(id: &str, blocked: bool) -> aq_playbook_host::model::Artifact {
    use aq_playbook_host::model::*;
    let event = aq_server::playbooks::review_domain::graph_operation_event();
    let mut content = pure_artifact().content;
    content.source.digest = hash_bytes(id.as_bytes());
    content.source.header.id = id.into();
    content.playbook.id = id.into();
    let input = if blocked {
        let entry = content.playbook.entries.get_mut("main").unwrap();
        entry.input = event.payload.clone();
        content.playbook.limits.max_payload_bytes = 8;
        Expr::reference("event")
    } else {
        Expr::literal(serde_json::Value::Null)
    };
    content.playbook.triggers = vec![Trigger {
        id: "confirmed".into(),
        event: ContractRef {
            name: event.name.clone(),
            fingerprint: event.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input,
    }];
    content.registry = aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    Artifact::seal(content).unwrap()
}
async fn finished_runs(e: &Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM aq_playbooks.run_history WHERE kind='runs'")
        .fetch_one(&e.f.db.pool().await)
        .await
        .unwrap()
}
/// An accepted event is never sent to the engine as a new event again. Its
/// destination blocked by its input keeps the row incomplete until the
/// engine forgets the event (here: 257 newer events push it out of the
/// 256-entry repeat set); then the row is complete, no second run starts for
/// it and the drive goes on. About four minutes in a debug build (130 quanta
/// on a protected record of 256 events), so it is run by hand;
/// `an_accepted_event_forgotten_by_its_window_...` covers the same path fast.
#[tokio::test]
#[ignore = "slow: about 4 minutes; run by hand (B1 interim note)"]
async fn a_forgotten_accepted_event_completes_its_row_and_starts_no_second_run() {
    let e = setup_with(true).await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    for (binding, blocked) in [("runs", false), ("blocked", true)] {
        let a = trigger_artifact(binding, blocked);
        let hash = a.digest.clone();
        driver.install(a).await.unwrap();
        driver
            .activate(binding.into(), hash, e.grant, 1)
            .await
            .unwrap();
    }
    let first = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    driver.quantum().await.unwrap();
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), false)
    );
    // Later events no longer reach the blocked consumer, so only the first
    // row stays incomplete (and the newer ones need no second visit).
    let mut quiet = pure_artifact();
    quiet.content.registry =
        aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    let quiet = aq_playbook_host::model::Artifact::seal(quiet.content).unwrap();
    let hash = quiet.digest.clone();
    driver.install(quiet).await.unwrap();
    driver
        .activate("blocked".into(), hash, e.grant, 1)
        .await
        .unwrap();
    const NEWER: i64 = 257;
    for _ in 0..NEWER {
        graph_event(&e).await;
    }
    let mut quanta = 0;
    while !outbox_status(&e, first).await.1 {
        quanta += 1;
        assert!(quanta < 1000, "the first row never completed");
        driver.test_collect_graph_events().await.unwrap();
        driver.quantum().await.unwrap();
    }
    // Every event, the first included, started exactly one run.
    for _ in 0..1000 {
        if finished_runs(&e).await > NEWER {
            break;
        }
        driver.quantum().await.unwrap();
    }
    for _ in 0..8 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(finished_runs(&e).await, NEWER + 1);
    driver.stop().await.unwrap();
}

/// The same path when the engine forgets the accepted event because its 24 h
/// window passed: the row is complete, the first event's runs are not
/// started again, and the drive goes on.
#[tokio::test]
async fn an_accepted_event_forgotten_by_its_window_completes_without_a_second_run() {
    let e = setup_with(true).await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    for (binding, blocked) in [("runs", false), ("blocked", true)] {
        let a = trigger_artifact(binding, blocked);
        let hash = a.digest.clone();
        driver.install(a).await.unwrap();
        driver
            .activate(binding.into(), hash, e.grant, 1)
            .await
            .unwrap();
    }
    let first = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), false)
    );
    assert_eq!(finished_runs(&e).await, 1);
    let day = 86_400_000;
    e.f.time.store(
        NOW as i64 + day + 60_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    for _ in 0..4 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), true)
    );
    let later = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    for _ in 0..4 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        outbox_status(&e, later).await,
        ("engine_accepted".into(), false)
    );
    // The first event's run and the later one's: no second run for the first.
    assert_eq!(finished_runs(&e).await, 2);
    driver.stop().await.unwrap();
}

/// A row that always fails (its stored event no longer matches its digest)
/// is held with its reason; the rows behind it are delivered, quantum after
/// quantum.
#[tokio::test]
async fn a_row_that_always_fails_is_held_and_later_rows_are_delivered() {
    let e = setup_with(true).await;
    let pool = e.f.db.pool().await;
    let broken = graph_event(&e).await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let mut a = pure_artifact();
    a.content.registry =
        aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    driver
        .install(aq_playbook_host::model::Artifact::seal(a.content).unwrap())
        .await
        .unwrap();
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    // Its stored event no longer matches its record (another producer).
    mismatch(&e, broken).await;
    for _ in 0..3 {
        let later = graph_event(&e).await;
        assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
        driver.quantum().await.unwrap();
        driver.quantum().await.unwrap();
        assert_eq!(
            outbox_status(&e, later).await,
            ("engine_accepted".into(), true)
        );
    }
    let held: (String, Option<String>) =
        sqlx::query_as("SELECT status,held_reason FROM playbook_outbox WHERE event_id=$1")
            .bind(broken)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(held, ("unknown".into(), Some("invalid".into())));
    driver.stop().await.unwrap();
}

/// Held work is listed with its reason and can be retried; a retry is a new
/// delivery. Retrying an admission whose outcome is unknown says the work may
/// run again; one already accepted is never admitted again.
#[tokio::test]
async fn held_work_shows_its_reason_and_retries_as_a_new_delivery() {
    let e = setup_with(true).await;
    let pool = e.f.db.pool().await;
    let event = graph_event(&e).await;
    let driver = Arc::new(
        aq_server::playbooks::driver::Driver::start(
            e.service.clone(),
            aq_playbook_host::Limits::default(),
            protected_factory(source_url(&e)),
        )
        .await
        .unwrap(),
    );
    let mut a = pure_artifact();
    a.content.registry =
        aq_server::playbooks::review_domain::CatalogProfile::GraphEvents3.registry();
    let a = aq_playbook_host::model::Artifact::seal(a.content).unwrap();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let binding = driver
        .activate("held".into(), hash, e.grant, 1)
        .await
        .unwrap();
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    let producer: String =
        sqlx::query_scalar("SELECT producer FROM playbook_outbox WHERE event_id=$1")
            .bind(event)
            .fetch_one(&pool)
            .await
            .unwrap();
    mismatch(&e, event).await;
    driver.quantum().await.unwrap();
    let app = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    let (code, status) = api(
        app.clone(),
        "GET",
        "/api/playbooks/status",
        false,
        json!(null),
    )
    .await;
    assert_eq!(code, 200, "{status}");
    assert_eq!(
        status["held_events"][0]["event_id"],
        event.to_string(),
        "{status}"
    );
    assert_eq!(
        status["held_events"][0]["held_reason"], "invalid",
        "{status}"
    );
    set_outbox(&e, event, "producer", &producer).await;
    let retry = format!("/api/playbooks/events/{event}/retry");
    let (code, body) = api(app.clone(), "POST", &retry, true, json!({})).await;
    assert_eq!(code, 200, "{body}");
    driver.quantum().await.unwrap();
    assert_eq!(
        outbox_status(&e, event).await,
        ("engine_accepted".into(), true)
    );
    let (code, _) = api(app.clone(), "POST", &retry, true, json!({})).await;
    assert_eq!(code, 409, "only a held row can be retried");

    let id = Uuid::new_v4();
    e.service
        .prepare(
            e.f.actor,
            PrepareRequest {
                request_id: id,
                grant_id: e.grant,
                grant_revision: 1,
                binding: "held".into(),
                expected_activation_revision: binding["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE playbook_admissions SET status='unknown',held_reason='uncertain' WHERE request_id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (code, page) = api(
        app.clone(),
        "GET",
        "/api/playbooks/inspection/admissions",
        false,
        json!(null),
    )
    .await;
    assert_eq!(code, 200, "{page}");
    let item = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["request_id"] == id.to_string())
        .unwrap();
    assert_eq!(
        (item["status"].as_str(), item["held_reason"].as_str()),
        (Some("unknown"), Some("uncertain"))
    );
    let retry = format!("/api/playbooks/admissions/{id}/retry");
    let (code, first) = api(app.clone(), "POST", &retry, true, json!({})).await;
    assert_eq!(code, 200, "{first}");
    assert_eq!(first["status"], "engine_accepted", "{first}");
    assert!(
        first["warning"]
            .as_str()
            .unwrap()
            .contains("may run the work again"),
        "{first}"
    );
    let (code, again) = api(app.clone(), "POST", &retry, true, json!({})).await;
    assert_eq!(code, 200, "{again}");
    assert_eq!(again["receipt"], first["receipt"]);
    assert!(again.get("warning").is_none(), "{again}");
    driver.stop().await.unwrap();
}

/// A retained admission created long before now is admitted on its first
/// delivery.
#[tokio::test]
async fn an_old_admission_is_admitted_on_first_delivery() {
    let e = setup().await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let a = pure_artifact();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let activation = driver
        .activate("old".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let id = Uuid::from_u128(1);
    e.service
        .prepare(
            e.f.actor,
            PrepareRequest {
                request_id: id,
                grant_id: e.grant,
                grant_revision: 1,
                binding: "old".into(),
                expected_activation_revision: activation["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    sqlx::query("UPDATE playbook_admissions SET created_at_ms=$2 WHERE request_id=$1")
        .bind(id)
        .bind(NOW as i64 - EIGHT_DAYS)
        .execute(&e.f.db.pool().await)
        .await
        .unwrap();
    driver.quantum().await.unwrap();
    let admission = e.service.admission(e.f.actor, id).await.unwrap().status;
    assert_eq!(admission.status, "engine_accepted");
    driver.stop().await.unwrap();
}

/// A finished run leaves the engine State as history. Cancelling it is the
/// no-op it was while finished runs stayed in the State; an unknown run is
/// still not confirmed.
#[tokio::test]
async fn cancelling_a_finished_run_is_confirmed_and_an_unknown_run_is_not() {
    let e = setup().await;
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    let a = pure_artifact();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let activation = driver
        .activate("finished".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let id = Uuid::from_u128(1);
    e.service
        .prepare(
            e.f.actor,
            PrepareRequest {
                request_id: id,
                grant_id: e.grant,
                grant_revision: 1,
                binding: "finished".into(),
                expected_activation_revision: activation["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    driver.quantum().await.unwrap();
    let admission = e.service.admission(e.f.actor, id).await.unwrap().status;
    let run = admission.receipt.unwrap().run_id;
    let view = driver.inspect_run(run.clone()).await.unwrap();
    assert_eq!(view["items"][0]["status"], "completed", "{view}");
    driver.cancel_run(run.clone()).await.unwrap();
    // A run AQ admitted that has also rolled out of history is still finished.
    // (The test bypasses the engine's writer trigger to simulate the trim.)
    let mut trim = e.f.db.pool().await.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *trim)
        .await
        .unwrap();
    sqlx::query("DELETE FROM aq_playbooks.run_history WHERE run_id=$1")
        .bind(&run)
        .execute(&mut *trim)
        .await
        .unwrap();
    trim.commit().await.unwrap();
    driver.cancel_run(run).await.unwrap();
    assert_eq!(
        driver.cancel_run("run-ffffffffffffffff".into()).await,
        Err(Error::Unknown)
    );
    driver.stop().await.unwrap();
}

/// A driver with the graph-event consumers `runs` and `blocked` active.
async fn consumers(e: &Env) -> Arc<aq_server::playbooks::driver::Driver> {
    let driver = aq_server::playbooks::driver::Driver::start(
        e.service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(e)),
    )
    .await
    .unwrap();
    for (binding, blocked) in [("runs", false), ("blocked", true)] {
        let a = trigger_artifact(binding, blocked);
        let hash = a.digest.clone();
        driver.install(a).await.unwrap();
        driver
            .activate(binding.into(), hash, e.grant, 1)
            .await
            .unwrap();
    }
    Arc::new(driver)
}
async fn set_outbox(e: &Env, event: Uuid, column: &str, value: &str) {
    sqlx::query(&format!(
        "UPDATE playbook_outbox SET {column}=$2 WHERE event_id=$1"
    ))
    .bind(event)
    .bind(value)
    .execute(&e.f.db.pool().await)
    .await
    .unwrap();
}
/// The outbox row's stored event no longer matches its record: another
/// producer (the graph source row is marked delivered so it is not collected
/// again under its own producer).
async fn mismatch(e: &Env, event: Uuid) {
    set_outbox(e, event, "producer", "aq.host.bridge@1").await;
    sqlx::query("UPDATE agent_graph_outbox SET delivered=true WHERE event_id=$1")
        .bind(event)
        .execute(&e.f.db.pool().await)
        .await
        .unwrap();
}
/// Retrying a held row the engine had accepted resumes its fan-out: it is
/// never sent as a new event, so no second run starts once the engine has
/// forgotten the event.
#[tokio::test]
async fn retrying_a_held_accepted_event_resumes_its_fanout_without_a_second_run() {
    let e = setup_with(true).await;
    let driver = consumers(&e).await;
    let first = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), false)
    );
    assert_eq!(finished_runs(&e).await, 1);
    let producer: String =
        sqlx::query_scalar("SELECT producer FROM playbook_outbox WHERE event_id=$1")
            .bind(first)
            .fetch_one(&e.f.db.pool().await)
            .await
            .unwrap();
    mismatch(&e, first).await;
    // The outbox cursor wraps first, then reaches the row again.
    for _ in 0..2 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(outbox_status(&e, first).await, ("unknown".into(), false));
    set_outbox(&e, first, "producer", &producer).await;
    let app = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    let retry = format!("/api/playbooks/events/{first}/retry");
    let (code, body) = api(app, "POST", &retry, true, json!({})).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), false)
    );
    e.f.time.store(
        NOW as i64 + 86_400_000 + 60_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    for _ in 0..4 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        outbox_status(&e, first).await,
        ("engine_accepted".into(), true)
    );
    assert_eq!(finished_runs(&e).await, 1);
    driver.stop().await.unwrap();
}

/// The request for one run of the pure playbook, which only a tick advances.
async fn pure_run(e: &Env, driver: &aq_server::playbooks::driver::Driver) -> Uuid {
    let a = pure_artifact();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let activation = driver
        .activate("pure".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let request = PrepareRequest {
        request_id: id,
        grant_id: e.grant,
        grant_revision: 1,
        binding: "pure".into(),
        expected_activation_revision: activation["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: serde_json::Value::Null,
    };
    e.service.prepare(e.f.actor, request).await.unwrap();
    id
}
async fn status_read(
    e: &Env,
    driver: &Arc<aq_server::playbooks::driver::Driver>,
) -> serde_json::Value {
    let app = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    api(app, "GET", "/api/playbooks/status", false, json!(null))
        .await
        .1
}
/// A queued event that cannot be decrypted does not stop the namespace (N1):
/// the quantum fails with a reason naming the row and its key id, the row
/// stays queued, and runs still tick. Fixing the keyring recovers it without
/// a retry; or the owner holds the named row and delivery goes on past it.
#[tokio::test]
async fn an_undecryptable_queued_event_is_named_and_runs_still_tick() {
    let e = setup_with(true).await;
    let driver = consumers(&e).await;
    let event = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    set_outbox(&e, event, "key_id", "missing").await;
    let id = pure_run(&e, &driver).await;
    for _ in 0..3 {
        assert!(driver.quantum().await.is_err());
    }
    let admission = e.service.admission(e.f.actor, id).await.unwrap().status;
    let run = admission.receipt.expect("admitted").run_id;
    let view = driver.inspect_run(run).await.unwrap();
    assert_eq!(view["items"][0]["status"], "completed", "{view}");
    let status = status_read(&e, &driver).await;
    let reason = status["automatic_driver_reason"]
        .as_str()
        .unwrap_or_default();
    let named = format!("cannot decrypt queued event {event} (key id missing)");
    assert!(reason.contains(&named), "{status}");
    assert_eq!(outbox_status(&e, event).await, ("pending".into(), false));
    set_outbox(&e, event, "key_id", "test").await;
    driver.quantum().await.unwrap();
    assert_eq!(outbox_status(&e, event).await.0, "engine_accepted");
    // A row that stays undecryptable: the owner holds it by its id.
    let bad = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    set_outbox(&e, bad, "key_id", "missing").await;
    let later = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    for _ in 0..3 {
        assert!(driver.quantum().await.is_err());
    }
    assert_eq!(outbox_status(&e, later).await.0, "pending");
    let app = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    let hold = format!("/api/playbooks/events/{bad}/hold");
    let (code, body) = api(app, "POST", &hold, true, json!({})).await;
    assert_eq!((code.as_u16(), &body["disposition"]), (200, &json!("held")));
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(outbox_status(&e, bad).await.0, "unknown");
    assert_eq!(outbox_status(&e, later).await.0, "engine_accepted");
    let status = status_read(&e, &driver).await;
    assert_eq!(status["held_events"][0]["event_id"], json!(bad), "{status}");
    assert_eq!(status["held_events"][0]["held_reason"], "invalid");
    assert_eq!(status["automatic_driver_reason"], json!(null), "{status}");
    driver.stop().await.unwrap();
}

/// An engine commit that fails for storage (here: a lock wait past the lock
/// timeout) leaves a never-accepted event and admission pending, not held;
/// the next quantum delivers both.
#[tokio::test]
async fn a_storage_failure_leaves_new_work_pending_and_it_is_delivered_after() {
    let e = setup_with(true).await;
    let driver = consumers(&e).await;
    let a = pure_artifact();
    let hash = a.digest.clone();
    driver.install(a).await.unwrap();
    let activation = driver
        .activate("pure".into(), hash, e.grant, 1)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    e.service
        .prepare(
            e.f.actor,
            PrepareRequest {
                request_id: id,
                grant_id: e.grant,
                grant_revision: 1,
                binding: "pure".into(),
                expected_activation_revision: activation["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let event = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    let pool = e.f.db.pool().await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM aq_playbooks.namespaces FOR UPDATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let _ = driver.quantum().await;
    let held = e.service.admission(e.f.actor, id).await.unwrap().status;
    assert_eq!(held.status, "pending");
    // Its unknown outcome is recorded on the row while it waits.
    let reason = "SELECT held_reason FROM playbook_admissions WHERE request_id=$1";
    let why: Option<String> = (sqlx::query_scalar(reason).bind(id).fetch_one(&pool).await).unwrap();
    assert_eq!(why.as_deref(), Some("uncertain"));
    assert_eq!(outbox_status(&e, event).await, ("pending".into(), false));
    lock.rollback().await.unwrap();
    for _ in 0..2 {
        driver.quantum().await.unwrap();
    }
    let admitted = e.service.admission(e.f.actor, id).await.unwrap().status;
    assert_eq!(admitted.status, "engine_accepted");
    let why: Option<String> = (sqlx::query_scalar(reason).bind(id).fetch_one(&pool).await).unwrap();
    assert_eq!(why, None);
    assert_eq!(outbox_status(&e, event).await.0, "engine_accepted");
    driver.stop().await.unwrap();
}

/// A bad source item (here: a graph operation whose binding has no task id)
/// blocks nothing (N2): it is queued held `invalid` and listed on the status
/// read, later graph events are collected and delivered, and admissions and
/// runs go on.
#[tokio::test]
async fn a_bad_graph_item_is_held_and_later_graph_events_are_collected() {
    let e = setup_with(true).await;
    let driver = consumers(&e).await;
    let good = graph_event(&e).await;
    assert_eq!(driver.test_collect_graph_events().await.unwrap(), 1);
    let pool = e.f.db.pool().await;
    let task = |value: &'static str| {
        sqlx::query("UPDATE agent_launch_bindings SET binding=jsonb_set(binding,'{task_id}',to_jsonb($2::text)) WHERE attempt_id=$1")
            .bind(e.attempt)
            .bind(value)
            .execute(&pool)
    };
    let original: String = sqlx::query_scalar(
        "SELECT binding->>'task_id' FROM agent_launch_bindings WHERE attempt_id=$1",
    )
    .bind(e.attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    task("bad").await.unwrap();
    let bad = graph_event(&e).await;
    let id = pure_run(&e, &driver).await;
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(outbox_status(&e, good).await.0, "engine_accepted");
    assert_eq!(outbox_status(&e, bad).await.0, "unknown");
    let original: &'static str = original.leak();
    task(original).await.unwrap();
    let later = graph_event(&e).await;
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(outbox_status(&e, later).await.0, "engine_accepted");
    let admission = e.service.admission(e.f.actor, id).await.unwrap().status;
    let run = admission.receipt.expect("admitted").run_id;
    let view = driver.inspect_run(run).await.unwrap();
    assert_eq!(view["items"][0]["status"], "completed", "{view}");
    let status = status_read(&e, &driver).await;
    assert_eq!(status["held_events"][0]["event_id"], json!(bad), "{status}");
    assert_eq!(status["held_events"][0]["held_reason"], "invalid");
    // Retrying it holds it again: it never reaches the engine.
    let app = aq_server::build_admin_router_with_playbooks(e.f.state.clone(), Some(driver.clone()));
    let retry = format!("/api/playbooks/events/{bad}/retry");
    assert_eq!(api(app, "POST", &retry, true, json!({})).await.0, 200);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(outbox_status(&e, bad).await.0, "unknown");
    driver.stop().await.unwrap();
}

#[tokio::test]
async fn notification_outbox_replays_frozen_projection_independently_of_engine_ack() {
    use aq_server::notifications::{api, policy::Policy, sources::milestone_events};
    let e = setup_with(true).await;
    let pool = e.f.db.pool().await;
    let scope = aq_lifecycle_api::Scope {
        project_id: e.f.selection.lineage.project_id,
        workspace_id: e.f.selection.lineage.workspace_id,
    };
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}') ON CONFLICT(project_id) DO UPDATE SET binding=project_execution_bindings.binding || EXCLUDED.binding")
        .bind(scope.project_id).bind(json!({"project_id":scope.project_id,"enabled":true,"graph_workspace_id":scope.workspace_id})).execute(&pool).await.unwrap();
    let (operation, source) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,retained_bytes) VALUES($1,$2,$3,'{}','d3','{}','{}','{}','confirmed',0,0)")
        .bind(operation).bind(e.attempt).bind(e.f.node.node_id).execute(&pool).await.unwrap();
    // Projection fixture stands for an already validated Graph ready response.
    // Mark engine delivery first: notification fanout must still consume it.
    let events = milestone_events(
        source,
        scope,
        &[json!({"id":Uuid::new_v4(),"labels":["aq:milestone-gate"]})],
    )
    .unwrap();
    sqlx::query("INSERT INTO agent_graph_outbox(event_id,operation_id,kind,payload,delivered,notification_events) VALUES($1,$2,'graph.operation_confirmed','{}',TRUE,$3)")
        .bind(source).bind(operation).bind(json!(events)).execute(&pool).await.unwrap();
    let service = api::service(&e.f.state);
    service.install(Uuid::new_v4()).await.unwrap();
    let policy = Policy::new(service);
    assert_eq!(policy.reconcile_graph(&e.f.state).await.unwrap(), 1);
    assert_eq!(policy.reconcile_graph(&e.f.state).await.unwrap(), 0);
    // Crash after policy commit but before source ACK: exact events replay.
    sqlx::query("UPDATE agent_graph_outbox SET notification_observed=FALSE WHERE event_id=$1")
        .bind(source)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(policy.reconcile_graph(&e.f.state).await.unwrap(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notification_messages")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    let row = sqlx::query(
        "SELECT delivered,notification_observed FROM agent_graph_outbox WHERE event_id=$1",
    )
    .bind(source)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(row.get::<bool, _>("delivered"));
    assert!(row.get::<bool, _>("notification_observed"));
}

// ------------------------------------------------- rolling records (B1b)

const ZERO12: &str = "decode(repeat('00',12),'hex')";
const ZERO16: &str = "decode(repeat('00',16),'hex')";

async fn exec(pool: &sqlx::PgPool, sql: &str, e: &Env, grant: Uuid, n: i64) {
    sqlx::query(sql)
        .bind(e.service.namespace.namespace_id)
        .bind(e.service.namespace.generation)
        .bind(grant)
        .bind("a".repeat(64))
        .bind(n)
        .execute(pool)
        .await
        .unwrap();
}

/// `n` finished rows in every bridge table, named `done-1` (oldest) to
/// `done-n` where a table has a text key. `grant` backs the admissions.
async fn seed_finished(e: &Env, grant: Uuid, n: i64) {
    let pool = e.f.db.pool().await;
    for sql in [
        format!("INSERT INTO playbook_admissions(namespace_id,generation,request_id,request_digest,engine_key,grant_id,grant_revision,expected_activation_revision,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) SELECT $1,$2,gen_random_uuid(),$4,'done-'||i,$3,1,1,1,'test',{ZERO12},{ZERO16},i,'engine_accepted' FROM generate_series(1,$5) i"),
        "INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) SELECT $1,$2,'done-'||i,'run','message.send',$4,$4,$4,$3,1,gen_random_uuid(),'queued','{}',i FROM generate_series(1,$5) i".into(),
        "INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) SELECT $1,$2,'done-'||i,'cancel-done-'||i,$4,'confirmed',i FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL".into(),
        format!("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,fanout_complete) SELECT $1,$2,'aq.host.bridge@1',0,gen_random_uuid(),$4,1,'test',{ZERO12},{ZERO16},i,'engine_accepted',true FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL"),
        "INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,output,created_at_ms,completed_at_ms) SELECT gen_random_uuid(),$1,$2,'done-'||i,'judgment.request','work-'||i,1,'{}','assessed','{}',i,i FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL AND $4<>''".into(),
        "INSERT INTO playbook_health_observations(namespace_id,generation,observation,partition,baseline,expires_at_ms,evaluation,complete,request_digest,result) SELECT $1,$2,'done-'||i,'p','{}',i,'e',true,$4,'{}' FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL".into(),
        "INSERT INTO playbook_finding_notifications(namespace_id,generation,notification_id,request,submitted) SELECT $1,$2,gen_random_uuid(),'{}',true FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL AND $4<>''".into(),
        "INSERT INTO playbook_finding_episodes(namespace_id,generation,partition,finding_key,revision,present,evidence,last_notified_ms) SELECT $1,$2,'p','done-'||i,2,false,'e',i FROM generate_series(1,$5) i WHERE $3::uuid IS NOT NULL AND $4<>''".into(),
    ] {
        exec(&pool, &sql, e, grant, n).await;
    }
    finish_run(e, "run").await;
}

/// Records `run` as finished in the engine's history (a root run), which is
/// what lets its effects and host operations be deleted (migration 0128).
/// These tests open no PostgreSQL engine, so the history table is made here.
async fn finish_run(e: &Env, run: &str) {
    let pool = e.f.db.pool().await;
    sqlx::raw_sql("CREATE SCHEMA IF NOT EXISTS aq_playbooks; CREATE TABLE IF NOT EXISTS aq_playbooks.run_history(namespace text,kind text,item_key text,run_id text,root_id text,sequence bigint,finished_at_ms bigint,public_value jsonb,PRIMARY KEY(namespace,kind,item_key))")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO aq_playbooks.run_history VALUES($1,'runs',$2,$2,$2,0,0,'{}') ON CONFLICT DO NOTHING")
        .bind(e.service.namespace.engine_namespace())
        .bind(run)
        .execute(&pool)
        .await
        .unwrap();
}

/// A committed effect is what the engine reads back when it lost the
/// acknowledgement (reconcile), and a completed host operation is evidence
/// `candidate.publish` reads: neither is deleted while its run is not in the
/// engine's history, however many newer rows there are.
#[tokio::test]
async fn effects_and_host_operations_of_a_live_run_survive_deletion() {
    let e = setup().await;
    let old = expired_grant(&e).await;
    seed_finished(&e, old, 4).await;
    sqlx::query("UPDATE playbook_effects SET run_id='live' WHERE namespace_id=$1")
        .bind(e.service.namespace.namespace_id)
        .execute(&e.f.db.pool().await)
        .await
        .unwrap();
    for _ in 0..2 {
        prune(&e, 1, 500).await;
    }
    // The effects, and the operations of the same intents, are all kept.
    assert_eq!(rows(&e, "playbook_effects").await, 4);
    assert_eq!(rows(&e, "playbook_host_operations").await, 4);
    assert_eq!(rows(&e, "playbook_cancellations").await, 1);
    finish_run(&e, "live").await;
    prune(&e, 1, 500).await;
    assert_eq!(rows(&e, "playbook_effects").await, 1);
    assert_eq!(rows(&e, "playbook_host_operations").await, 1);
}

const TABLES: [&str; 8] = [
    "playbook_outbox",
    "playbook_admissions",
    "playbook_effects",
    "playbook_cancellations",
    "playbook_host_operations",
    "playbook_health_observations",
    "playbook_finding_notifications",
    "playbook_finding_episodes",
];

async fn rows(e: &Env, table: &str) -> i64 {
    assert!(TABLES.contains(&table));
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {table} WHERE namespace_id=$1"
    ))
    .bind(e.service.namespace.namespace_id)
    .fetch_one(&e.f.db.pool().await)
    .await
    .unwrap()
}

/// An expired grant row, so its accepted admissions can be deleted.
async fn expired_grant(e: &Env) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO playbook_grants(grant_id,namespace_id,generation,revision,owner_user_id,scope,expires_at_ms,payload_digest) VALUES($1,$2,$3,1,$4,'{}',1,$5)")
        .bind(id)
        .bind(e.service.namespace.namespace_id)
        .bind(e.service.namespace.generation)
        .bind(e.f.actor)
        .bind("a".repeat(64))
        .execute(&e.f.db.pool().await)
        .await
        .unwrap();
    id
}

/// One pass of every table; the number of rows each deleted.
async fn prune(e: &Env, keep: u64, batch: u64) -> Vec<u64> {
    let retention = aq_playbook_host::retention::Retention {
        keep_finished: keep,
        delete_batch: batch,
    };
    let mut deleted = vec![];
    for (i, (table, _)) in aq_playbook_host::retention::TABLES.iter().enumerate() {
        assert_eq!(*table, TABLES[i]);
        deleted.push(e.service.prune(&e.runtime, &retention, i).await.unwrap());
    }
    deleted
}

fn admission(e: &Env, grant: Uuid) -> PrepareRequest {
    PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: grant,
        grant_revision: 1,
        binding: "mail".into(),
        expected_activation_revision: 1,
        entry: "main".into(),
        input: json!({"n": e.attempt}),
    }
}

#[tokio::test]
async fn a_namespace_keeps_admitting_past_the_old_lifetime_caps() {
    let e = setup().await;
    let pool = e.f.db.pool().await;
    // Where the lifetime counters still exist, start them at their caps
    // (grants 1,024; admissions 4,096; effects and cancellations 8,192;
    // outbox 16,384; 64 MiB).
    sqlx::raw_sql("DO $$ BEGIN IF EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='playbook_namespaces' AND column_name='admission_count') THEN EXECUTE 'UPDATE playbook_namespaces SET grant_count=1024,admission_count=4096,effect_count=8192,cancellation_count=8192,outbox_count=16384,retained_bytes=67108864'; END IF; END $$")
        .execute(&pool)
        .await
        .unwrap();
    // And hold as many rows as the old caps allowed.
    let old = expired_grant(&e).await;
    let n = 16_384;
    seed_finished(&e, old, n).await;
    for table in TABLES {
        assert_eq!(rows(&e, table).await, n, "{table}");
    }
    let scope = GrantScope {
        workspace_id: e.f.selection.lineage.workspace_id,
        project_id: e.f.selection.lineage.project_id,
        recipients: vec![e.recipient.clone()],
        capabilities: BTreeSet::from(["message.send".into()]),
    };
    e.service
        .grant(e.f.actor, Uuid::new_v4(), scope, NOW as i64 + 10_000)
        .await
        .unwrap();
    let status = e
        .service
        .prepare(e.f.actor, admission(&e, e.grant))
        .await
        .unwrap();
    assert_eq!(status.status, "pending");
    assert!(matches!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &intent(&e, "past-cap"))
            .await
            .unwrap(),
        MessageResult::Queued(_)
    ));
    assert!(matches!(
        e.service
            .cancel(&e.runtime, e.grant, 1, &intent(&e, "cancel-past-cap"))
            .await
            .unwrap(),
        CancellationResult::Confirmed
    ));
    assert_eq!(rows(&e, "playbook_admissions").await, n + 1);
    assert_eq!(rows(&e, "playbook_effects").await, n + 1);
    assert_eq!(rows(&e, "playbook_outbox").await, n + 1);
    assert_eq!(rows(&e, "playbook_cancellations").await, n + 1);
}

#[tokio::test]
async fn finished_playbook_rows_are_deleted_down_to_keep_finished() {
    let e = setup().await;
    let old = expired_grant(&e).await;
    seed_finished(&e, old, 5).await;
    // An accepted judgment whose operation completed is finished too.
    sqlx::query("UPDATE playbook_effects SET command_name='judgment.request',status='accepted',receipt='{\"handle\":\"h\"}' WHERE namespace_id=$1 AND intent_id='done-1'")
        .bind(e.service.namespace.namespace_id)
        .execute(&e.f.db.pool().await)
        .await
        .unwrap();
    // Bounded batches: at most two rows per table per pass.
    assert_eq!(prune(&e, 2, 2).await, vec![2; 8]);
    assert_eq!(prune(&e, 2, 2).await, vec![1; 8]);
    assert_eq!(prune(&e, 2, 2).await, vec![0; 8]);
    for table in TABLES {
        assert_eq!(rows(&e, table).await, 2, "{table}");
    }
    // The newest two are kept.
    let pool = e.f.db.pool().await;
    for (table, column) in [
        ("playbook_admissions", "engine_key"),
        ("playbook_effects", "intent_id"),
        ("playbook_cancellations", "cancellation_key"),
        ("playbook_host_operations", "intent_id"),
        ("playbook_health_observations", "observation"),
        ("playbook_finding_episodes", "finding_key"),
    ] {
        let kept: Vec<String> =
            sqlx::query_scalar(&format!("SELECT {column} FROM {table} ORDER BY 1"))
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, ["done-4", "done-5"], "{table}");
    }
    let kept: Vec<i64> = sqlx::query_scalar("SELECT created_at_ms FROM playbook_outbox ORDER BY 1")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept, [4, 5]);
}

#[tokio::test]
async fn rows_in_progress_or_held_survive_deletion() {
    let e = setup().await;
    let pool = e.f.db.pool().await;
    let (ns, g) = (
        e.service.namespace.namespace_id,
        e.service.namespace.generation,
    );
    let old = expired_grant(&e).await;
    let d = "a".repeat(64);
    // Admissions: pending, held (unknown), and accepted under a live grant.
    for (status, held, grant) in [
        ("pending", None, old),
        ("unknown", Some("uncertain"), old),
        ("engine_accepted", None, e.grant),
    ] {
        sqlx::query(&format!("INSERT INTO playbook_admissions(namespace_id,generation,request_id,request_digest,engine_key,grant_id,grant_revision,expected_activation_revision,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,held_reason) VALUES($1,$2,gen_random_uuid(),$3,$4,$5,1,1,1,'test',{ZERO12},{ZERO16},0,$4,$6)"))
            .bind(ns).bind(g).bind(&d).bind(status).bind(grant).bind(held).execute(&pool).await.unwrap();
    }
    // Outbox: pending, held, accepted but not fanned out.
    for (status, fanned) in [
        ("pending", false),
        ("unknown", false),
        ("engine_accepted", false),
    ] {
        sqlx::query(&format!("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status,fanout_complete) VALUES($1,$2,'aq.host.bridge@1',0,gen_random_uuid(),$3,1,'test',{ZERO12},{ZERO16},0,$4,$5)"))
            .bind(ns).bind(g).bind(&d).bind(status).bind(fanned).execute(&pool).await.unwrap();
    }
    // Effects: pending; an accepted judgment whose operation runs; an
    // accepted review decision nobody has answered.
    let decision = Uuid::new_v4();
    for (intent, command, status, receipt) in [
        ("p", "work.change", "pending", json!({})),
        ("j", "judgment.request", "accepted", json!({"handle": "h"})),
        (
            "d",
            "decision.request",
            "accepted",
            json!({"handle": decision, "subject": {}}),
        ),
    ] {
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,'run',$4,$5,$5,$5,$6,1,gen_random_uuid(),$7,$8,0)")
            .bind(ns).bind(g).bind(intent).bind(command).bind(&d).bind(e.grant).bind(status).bind(receipt).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,created_at_ms) VALUES(gen_random_uuid(),$1,$2,'j','judgment.request','work-j',1,'{}','running',0)")
        .bind(ns).bind(g).execute(&pool).await.unwrap();
    // Cancellations requested but not yet answered.
    for (key, disposition) in [("a", "accepted"), ("u", "unknown")] {
        sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$3,$4,$5,0)")
            .bind(ns).bind(g).bind(key).bind(&d).bind(disposition).execute(&pool).await.unwrap();
    }
    // A scan that can still be reconciled, a notification not yet sent.
    sqlx::query("INSERT INTO playbook_health_observations(namespace_id,generation,observation,partition,baseline,expires_at_ms) VALUES($1,$2,'open','p','{}',$3)")
        .bind(ns).bind(g).bind(NOW as i64 + 60_000).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO playbook_finding_notifications(namespace_id,generation,notification_id,request) VALUES($1,$2,gen_random_uuid(),'{}')")
        .bind(ns).bind(g).execute(&pool).await.unwrap();
    // A finding that is still open.
    sqlx::query("INSERT INTO playbook_finding_episodes(namespace_id,generation,partition,finding_key,revision,present,evidence,last_notified_ms) VALUES($1,$2,'p','open',1,true,'e',0)")
        .bind(ns).bind(g).execute(&pool).await.unwrap();
    let before: Vec<i64> = {
        let mut v = vec![];
        for table in TABLES {
            v.push(rows(&e, table).await);
        }
        v
    };
    assert_eq!(before, [3, 3, 3, 2, 1, 1, 1, 1]);
    // Many finished rows newer than all of them, and the smallest keep.
    seed_finished(&e, old, 4).await;
    for _ in 0..4 {
        prune(&e, 1, 500).await;
    }
    for (i, table) in TABLES.into_iter().enumerate() {
        // Each table keeps its one newest finished row and all the others.
        // The cleared episodes all stay: the open scan above is of their
        // partition, and its baseline holds their revisions.
        let kept = if table == "playbook_finding_episodes" {
            4
        } else {
            1
        };
        assert_eq!(rows(&e, table).await, before[i] + kept, "{table}");
    }
    // The running judgment, unanswered decision and pending effect remain.
    let effects: Vec<String> = sqlx::query_scalar(
        "SELECT intent_id FROM playbook_effects WHERE intent_id NOT LIKE 'done-%' ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(effects, ["d", "j", "p"]);
}

#[tokio::test]
async fn a_repeat_of_a_deleted_operation_is_not_executed_twice() {
    let e = setup().await;
    let pool = e.f.db.pool().await;
    let accept = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "UPDATE playbook_admissions SET status='engine_accepted' WHERE request_id=$1",
            )
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let first = admission(&e, e.grant);
    e.service.prepare(e.f.actor, first.clone()).await.unwrap();
    accept(first.request_id).await;
    for step in 1..=2 {
        e.f.time
            .store(NOW as i64 + step, std::sync::atomic::Ordering::SeqCst);
        let newer = admission(&e, e.grant);
        e.service.prepare(e.f.actor, newer.clone()).await.unwrap();
        accept(newer.request_id).await;
    }
    // While its grant can still admit, an accepted admission beyond keep is
    // kept: its repeat returns the original record and admits nothing.
    prune(&e, 1, 500).await;
    assert_eq!(rows(&e, "playbook_admissions").await, 3);
    let repeat = e.service.prepare(e.f.actor, first.clone()).await.unwrap();
    assert_eq!(repeat.status, "engine_accepted");
    // Once the grant has expired, the older two leave.
    e.f.time
        .store(NOW as i64 + 20_000, std::sync::atomic::Ordering::SeqCst);
    prune(&e, 1, 500).await;
    assert_eq!(rows(&e, "playbook_admissions").await, 1);
    // A repeat of the deleted request is refused as expired; no new
    // admission is queued, so the engine is never asked to run it again.
    assert_eq!(
        e.service.prepare(e.f.actor, first).await.unwrap_err(),
        Error::Forbidden
    );
    assert_eq!(rows(&e, "playbook_admissions").await, 1);
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM playbook_admissions WHERE status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending, 0);
}
