use super::*;
use aq_playbook_host::{model::*, plugins::Plugins};
use aq_server::playbooks::{driver::Driver, inspection::RunsQuery};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::Path};

const PACKAGE: &str = "tests/fixtures/plugins/test";
const COMMAND: &str = "ext.test.ping";
const EVENT: &str = "ext.test.pinged";

struct PluginFiles(tempfile::TempDir);

impl PluginFiles {
    fn new() -> Self {
        let dir = Self(tempfile::tempdir().unwrap());
        let package = dir.root().join("plugins/test");
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::create_dir_all(package.join("contracts")).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(PACKAGE);
        for path in [
            "plugin.toml",
            "bin/test",
            "contracts/notes.json",
            "contracts/registry.json",
        ] {
            fs::copy(source.join(path), package.join(path)).unwrap();
        }
        fs::create_dir_all(dir.root().join("vault/plugins/test")).unwrap();
        dir.private("secret", "fake token");
        dir
    }

    fn root(&self) -> &Path {
        self.0.path()
    }

    fn private(&self, file: &str, contents: &str) {
        let path = self.root().join("vault/plugins/test").join(file);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn discover(&self) -> Plugins {
        Plugins::discover(
            &self.root().join("plugins"),
            &self.root().join("state"),
            &self.root().join("vault"),
        )
    }

    fn update_registry(&self, update: impl FnOnce(&mut Registry)) {
        let path = self.root().join("plugins/test/contracts/registry.json");
        let mut registry = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        update(&mut registry);
        fs::write(path, serde_json::to_vec(&registry).unwrap()).unwrap();
    }

    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root().join("state/test/calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

async fn plugin_grant(e: &Env, service: &Service) -> Uuid {
    let grant = Uuid::new_v4();
    service
        .grant(
            e.f.actor,
            grant,
            GrantScope {
                workspace_id: service.namespace.workspace_id,
                project_id: service.namespace.project_id,
                recipients: vec![],
                capabilities: BTreeSet::from(["ext.test".into()]),
            },
            NOW as i64 + 10_000,
        )
        .await
        .unwrap();
    grant
}

fn plugin_intent(e: &Env, service: &Service, grant: Uuid, mode: &str) -> Intent {
    let mut intent = super::intent(e, mode);
    intent.input = json!({"mode":mode});
    intent.input_digest = aq_playbook_host::model::digest(&intent.input).unwrap();
    intent.command = service.registry().commands[COMMAND].clone();
    intent.authority = authority(grant, 1);
    intent.authority.capabilities = BTreeSet::from(["ext.test".into()]);
    intent
}

#[tokio::test]
async fn plugin_transport_retains_replay_and_refuses_invalid_responses() {
    let e = setup().await;
    let files = PluginFiles::new();
    let service = e.service.clone().with_plugins(files.discover());
    let grant = plugin_grant(&e, &service).await;
    let project = service.namespace.project_id;
    files.private(
        "bindings.toml",
        &format!("[projects.{project}]\nmode='first'\n"),
    );
    let crashed = plugin_intent(&e, &service, grant, "crash");
    assert_eq!(
        service.host_dispatch(&e.runtime, grant, 1, &crashed).await,
        Err(Error::Unknown)
    );
    assert_eq!(count(&e, "playbook_effects").await, 0);
    let replay = service
        .host_dispatch(&e.runtime, grant, 1, &crashed)
        .await
        .unwrap();
    assert_eq!(
        service
            .host_dispatch(&e.runtime, grant, 1, &crashed)
            .await
            .unwrap(),
        replay
    );
    assert_eq!(replay.outcome, "ok");
    assert_eq!(
        replay.output["environment"],
        json!(["AQ_PLUGIN_PROTOCOL", "HOME", "PATH", "TMPDIR"])
    );
    let calls = files.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["idempotency_key"], calls[1]["idempotency_key"]);
    assert_eq!(calls[0]["deadline_ms"], 1000);
    assert_eq!(calls[0]["binding"], json!({"mode":"first"}));
    assert_eq!(calls[0]["project_id"], project.to_string());
    assert_eq!(count(&e, "playbook_effects").await, 1);

    files.private(
        "bindings.toml",
        &format!("[projects.{project}.nested]\nmode='reloaded'\n"),
    );
    for mode in ["nonzero", "bad_json"] {
        let intent = plugin_intent(&e, &service, grant, mode);
        assert_eq!(
            service.host_dispatch(&e.runtime, grant, 1, &intent).await,
            Err(Error::Unknown)
        );
    }
    assert_eq!(
        files.calls()[2]["binding"],
        json!({"nested":{"mode":"reloaded"}})
    );
    for mode in [
        "wrong_outcome",
        "wrong_output",
        "invalid_event",
        "host_event",
    ] {
        let intent = plugin_intent(&e, &service, grant, mode);
        let result = service
            .host_dispatch(&e.runtime, grant, 1, &intent)
            .await
            .unwrap();
        assert_eq!(result.outcome, "refused");
        assert_eq!(result.output, json!({"reason":"invalid_plugin_response"}));
    }
    let pool = e.f.db.pool().await;
    let notices: Vec<String> =
        sqlx::query_scalar("SELECT identity FROM playbook_plugin_notifications")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].starts_with("plugin:test:invalid:"));
    let events = service.outbox(&e.runtime, 0, 16).await.unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0].2;
    assert_eq!(
        event.event_id,
        aq_playbook_host::model::digest(&(&crashed.id, 0)).unwrap()
    );
    assert_eq!(event.producer, "ext.test");
    assert_eq!(event.subject.as_deref(), Some("sample"));
    assert_eq!(event.revision, Some(5));
    assert_eq!(event.correlation["ref"], "ready");
    assert_eq!(event.correlation["subject.id"], "sample");
    let retain_until: i64 =
        sqlx::query_scalar("SELECT plugin_retain_until_ms FROM playbook_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retain_until, NOW as i64 + 3_600_000);
}

