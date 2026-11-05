//! Actual completion and protected engine outboxes; synthetic execution/Graph peers.
use super::*;
use aq_server::{
    playbooks::{
        driver::Driver,
        review_domain::CatalogProfile,
        service::{Namespace, Service},
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use sqlx::Row;
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, time::Duration};

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
async fn settlement_completion_outbox_replays_engine_acceptance_before_ack() {
    let c = Box::pin(setup()).await;
    let f = &c.env.f;
    let pool = f.db.pool().await;
    let path = c.env._directory.path().join("completion-outbox-key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Arc::new(
        Keyring::load(&ProtectionConfig {
            active_key_id: "fixture".into(),
            keys: vec![KeyFile {
                id: "fixture".into(),
                file: path,
            }],
        })
        .unwrap(),
    );
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: "settlement-completion-fixture".into(),
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        owner_user_id: c.actor.id,
    };
    let service = Service::new_profile4(&f.state, namespace.clone(), keys.clone())
        .await
        .unwrap();
    service.initialize(c.actor.id).await.unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")));
    url.set_path(f.db.options().get_database().unwrap());
    let driver = Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(url.to_string()),
    )
    .await
    .unwrap();
    let mut artifact = pure_artifact();
    artifact.content.registry = CatalogProfile::SettlementEvents4.registry();
    let artifact = aq_playbook_host::model::Artifact::seal(artifact.content).unwrap();
    let accepted = Box::pin(c.service.evaluate(&c.actor, &c.request))
        .await
        .unwrap();
    let op = accepted.completion_operation_id.unwrap();
    assert_eq!(
        driver.test_collect_settlement_events().await.unwrap(),
        0,
        "prepared is not completed"
    );
    c.loss.store(true, Ordering::SeqCst);
    assert_eq!(
        Box::pin(c.service.complete(&c.actor, op))
            .await
            .unwrap()
            .state,
        "possible"
    );
    assert_eq!(
        driver.test_collect_settlement_events().await.unwrap(),
        0,
        "possible is not completed"
    );
    assert_eq!(
        Box::pin(c.service.complete(&c.actor, op))
            .await
            .unwrap()
            .state,
        "confirmed"
    );
    let acknowledged = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT outbox_acknowledged FROM profiled_task_completions WHERE operation_id=$1",
        )
        .bind(op)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    // No installed contract: ordinary driving must keep the source pending.
    driver.quantum().await.unwrap();
    assert!(!acknowledged().await);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    // A different project in the same workspace cannot collect this event.
    let foreign_project = Uuid::new_v4();
    // Synthetic second project binding, using the same qualified fixture shape.
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) SELECT $1,1,jsonb_set(binding,'{project_id}',to_jsonb($1::text)),validated_at_ms,binding_observation FROM project_execution_bindings WHERE project_id=$2")
        .bind(foreign_project).bind(namespace.project_id).execute(&pool).await.unwrap();
    let foreign = Service::new_profile4(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            project_id: foreign_project,
            name: "foreign-completion-fixture".into(),
            ..namespace
        },
        keys,
    )
    .await
    .unwrap();
    foreign.initialize(c.actor.id).await.unwrap();
    let foreign_driver = Driver::start(
        foreign,
        aq_playbook_host::Limits::default(),
        protected_factory(url.to_string()),
    )
    .await
    .unwrap();
    assert_eq!(
        foreign_driver
            .test_collect_settlement_events()
            .await
            .unwrap(),
        0
    );
    foreign_driver.stop().await.unwrap();
    driver.install(artifact.clone()).await.unwrap();
    assert_eq!(driver.test_collect_settlement_events().await.unwrap(), 1);
    assert_eq!(driver.test_collect_settlement_events().await.unwrap(), 0);
    let first = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(!first.duplicate);
    assert!(!acknowledged().await, "an omitted AQ ACK remains pending");
    driver.stop().await.unwrap();
    let driver = Driver::start(
        service,
        aq_playbook_host::Limits::default(),
        protected_factory(url.to_string()),
    )
    .await
    .unwrap();
    driver.install(artifact).await.unwrap();
    let replay = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(first.key, replay.key);
    driver.quantum().await.unwrap();
    assert!(acknowledged().await);
    let row = sqlx::query(
        "SELECT producer,event_id,status,fanout_complete FROM playbook_outbox WHERE event_id=$1",
    )
    .bind(op)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("producer"), "aq.task.settlement@1");
    assert_eq!(row.get::<Uuid, _>("event_id"), op);
    assert_eq!(row.get::<String, _>("status"), "engine_accepted");
    assert!(row.get::<bool, _>("fanout_complete"));
    assert_eq!(driver.test_collect_settlement_events().await.unwrap(), 0);
    // Confirmation replay neither reopens the outbox nor repeats Graph mutation.
    assert_eq!(
        Box::pin(c.service.complete(&c.actor, op))
            .await
            .unwrap()
            .state,
        "confirmed"
    );
    assert!(acknowledged().await);
    assert_eq!(c.writes.load(Ordering::SeqCst), 1);
    driver.stop().await.unwrap();
    c.env.peer.abort();
    c._graph.abort();
}
