//! Explicit candidate0023 schema component tests on disposable PostgreSQL.
//! No normal migration registration, native credential issuance, production
//! mount or provider qualification is claimed by these synthetic known bindings.
mod common;
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
}
async fn setup() -> Env {
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
        sqlx::raw_sql(include_str!("../migrations/0023_playbook_bridge.sql"))
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
    let service = Service::new(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("fixture-{}", Uuid::new_v4()),
            workspace_id: f.selection.lineage.workspace_id,
            project_id: f.selection.lineage.project_id,
            owner_user_id: f.actor,
        },
        Arc::new(keys),
    )
    .await
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
async fn quota_refusal_rolls_back_mailbox_and_event_but_retained_exact_replay_works() {
    let e = setup().await;
    let pool = e.f.db.pool().await;
    sqlx::query("UPDATE playbook_namespaces SET effect_count=8192 WHERE namespace_id=$1")
        .bind(e.service.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &intent(&e, "refused"))
            .await
            .unwrap_err(),
        Error::Overloaded
    );
    for table in ["mailbox_messages", "playbook_effects", "playbook_outbox"] {
        assert_eq!(count(&e, table).await, 0);
    }
    sqlx::query("UPDATE playbook_namespaces SET effect_count=0 WHERE namespace_id=$1")
        .bind(e.service.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();
    let r = intent(&e, "accepted");
    e.service
        .send_message(&e.runtime, e.grant, 1, &r)
        .await
        .unwrap();
    sqlx::query("UPDATE playbook_namespaces SET retained_bytes=67108864,effect_count=8192,outbox_count=16384 WHERE namespace_id=$1").bind(e.service.namespace.namespace_id).execute(&pool).await.unwrap();
    assert!(matches!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &r)
            .await
            .unwrap(),
        MessageResult::Queued(_)
    ));
    assert_eq!(
        e.service
            .send_message(&e.runtime, e.grant, 1, &intent(&e, "full"))
            .await
            .unwrap_err(),
        Error::Overloaded
    );
    assert_eq!(count(&e, "mailbox_messages").await, 1);
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
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    assert_eq!(url.port(), Some(55432));
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