#[test]
fn plugin_discovery_refuses_missing_secret_and_missing_refused_outcome() {
    let files = PluginFiles::new();
    fs::remove_file(files.root().join("vault/plugins/test/secret")).unwrap();
    assert!(files.discover().get("test").is_none());
    files.private("secret", "fake token");
    assert!(files.discover().get("test").is_some());

    files.update_registry(|registry| {
        registry
            .commands
            .get_mut(COMMAND)
            .unwrap()
            .outcomes
            .remove("refused");
    });
    assert!(files.discover().get("test").is_none());
}

fn reference(name: &str, fingerprint: String) -> ContractRef {
    ContractRef {
        name: name.into(),
        fingerprint,
    }
}

#[tokio::test]
async fn plugin_timeout_retries_and_trigger_delivery_replays_once() {
    let e = setup().await;
    let files = PluginFiles::new();
    let service = e.service.clone().with_plugins(files.discover());
    let grant = plugin_grant(&e, &service).await;
    let driver = Driver::start(
        service.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();

    let command = service.registry().commands[COMMAND].clone();
    let mut producer = pure_artifact().content;
    producer
        .registry
        .commands
        .insert(COMMAND.into(), command.clone());
    producer
        .playbook
        .required_capabilities
        .insert("ext.test".into());
    let main = producer.playbook.entries.get_mut("main").unwrap();
    main.start = "ping".into();
    main.input = command.input.clone();
    main.outcomes.insert("refused".into(), Schema::Null);
    producer.playbook.steps.insert(
        "ping".into(),
        Step {
            op: Operation::Invoke {
                command: reference(COMMAND, command.fingerprint().unwrap()),
                input: Expr::Ref {
                    root: "input".into(),
                    path: vec![],
                },
                bind: "ping_result".into(),
                retry: None,
            },
            edges: BTreeMap::from([
                ("ok".into(), "done".into()),
                ("refused".into(), "refused".into()),
            ]),
            on_error: None,
        },
    );
    producer.playbook.steps.insert(
        "refused".into(),
        Step {
            op: Operation::Return {
                outcome: "refused".into(),
                output: Expr::literal(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    let event = service.registry().events[EVENT].clone();
    producer.registry.events.insert(EVENT.into(), event.clone());
    let producer = Artifact::seal(producer).unwrap();
    let artifact = producer.digest.clone();

    let mut consumer = pure_artifact().content;
    consumer.registry.events.insert(EVENT.into(), event.clone());
    consumer.playbook.triggers.push(Trigger {
        id: "pinged".into(),
        event: reference(EVENT, event.fingerprint().unwrap()),
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    let consumer = Artifact::seal(consumer).unwrap();
    let consumer_id = consumer.digest.clone();
    driver.install(consumer).await.unwrap();
    driver
        .activate("consume".into(), consumer_id.clone(), e.grant, 1)
        .await
        .unwrap();
    driver.install(producer).await.unwrap();
    let binding = driver
        .activate("produce".into(), artifact.clone(), grant, 1)
        .await
        .unwrap();
    let admit = |mode| {
        service.prepare(
            e.f.actor,
            PrepareRequest {
                request_id: Uuid::new_v4(),
                grant_id: grant,
                grant_revision: 1,
                binding: "produce".into(),
                expected_activation_revision: binding["revision"].as_u64().unwrap(),
                entry: "main".into(),
                input: json!({"mode":mode}),
            },
        )
    };
    admit("timeout").await.unwrap();
    driver.quantum().await.unwrap();
    assert_eq!(count(&e, "playbook_effects").await, 0);
    driver.quantum().await.unwrap();
    let calls = files.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["idempotency_key"], calls[1]["idempotency_key"]);
    assert_eq!(count(&e, "playbook_effects").await, 1);
    let first = driver.test_commit_event_without_aq_ack().await.unwrap();
    let replay = driver.test_commit_event_without_aq_ack().await.unwrap();
    assert!(!first.duplicate && replay.duplicate);
    assert_eq!(first.key, replay.key);
    assert_eq!(first.destinations, 1);
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    admit("invalid_event").await.unwrap();
    admit("host_event").await.unwrap();
    for _ in 0..3 {
        driver.quantum().await.unwrap();
    }
    let runs = driver
        .inspect_runs(e.f.actor, RunsQuery::default())
        .await
        .unwrap();
    assert_eq!(
        runs.items
            .iter()
            .filter(|r| r.artifact == consumer_id)
            .count(),
        1
    );
    assert_eq!(count(&e, "playbook_outbox").await, 1);
    let pool = e.f.db.pool().await;
    let notifications: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(notifications, 1);

    // Change only the event contract pinned by an already admitted run.
    let pending = admit("normal").await.unwrap();
    driver
        .admission(e.f.actor, pending.request_id, false)
        .await
        .unwrap();
    driver.stop().await.unwrap();
    files.update_registry(|registry| registry.events.get_mut(EVENT).unwrap().retention_ms += 1);
    let upgraded = e.service.clone().with_plugins(files.discover());
    assert_eq!(upgraded.registry().commands[COMMAND], command);
    assert_ne!(upgraded.registry().events[EVENT], event);
    let restarted = Driver::start(
        upgraded,
        aq_playbook_host::Limits::default(),
        protected_factory(source_url(&e)),
    )
    .await
    .unwrap();
    assert_eq!(
        restarted
            .activate("produce".into(), artifact, grant, 1)
            .await,
        Err(Error::Invalid)
    );
    for _ in 0..3 {
        restarted.quantum().await.unwrap();
    }
    let reason: String = sqlx::query_scalar("SELECT receipt->'output'->>'reason' FROM playbook_effects WHERE receipt->'output'->>'reason' = 'plugin_unavailable'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(reason, "plugin_unavailable");
    assert_eq!(files.calls().len(), 4);
    restarted.stop().await.unwrap();
}
