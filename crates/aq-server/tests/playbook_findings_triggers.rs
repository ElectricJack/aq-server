//! D8: real AQ transactions and the pinned engine; fake graph has no mutations.
mod common;
use aq_playbook_host::{
    model::*,
    runner::{Engine, Intent, IntentStatus, MemoryStorage},
};
use aq_server::playbooks::domain::digest;
use aq_server::playbooks::{
    domain::*,
    host_adapters::WorkGraph,
    review_domain::CatalogProfile,
    service::*,
    triggers::{DailySchedule, Misfire},
};
use aq_server::protected_storage::{KeyFile, Keyring, ProtectionConfig};
use serde_json::{Value, json};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    sync::Arc,
};
use uuid::Uuid;
struct NoGraph;
#[async_trait::async_trait]
impl WorkGraph for NoGraph {
    async fn node(&self, _: Uuid, _: Uuid) -> Result<Option<Value>> {
        Ok(None)
    }
    async fn mutate(&self, _: Uuid, _: Value) -> Result<(u16, Value)> {
        panic!("no graph writes")
    }
}
struct Env {
    f: common::profiled::Fixture,
    s: Service,
    keys: Arc<Keyring>,
    owner: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
}
async fn setup() -> Env {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    std::fs::write(&path, [9_u8; 32]).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let keys = Arc::new(
        Keyring::load(&ProtectionConfig {
            active_key_id: "test".into(),
            keys: vec![KeyFile {
                id: "test".into(),
                file: path,
            }],
        })
        .unwrap(),
    );
    let l = &f.selection.lineage;
    let s = Service::new_monitoring6(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("d8-{}", Uuid::new_v4()),
            workspace_id: l.workspace_id,
            project_id: l.project_id,
            owner_user_id: f.actor,
        },
        keys.clone(),
        Arc::new(NoGraph),
    )
    .await
    .unwrap();
    s.initialize(f.actor).await.unwrap();
    let scope = GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(["context.read".into(), "findings.reconcile".into()]),
    };
    let grant = Uuid::new_v4();
    s.grant(
        f.actor,
        grant,
        scope.clone(),
        common::profiled::NOW as i64 + 86_400_000,
    )
    .await
    .unwrap();
    let owner = s.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
    Env {
        f,
        s,
        keys,
        owner,
        grant,
        scope,
    }
}
fn artifact(event: EventContract, trigger: bool) -> Artifact {
    let reference = ContractRef {
        name: event.name.clone(),
        fingerprint: event.fingerprint().unwrap(),
    };
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "D8 test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"D8 immutable source"),
            header: SourceHeader {
                format_version: 1,
                id: "monitor".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "monitor".into(),
            purpose: "D8 admission".into(),
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
                        output: Expr::literal(Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: if trigger {
                vec![Trigger {
                    id: "confirmed".into(),
                    event: reference,
                    entry: "main".into(),
                    input: Expr::literal(Value::Null),
                }]
            } else {
                vec![]
            },
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(event.name.clone(), event)]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
#[tokio::test]
async fn domain_event_trigger_admits_once() {
    let e = setup().await;
    let pool = e.f.db.pool().await;
    let l = &e.f.selection.lineage;
    let attempt = e.f.profiled.offer.binding.attempt_id;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
        .bind(attempt).bind(e.f.node.node_id).bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id})).execute(&pool).await.unwrap();
    let operation = Uuid::new_v4();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO agent_graph_operations(operation_id,attempt_id,node_id,request,payload_digest,mapped,policy,status,phase,created_at_ms,retained_bytes) VALUES($1,$2,$3,'{}','d8','{}','{}','{}','queued',0,0)")
        .bind(operation).bind(attempt).bind(e.f.node.node_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO agent_graph_outbox(event_id,operation_id,kind,payload) VALUES($1,$2,'graph.operation_confirmed','{}')").bind(id).bind(operation).execute(&pool).await.unwrap();
    assert_eq!(
        e.s.collect_domain_events(&e.owner, 2).await.unwrap(),
        0,
        "unconfirmed operations never trigger"
    );
    sqlx::query("UPDATE agent_graph_operations SET phase='confirmed' WHERE operation_id=$1")
        .bind(operation)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(e.s.collect_domain_events(&e.owner, 2).await.unwrap(), 1);
    assert_eq!(e.s.collect_domain_events(&e.owner, 2).await.unwrap(), 0);
    let event = e.s.outbox(&e.owner, 0, 2).await.unwrap().remove(0).2;
    let memory = MemoryStorage::default();
    let engine = Engine::new(memory.clone(), "d8", "first").unwrap();
    let a = artifact(
        aq_server::playbooks::review_domain::graph_operation_event(),
        true,
    );
    engine.install(a.clone()).unwrap();
    engine
        .activate(
            "activation",
            &a.digest,
            aq_playbook_host::runner::Authority {
                actor: "host".into(),
                scope: "project".into(),
                capabilities: BTreeSet::new(),
            },
        )
        .unwrap();
    let first = engine
        .admit_event(&event.producer, event.clone(), 1)
        .unwrap();
    assert_eq!(first.destinations, 1);
    engine.fanout(&first.key, 2, 1).unwrap();
    // Crash cut: engine accepted and fanned out, AQ has no acknowledgement yet.
    engine.release().unwrap();
    let restarted = Engine::new(memory, "d8", "second").unwrap();
    let replay = restarted
        .admit_event(&event.producer, event.clone(), 2)
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(first.key, replay.key);
    assert_eq!(replay.destinations, 1);
    restarted.fanout(&replay.key, 2, 2).unwrap();
    assert_eq!(restarted.snapshot().unwrap().runs.len(), 1);
    let mut changed = event.clone();
    changed.payload["task_id"] = json!(Uuid::new_v4());
    assert!(
        restarted
            .admit_event(&changed.producer, changed.clone(), 3)
            .is_err()
    );
    e.s.acknowledge_event(&e.owner, id, replay, true)
        .await
        .unwrap();
    assert!(e.s.outbox(&e.owner, 0, 2).await.unwrap().is_empty());
    // No matching subscription is a durable disposition at this catalog revision.
    let a = artifact(
        aq_server::playbooks::review_domain::graph_operation_event(),
        false,
    );
    restarted.install(a.clone()).unwrap();
    restarted
        .activate(
            "activation",
            &a.digest,
            aq_playbook_host::runner::Authority {
                actor: "host".into(),
                scope: "project".into(),
                capabilities: BTreeSet::new(),
            },
        )
        .unwrap();
    let mut unmatched = event.clone();
    unmatched.event_id = Uuid::new_v4().to_string();
    let empty = restarted
        .admit_event(&unmatched.producer, unmatched.clone(), 4)
        .unwrap();
    let state = restarted.snapshot().unwrap();
    assert_eq!(empty.destinations, 0);
    let original_revision = state.events[&empty.key].catalog_revision;
    let matching = artifact(
        aq_server::playbooks::review_domain::graph_operation_event(),
        true,
    );
    restarted.install(matching.clone()).unwrap();
    restarted
        .activate(
            "activation",
            &matching.digest,
            aq_playbook_host::runner::Authority {
                actor: "host".into(),
                scope: "project".into(),
                capabilities: BTreeSet::new(),
            },
        )
        .unwrap();
    let replay = restarted
        .admit_event(&unmatched.producer, unmatched.clone(), 5)
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.destinations, 0);
    assert_eq!(
        restarted.snapshot().unwrap().events[&empty.key].catalog_revision,
        original_revision
    );

    assert_eq!(
        state.events[&empty.key].catalog_revision,
        state.catalog_revision
    );
    restarted.release().unwrap();
}
#[tokio::test]
async fn schedule_survives_restart_without_double_fire() {
    let e = setup().await;
    let now = common::profiled::NOW as i64;
    let local_date = chrono::DateTime::from_timestamp_millis(now)
        .unwrap()
        .date_naive();
    let schedule = DailySchedule {
        schedule_id: Uuid::new_v4(),
        timezone: "UTC".into(),
        hour: 0,
        minute: 0,
        start_date: local_date,
        misfire: Misfire::CoalesceOnce,
    };
    assert_eq!(
        e.s.put_schedule(&e.owner, schedule.clone(), 0)
            .await
            .unwrap(),
        1
    );
    assert_eq!(e.s.collect_schedule_events(&e.owner, 2).await.unwrap(), 1);
    let first = e.s.outbox(&e.owner, 0, 2).await.unwrap();
    assert_eq!(first.len(), 1);
    e.s.retire_runtime(&e.owner).await.unwrap();
    let next = e.s.adopt_runtime(Uuid::new_v4(), 2).await.unwrap();
    assert_eq!(e.s.collect_schedule_events(&next, 2).await.unwrap(), 0);
    assert_eq!(e.s.outbox(&next, 0, 2).await.unwrap()[0].2, first[0].2);
    assert_eq!(
        e.s.put_schedule(&next, schedule.clone(), 0).await,
        Err(Error::Conflict)
    );
    // A changed revision has its own local-date occurrence identity.
    let mut changed = schedule.clone();
    changed.minute = 1;
    assert_eq!(e.s.put_schedule(&next, changed, 1).await.unwrap(), 2);
    assert_eq!(e.s.collect_schedule_events(&next, 2).await.unwrap(), 1);
    assert_eq!(e.s.outbox(&next, 0, 4).await.unwrap().len(), 2);
    let pool = e.f.db.pool().await;
    sqlx::query("UPDATE playbook_namespaces SET outbox_count=16384 WHERE namespace_id=$1")
        .bind(e.s.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();
    let tomorrow = DailySchedule {
        schedule_id: Uuid::new_v4(),
        ..schedule
    };
    e.s.put_schedule(&next, tomorrow, 0).await.unwrap();
    assert_eq!(
        e.s.collect_schedule_events(&next, 2).await,
        Err(Error::Overloaded)
    );
    sqlx::query("UPDATE playbook_namespaces SET outbox_count=2 WHERE namespace_id=$1")
        .bind(e.s.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        e.s.collect_schedule_events(&next, 2).await.unwrap(),
        1,
        "refusal must not advance the cursor"
    );
}
impl Env {
    fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
        Intent {
            id: id.into(),
            run: "monitor".into(),
            step: name.replace('.', "-"),
            invocation: 1,
            deadline_at_ms: (self.f.state.clock)().timestamp_millis() + 5000,
            command: CatalogProfile::Monitoring6.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: CatalogProfile::Monitoring6
                .authority(self.grant, 1, &self.scope)
                .unwrap(),
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
    async fn scan(&self, name: &str, complete: bool) {
        self.s
            .start_health_scan(&self.owner, name, "workers", 60_000)
            .await
            .unwrap();
        self.s
            .complete_health_scan(&self.owner, name, &format!("eval-{name}"), complete)
            .await
            .unwrap();
    }
    async fn findings(
        &self,
        id: &str,
        observation: &str,
        findings: Value,
    ) -> Result<aq_server::playbooks::host_adapters::HostResult> {
        self.s.host_dispatch(&self.owner,self.grant,1,&self.intent(id,"findings.reconcile",
            json!({"partition":"workers","observation":observation,"evaluation":format!("eval-{observation}"),"findings":findings,"reminder_ms":1000}))).await
    }
}
#[tokio::test]
async fn findings_reconcile_preserves_episode_and_notification_identity() {
    let e = setup().await;
    // Notification owner validates the actual bound project and derives recipient.
    let pool = e.f.db.pool().await;
    sqlx::query("UPDATE project_execution_bindings SET binding=binding || $2 WHERE project_id=$1")
        .bind(e.s.namespace.project_id)
        .bind(json!({"enabled":true,"graph_workspace_id":e.s.namespace.workspace_id}))
        .execute(&pool)
        .await
        .unwrap();
    e.scan("one", false).await;
    sqlx::query("UPDATE playbook_namespaces SET effect_count=8192 WHERE namespace_id=$1")
        .bind(e.s.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        e.findings(
            "first",
            "one",
            json!([{"key":"offline","expected_revision":0,"evidence":"node/a"}])
        )
        .await,
        Err(Error::Overloaded)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_finding_episodes")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_finding_notifications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE playbook_namespaces SET effect_count=0 WHERE namespace_id=$1")
        .bind(e.s.namespace.namespace_id)
        .execute(&pool)
        .await
        .unwrap();

    let first = e
        .findings(
            "first",
            "one",
            json!([{"key":"offline","expected_revision":0,"evidence":"node/a"}]),
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, "reconciled");
    // Same observation from a different intent returns the original result.
    assert_eq!(
        e.findings(
            "retry",
            "one",
            json!([{"key":"offline","expected_revision":0,"evidence":"node/a"}])
        )
        .await
        .unwrap(),
        first
    );
    e.scan("two", true).await;
    e.findings(
        "unchanged",
        "two",
        json!([{"key":"offline","expected_revision":1,"evidence":"node/a"}]),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_finding_notifications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    e.f.time
        .fetch_add(1000, std::sync::atomic::Ordering::SeqCst);
    e.scan("three", true).await;
    e.findings(
        "reminder",
        "three",
        json!([{"key":"offline","expected_revision":2,"evidence":"node/a"}]),
    )
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_finding_notifications")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    // Health view carries the revisions captured before the next scan.
    let view=e.s.host_dispatch(&e.owner,e.grant,1,&e.intent("view","context.read",json!({"view":"health_observations","subject":{"id":"workers","revision":0},"snapshot":"","page":0}))).await.unwrap();
    assert_eq!(view.outcome, "observed");
    let snapshot: Value = serde_json::from_str(view.output["snapshot"].as_str().unwrap()).unwrap();
    assert_eq!(snapshot["offline"], 3);
    e.scan("clear", true).await;
    assert_eq!(
        e.findings("clear", "clear", json!([]))
            .await
            .unwrap()
            .outcome,
        "reconciled"
    );
    let row = sqlx::query(
        "SELECT revision,present FROM playbook_finding_episodes WHERE finding_key='offline'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<i64, _>("revision"), 4);
    assert!(!row.get::<bool, _>("present"));
    assert_eq!(
        e.s.flush_finding_notifications(&e.owner, 8).await.unwrap(),
        3
    );
    // Requeue after enqueue commit but before local ACK: one logical message.
    sqlx::query("UPDATE playbook_finding_notifications SET submitted=false")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        e.s.flush_finding_notifications(&e.owner, 8).await.unwrap(),
        3
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notification_messages")
            .fetch_one(&pool)
            .await
            .unwrap(),
        3
    );
}
#[tokio::test]
async fn findings_clear_requires_original_complete_scan() {
    let e = setup().await;
    e.scan("open", false).await;
    e.findings(
        "open",
        "open",
        json!([{"key":"offline","expected_revision":0,"evidence":"node/a"}]),
    )
    .await
    .unwrap();
    e.scan("stale", true).await; // baseline revision 1
    e.scan("partial", false).await;
    e.findings("partial", "partial", json!([])).await.unwrap();
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT present FROM playbook_finding_episodes")
            .fetch_one(&e.f.db.pool().await)
            .await
            .unwrap()
    );
    e.scan("update", true).await;
    e.findings(
        "update",
        "update",
        json!([{"key":"offline","expected_revision":1,"evidence":"node/b"}]),
    )
    .await
    .unwrap();
    assert_eq!(
        e.findings("stale-clear", "stale", json!([]))
            .await
            .unwrap()
            .outcome,
        "conflict"
    );
    // Supplying a new revision cannot refresh old absence evidence.
    assert_eq!(
        e.findings(
            "stale-present",
            "stale",
            json!([{"key":"offline","expected_revision":2,"evidence":"node/c"}])
        )
        .await
        .unwrap()
        .outcome,
        "conflict"
    );
    let forged=e.intent("forged","findings.reconcile",json!({"partition":"workers","observation":"missing","evaluation":"fake","findings":[],"reminder_ms":0,"complete":true}));
    assert_eq!(
        e.s.host_dispatch(&e.owner, e.grant, 1, &forged).await,
        Err(Error::Invalid)
    );
    assert_eq!(
        e.findings("missing", "missing", json!([]))
            .await
            .unwrap()
            .outcome,
        "refused"
    );
    e.scan("expires", true).await;
    e.f.time
        .fetch_add(60_001, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        e.findings("expired", "expires", json!([]))
            .await
            .unwrap()
            .outcome,
        "refused"
    );
}
#[tokio::test]
async fn calendar_dst_and_misfire_policies_keep_local_dates() {
    let e = setup().await;
    fn at(e: &Env, s: &str) {
        e.f.time.store(
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .timestamp_millis(),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    let date = |s: &str| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
    let gap = DailySchedule {
        schedule_id: Uuid::new_v4(),
        timezone: "America/New_York".into(),
        hour: 2,
        minute: 30,
        start_date: date("2026-03-08"),
        misfire: Misfire::CoalesceOnce,
    };
    at(&e, "2026-03-08T07:29:00Z");
    e.s.put_schedule(&e.owner, gap, 0).await.unwrap();
    assert_eq!(e.s.collect_schedule_events(&e.owner, 2).await.unwrap(), 0);
    at(&e, "2026-03-08T07:30:00Z");
    assert_eq!(e.s.collect_schedule_events(&e.owner, 2).await.unwrap(), 1);
    assert_eq!(
        e.s.outbox(&e.owner, 0, 8).await.unwrap()[0]
            .2
            .occurred_at_ms,
        Some(1772955000000)
    );
    let fold = DailySchedule {
        schedule_id: Uuid::new_v4(),
        timezone: "America/New_York".into(),
        hour: 1,
        minute: 30,
        start_date: date("2026-11-01"),
        misfire: Misfire::CoalesceOnce,
    };
    e.s.put_schedule(&e.owner, fold, 0).await.unwrap();
    at(&e, "2026-11-01T05:30:00Z");
    e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
    let before = e.s.outbox(&e.owner, 0, 8).await.unwrap().len();
    at(&e, "2026-11-01T06:30:00Z");
    e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
    assert_eq!(e.s.outbox(&e.owner, 0, 8).await.unwrap().len(), before + 1);
    at(&e, "2026-11-01T05:30:00Z");
    assert_eq!(
        e.s.collect_schedule_events(&e.owner, 2).await.unwrap(),
        0,
        "rollback never refires"
    );
    // A separate namespace isolates three backlog policies at one synthetic time.
    let e = setup().await;
    at(&e, "2026-11-03T12:00:00Z");
    for policy in [
        Misfire::Skip,
        Misfire::CoalesceOnce,
        Misfire::CatchUp { limit: 2 },
    ] {
        e.s.put_schedule(
            &e.owner,
            DailySchedule {
                schedule_id: Uuid::new_v4(),
                timezone: "UTC".into(),
                hour: 0,
                minute: 0,
                start_date: date("2026-11-01"),
                misfire: policy,
            },
            0,
        )
        .await
        .unwrap();
    }
    e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
    e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
    e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
    assert_eq!(
        e.s.outbox(&e.owner, 0, 8).await.unwrap().len(),
        4,
        "skip=0, coalesce=1, catch-up=3"
    );
}

#[tokio::test]
async fn monitoring_driver_delivers_schedule_and_restarts_without_another_root() {
    use aq_server::playbooks::driver::Driver;
    let e = setup().await;
    e.s.retire_runtime(&e.owner).await.unwrap();
    let driver = Driver::start(
        e.s.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(&e),
    )
    .await
    .unwrap();
    let date = chrono::DateTime::from_timestamp_millis(common::profiled::NOW as i64)
        .unwrap()
        .date_naive();
    driver
        .put_schedule(
            DailySchedule {
                schedule_id: Uuid::new_v4(),
                timezone: "UTC".into(),
                hour: 0,
                minute: 0,
                start_date: date,
                misfire: Misfire::CoalesceOnce,
            },
            0,
        )
        .await
        .unwrap();
    driver.quantum().await.unwrap(); // no declared consumer => retain cursor
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM playbook_outbox")
            .fetch_one(&e.f.db.pool().await)
            .await
            .unwrap(),
        0
    );
    let event = CatalogProfile::Monitoring6.registry().events["aq.signal.reported"].clone();
    let a = artifact(event, true);
    driver.install(a.clone()).await.unwrap();
    driver
        .activate("daily".into(), a.digest.clone(), e.grant, 1)
        .await
        .unwrap();
    driver.quantum().await.unwrap();
    driver.quantum().await.unwrap();
    assert_eq!(
        driver
            .inspect_runs(e.f.actor, Default::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    driver.stop().await.unwrap();
    let driver = Driver::start(
        e.s.clone(),
        aq_playbook_host::Limits::default(),
        protected_factory(&e),
    )
    .await
    .unwrap();
    driver.install(a.clone()).await.unwrap();
    driver.quantum().await.unwrap();
    assert_eq!(
        driver
            .inspect_runs(e.f.actor, Default::default())
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    driver.stop().await.unwrap();
}

fn protected_factory(
    e: &Env,
) -> impl FnOnce() -> std::result::Result<
    aq_playbook_host::postgres_storage::PostgresStorage,
    aq_playbook_host::Error,
> + Send
+ 'static {
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(e.f.db.options().get_database().unwrap());
    move || {
        let client = url
            .as_str()
            .parse::<aq_playbook_host::postgres_client::Config>()
            .unwrap()
            .connect(aq_playbook_host::postgres_client::NoTls)
            .map_err(|_| aq_playbook_host::Error::Unknown)?;
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "d8",
            BTreeMap::from([("d8".into(), [11; 32])]),
        )
        .unwrap();
        aq_playbook_host::protected::storage(
            client,
            keys,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_millis(100),
        )
    }
}

#[tokio::test]
async fn monitoring_backpressure_does_not_block_retained_events_or_runs() {
    use aq_server::playbooks::driver::Driver;
    for refusal in ["schedule", "notification_capacity", "notification_scope"] {
        let e = setup().await;
        let pool = e.f.db.pool().await;
        let date = chrono::DateTime::from_timestamp_millis(common::profiled::NOW as i64)
            .unwrap()
            .date_naive();
        let schedule = DailySchedule {
            schedule_id: Uuid::new_v4(),
            timezone: "UTC".into(),
            hour: 0,
            minute: 0,
            start_date: date,
            misfire: Misfire::CoalesceOnce,
        };
        e.s.put_schedule(&e.owner, schedule.clone(), 0)
            .await
            .unwrap();
        e.s.collect_schedule_events(&e.owner, 2).await.unwrap();
        if refusal == "schedule" {
            e.s.put_schedule(
                &e.owner,
                DailySchedule {
                    schedule_id: Uuid::new_v4(),
                    ..schedule
                },
                0,
            )
            .await
            .unwrap();
            sqlx::query("UPDATE playbook_namespaces SET outbox_count=16384 WHERE namespace_id=$1")
                .bind(e.s.namespace.namespace_id)
                .execute(&pool)
                .await
                .unwrap();
        } else {
            e.scan("blocked-notification", false).await;
            e.findings(
                "blocked-notification",
                "blocked-notification",
                json!([{"key":"offline","expected_revision":0,"evidence":"node/a"}]),
            )
            .await
            .unwrap();
            if refusal == "notification_capacity" {
                sqlx::query("UPDATE notification_budget SET message_count=4096")
                    .execute(&pool)
                    .await
                    .unwrap();
            } else {
                sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false'::jsonb)").execute(&pool).await.unwrap();
            }
        }
        e.s.retire_runtime(&e.owner).await.unwrap();
        let driver = Driver::start(
            e.s.clone(),
            aq_playbook_host::Limits::default(),
            protected_factory(&e),
        )
        .await
        .unwrap();
        let a = artifact(
            CatalogProfile::Monitoring6.registry().events["aq.signal.reported"].clone(),
            true,
        );
        driver.install(a.clone()).await.unwrap();
        driver
            .activate("daily".into(), a.digest, e.grant, 1)
            .await
            .unwrap();
        assert_eq!(driver.quantum().await, Err(Error::Overloaded), "{refusal}");
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM playbook_outbox ORDER BY sequence LIMIT 1"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            "engine_accepted",
            "{refusal}: committed event must drain"
        );
        let runs = driver
            .inspect_runs(e.f.actor, Default::default())
            .await
            .unwrap();
        assert_eq!(
            runs.items.len(),
            1,
            "{refusal}: retained event must create one root"
        );
        assert_eq!(
            runs.items[0].status, "completed",
            "{refusal}: accepted run must tick"
        );
        sqlx::query("UPDATE playbook_namespaces SET outbox_count=1 WHERE namespace_id=$1")
            .bind(e.s.namespace.namespace_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE notification_budget SET message_count=0")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','true'::jsonb)").execute(&pool).await.unwrap();
        driver.quantum().await.unwrap();
        if refusal == "schedule" {
            assert_eq!(
                driver
                    .inspect_runs(e.f.actor, Default::default())
                    .await
                    .unwrap()
                    .items
                    .len(),
                2
            );
        } else {
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notification_messages")
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                1
            );
        }
        driver.stop().await.unwrap();
    }
}

#[tokio::test]
async fn finding_notifications_enqueue_in_episode_order() {
    let e = setup().await;
    let key = (0..128)
        .map(|n| format!("order-{n}"))
        .find(|key| {
            let id = |r: i64| {
                stable_id(
                    "aq.finding.notification",
                    e.s.namespace.namespace_id,
                    1,
                    &digest(&("workers", key, r)).unwrap(),
                )
                .unwrap()
            };
            id(2) < id(1)
        })
        .unwrap();
    e.scan("open-order", false).await;
    e.findings(
        "open-order",
        "open-order",
        json!([{"key":key,"expected_revision":0,"evidence":"node/a"}]),
    )
    .await
    .unwrap();
    e.scan("clear-order", true).await;
    e.findings("clear-order", "clear-order", json!([]))
        .await
        .unwrap();
    e.s.flush_finding_notifications(&e.owner, 8).await.unwrap();
    let summaries: Vec<String> = sqlx::query_scalar(
        "SELECT definition->>'summary' FROM notification_messages ORDER BY sequence",
    )
    .fetch_all(&e.f.db.pool().await)
    .await
    .unwrap();
    assert!(summaries[0].contains("needs attention"));
    assert!(summaries[1].contains("cleared"));
}

#[tokio::test]
async fn different_producers_with_same_event_uuid_acknowledge_independently() {
    let e = setup().await;
    let id = Uuid::new_v4();
    let operation = Uuid::new_v4();
    let registry = CatalogProfile::Monitoring6.registry();
    let engine = Engine::new(MemoryStorage::default(), "producer-identity", "owner").unwrap();
    for (producer, name, payload, subject, revision) in [
        (
            "aq.graph.operations@1",
            "aq.graph.operation.confirmed",
            json!({"operation_id":operation,"task_id":Uuid::new_v4(),"attempt_id":Uuid::new_v4()}),
            None,
            None,
        ),
        (
            "aq.host.bridge@1",
            "aq.signal.reported",
            json!({"outcome":"reported","reference":"aq.schedule/test","subject":{"id":operation,"revision":1}}),
            Some(operation.to_string()),
            Some(1),
        ),
    ] {
        let contract = registry.events[name].clone();
        engine.install(artifact(contract.clone(), false)).unwrap();
        let event = EventEnvelope {
            producer: producer.into(),
            event_id: id.to_string(),
            contract: ContractRef {
                name: name.into(),
                fingerprint: contract.fingerprint().unwrap(),
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), operation.to_string())]),
            subject,
            revision,
            causation: None,
            occurred_at_ms: None,
            observed_at_ms: Some(common::profiled::NOW as i64),
            payload,
        };
        validate_event(&event, &registry).unwrap();
        let hash = digest(&event).unwrap();
        let aad = canonical_bytes(&(
            "aq.playbooks.outbox",
            1,
            e.s.namespace.namespace_id,
            1_i64,
            id,
            &hash,
        ))
        .unwrap();
        let sealed = e
            .keys
            .seal(&aad, &canonical_bytes(&event).unwrap())
            .unwrap();
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,1,$2,0,$3,$4,$5,$6,$7,$8,$9,'pending')")
            .bind(e.s.namespace.namespace_id).bind(producer).bind(id).bind(hash).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(common::profiled::NOW as i64).execute(&e.f.db.pool().await).await.unwrap();
    }
    let rows = e.s.outbox(&e.owner, 0, 2).await.unwrap();
    assert_eq!(rows.len(), 2);
    let receipts: Vec<_> = rows
        .iter()
        .map(|(_, _, event)| {
            engine
                .admit_event(&event.producer, event.clone(), 1)
                .unwrap()
        })
        .collect();
    assert_ne!(receipts[0].key, receipts[1].key);
    e.s.acknowledge_event(&e.owner, id, receipts[1].clone(), true)
        .await
        .unwrap();
    let pending = e.s.outbox(&e.owner, 0, 2).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].2.producer, "aq.graph.operations@1");
    e.s.acknowledge_event(&e.owner, id, receipts[0].clone(), true)
        .await
        .unwrap();
    assert!(e.s.outbox(&e.owner, 0, 2).await.unwrap().is_empty());
}
