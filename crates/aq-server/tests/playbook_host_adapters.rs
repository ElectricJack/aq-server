//! D5 host adapters on disposable PostgreSQL through the production service
//! owners. The graph port is an in-memory CAS fake (no real graph network);
//! operation.cancel reaches the actual B3 native cancellation owner.
mod common;
use aq_playbook_host::runner::{Intent, IntentStatus};
use aq_server::{
    playbooks::{
        domain::*,
        host_adapters::{HostResult, WorkGraph},
        review_domain::CatalogProfile,
        service::*,
    },
    protected_storage::{KeyFile, Keyring, ProtectionConfig},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// Retained graph reply per idempotency key: (request body, (status, body)).
type Replay = (Value, (u16, Value));

/// In-memory graph with node CAS versions and idempotency-key replay.
#[derive(Default)]
struct FakeGraph {
    nodes: Mutex<BTreeMap<Uuid, Value>>,
    replies: Mutex<BTreeMap<String, Replay>>,
    mutations: Mutex<usize>,
}
impl FakeGraph {
    fn mutations(&self) -> usize {
        *self.mutations.lock().unwrap()
    }
    fn version(&self, id: Uuid) -> i64 {
        self.nodes.lock().unwrap()[&id]["version"].as_i64().unwrap()
    }
    /// Concurrent writer outside the playbook: bumps the CAS version.
    fn touch(&self, id: Uuid) {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&id).unwrap();
        node["version"] = json!(node["version"].as_i64().unwrap() + 1);
    }
}
#[async_trait::async_trait]
impl WorkGraph for FakeGraph {
    async fn node(&self, _: Uuid, id: Uuid) -> Result<Option<Value>> {
        Ok(self.nodes.lock().unwrap().get(&id).cloned())
    }
    async fn mutate(&self, _: Uuid, body: Value) -> Result<(u16, Value)> {
        let key = body["idempotency_key"].as_str().unwrap().to_string();
        if let Some((old, reply)) = self.replies.lock().unwrap().get(&key) {
            return Ok(if *old == body {
                reply.clone()
            } else {
                (409, json!({}))
            });
        }
        *self.mutations.lock().unwrap() += 1;
        let mut nodes = self.nodes.lock().unwrap();
        let mut staged = nodes.clone();
        let mut results = vec![];
        for op in body["operations"].as_array().unwrap() {
            match op["op"].as_str().unwrap() {
                "create_node" => {
                    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
                    if staged.contains_key(&id) {
                        return Ok((409, json!({})));
                    }
                    let mut node = op.clone();
                    node["version"] = json!(1);
                    staged.insert(id, node.clone());
                    results.push(node);
                }
                "update_node" => {
                    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
                    let Some(node) = staged.get_mut(&id) else {
                        return Ok((404, json!({})));
                    };
                    if node["version"] != op["expected_version"] {
                        return Ok((409, json!({})));
                    }
                    assert!(
                        op["patch"].get("task_state").is_none(),
                        "no lifecycle shortcut"
                    );
                    for (k, v) in op["patch"].as_object().unwrap() {
                        node[k] = v.clone();
                    }
                    node["version"] = json!(node["version"].as_i64().unwrap() + 1);
                    results.push(node.clone());
                }
                "add_edge" => results.push(op.clone()),
                other => panic!("unexpected op {other}"),
            }
        }
        *nodes = staged;
        let reply = (
            200,
            json!({"change_set_id": Uuid::new_v4(), "results": results}),
        );
        self.replies
            .lock()
            .unwrap()
            .insert(key, (body, reply.clone()));
        Ok(reply)
    }
}

struct Host {
    service: Service,
    runtime: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
    now: i64,
    graph: Arc<FakeGraph>,
}
async fn host(state: &aq_server::AppState, actor: Uuid, workspace: Uuid, project: Uuid) -> Host {
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
    let graph = Arc::new(FakeGraph::default());
    let service = Service::new_host5(
        state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("fixture-{}", Uuid::new_v4()),
            workspace_id: workspace,
            project_id: project,
            owner_user_id: actor,
        },
        Arc::new(keys),
        graph.clone(),
    )
    .await
    .unwrap();
    service.initialize(actor).await.unwrap();
    let now = (state.clock)().timestamp_millis();
    let scope = GrantScope {
        workspace_id: workspace,
        project_id: project,
        recipients: vec![],
        capabilities: BTreeSet::from(
            [
                "context.read",
                "work.ensure",
                "work.change",
                "operation.cancel",
            ]
            .map(String::from),
        ),
    };
    let grant = Uuid::new_v4();
    service
        .grant(actor, grant, scope.clone(), now + 10_000)
        .await
        .unwrap();
    let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
    Host {
        service,
        runtime,
        grant,
        scope,
        now,
        graph,
    }
}
impl Host {
    fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
        Intent {
            id: id.into(),
            run: "synthetic-run".into(),
            step: name.replace('.', "-"),
            invocation: 1,
            deadline_at_ms: self.now + 5000,
            command: CatalogProfile::Host5.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: CatalogProfile::Host5
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
    async fn run(&self, intent: &Intent) -> Result<HostResult> {
        self.service
            .host_dispatch(&self.runtime, self.grant, 1, intent)
            .await
    }
}
async fn profiled_host() -> (common::profiled::Fixture, Host) {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let l = &f.selection.lineage;
    let h = host(&f.state, f.actor, l.workspace_id, l.project_id).await;
    (f, h)
}
fn ensure_input(key: &str, context: &str) -> Value {
    json!({"context": context, "kind": "implementation", "lifetime": "independent",
        "logical_key": key, "subject": {"id": "spec-1", "revision": 0}, "visibility": "project"})
}
async fn outbox(pool: &sqlx::PgPool, namespace: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM playbook_outbox WHERE namespace_id=$1")
        .bind(namespace)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn work_ensure_is_idempotent_by_key() {
    let (f, h) = profiled_host().await;
    let pool = f.db.pool().await;
    let first = h
        .run(&h.intent("ensure-1", "work.ensure", ensure_input("feature-x", "ctx")))
        .await
        .unwrap();
    assert_eq!(first.outcome, "ensured", "ensured is not completion");
    let task = first.output["subject"]["id"].as_str().unwrap().to_string();
    assert_eq!(first.output["subject"]["revision"], 1);
    assert_eq!(
        first.output["reference"],
        format!("aq.playbook.work/{task}")
    );
    assert_eq!(h.graph.mutations(), 1);
    // A second intent with the same logical key ensures the same work.
    let second = h
        .run(&h.intent("ensure-2", "work.ensure", ensure_input("feature-x", "ctx")))
        .await
        .unwrap();
    assert_eq!(second.output["subject"]["id"], json!(task));
    assert_eq!(h.graph.mutations(), 1, "logical key is idempotent");
    // Replay of one intent returns the retained result and performs nothing.
    let replay = h
        .run(&h.intent("ensure-1", "work.ensure", ensure_input("feature-x", "ctx")))
        .await
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(
        h.service
            .host_retained(
                h.grant,
                1,
                &h.intent("ensure-1", "work.ensure", ensure_input("feature-x", "ctx"))
            )
            .await
            .unwrap(),
        Some(first.clone())
    );
    // Same intent id, different bytes: conflict.
    assert!(matches!(
        h.run(&h.intent("ensure-1", "work.ensure", ensure_input("feature-y", "ctx")))
            .await,
        Err(Error::Conflict)
    ));
    // Same logical key, different declared work: conflict, never a re-point.
    assert!(matches!(
        h.run(&h.intent(
            "ensure-3",
            "work.ensure",
            ensure_input("feature-x", "other")
        ))
        .await,
        Err(Error::Conflict)
    ));
    assert_eq!(h.graph.mutations(), 1);
    // Owner admission and the outbox event committed together per ensured intent.
    assert_eq!(outbox(&pool, h.service.namespace.namespace_id).await, 2);
    // Undeclared kinds never reach an adapter.
    let mut bad = ensure_input("feature-z", "ctx");
    bad["kind"] = json!("deploy");
    assert!(matches!(
        h.run(&h.intent("ensure-4", "work.ensure", bad)).await,
        Err(Error::Invalid)
    ));
}

#[tokio::test]
async fn work_change_conflict_returns_version_refusal() {
    let (f, h) = profiled_host().await;
    let pool = f.db.pool().await;
    let ensured = h
        .run(&h.intent("ensure", "work.ensure", ensure_input("feature-c", "ctx")))
        .await
        .unwrap();
    let task = ensured.output["subject"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id: Uuid = task.parse().unwrap();
    h.graph.touch(id);
    let before = h.graph.mutations();
    let change = |intent: &str, revision: i64, variant: &str, value: &str| {
        h.intent(
            intent,
            "work.change",
            json!({"hold_id": null, "subject": {"id": task, "revision": revision},
                "value": value, "variant": variant}),
        )
    };
    // Stale expected version: conflict outcome with the current version.
    let stale = h
        .run(&change("change-stale", 1, "priority", "2"))
        .await
        .unwrap();
    assert_eq!(stale.outcome, "conflict");
    assert_eq!(stale.output["subject"]["revision"], 2);
    assert_eq!(
        h.graph.mutations(),
        before,
        "a stale version performs no mutation"
    );
    // Current expected version: changed, version advances, no lifecycle edit.
    let changed = h
        .run(&change("change-ok", 2, "priority", "2"))
        .await
        .unwrap();
    assert_eq!(changed.outcome, "changed");
    assert_eq!(changed.output["subject"]["revision"], 3);
    assert_eq!(h.graph.version(id), 3);
    assert_eq!(
        h.graph.nodes.lock().unwrap()[&id]["task_state"],
        "active",
        "work.change never shortcuts lifecycle"
    );
    // Unknown subject (not namespace work) is refused, not proxied.
    let foreign = h.intent(
        "change-foreign",
        "work.change",
        json!({"hold_id": null, "subject": {"id": Uuid::new_v4().to_string(), "revision": 1},
                "value": "x", "variant": "requirements"}),
    );
    assert_eq!(h.run(&foreign).await.unwrap().outcome, "refused");
    // Replay of the conflict intent is the retained conflict.
    assert_eq!(
        h.run(&change("change-stale", 1, "priority", "2"))
            .await
            .unwrap(),
        stale
    );
    assert_eq!(outbox(&pool, h.service.namespace.namespace_id).await, 2);
}

#[tokio::test]
async fn operation_cancel_reaches_native_attempt() {
    let (c, offer) = common::native_offer::published().await;
    let attempt = offer.offered.offer.binding.attempt_id;
    let l = &c.env.lineage;
    let h = host(&c.env.env.state, c.env.actor, l.workspace_id, l.project_id).await;
    let cancel = |intent: &str, handle: String| {
        h.intent(intent, "operation.cancel", json!({"handle": handle}))
    };
    let handle = format!("aq.execution.attempt/{attempt}");
    let first = h.run(&cancel("cancel-1", handle.clone())).await.unwrap();
    assert_eq!(first.outcome, "cancelled", "{first:?}");
    assert_eq!(first.output["reference"], json!(handle));
    assert_eq!(first.output["subject"]["id"], json!(attempt.to_string()));
    let (held, status): (bool, String) = sqlx::query_as("SELECT a.holds_task,h.state->>'status' FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(&c.env.env.pool)
        .await
        .unwrap();
    assert!(!held);
    assert_eq!(status, "never_granted");
    let operations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_cancel_operations WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&c.env.env.pool)
            .await
            .unwrap();
    assert_eq!(operations, 1);
    // Replay is the retained result; no second B3 operation.
    assert_eq!(
        h.run(&cancel("cancel-1", handle.clone())).await.unwrap(),
        first
    );
    // A second cancel intent for the same attempt reports the settled state.
    assert_eq!(
        h.run(&cancel("cancel-2", handle.clone()))
            .await
            .unwrap()
            .outcome,
        "cancelled"
    );
    // Only server-owned handles dispatch; anything else is refused.
    for (i, other) in [
        format!("aq.execution.attempt/{}", Uuid::new_v4()),
        "https://example.invalid/cancel".to_string(),
        format!("aq.playbook.work/{}", Uuid::new_v4()),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            h.run(&cancel(&format!("cancel-x{i}"), other))
                .await
                .unwrap()
                .outcome,
            "refused"
        );
    }
}

/// smart-vault-20: the vendored D4 fixtures run through the production
/// profile-3 registry the host adapters validate against.
#[test]
fn host3_registry_runs_vendored_d4_fixtures() {
    let registry = CatalogProfile::Host5.registry();
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../vendor/aq-host-catalog-1.0.0/fixtures.json"
    ))
    .unwrap();
    let cases = fixtures["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 19);
    for case in cases {
        let name = case["contract"].as_str().unwrap();
        let schema = match case["kind"].as_str().unwrap() {
            "input" => &registry.commands[name].input,
            "output" => {
                match registry.commands[name]
                    .outcomes
                    .get(case["outcome"].as_str().unwrap())
                {
                    Some(schema) => schema,
                    None => {
                        assert!(!case["valid"].as_bool().unwrap(), "{case}");
                        continue;
                    }
                }
            }
            "event" => &registry.events[name].payload,
            other => panic!("unknown fixture kind {other}"),
        };
        assert_eq!(
            schema.validate(&case["value"]).is_ok(),
            case["valid"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
    }
    for replay in fixtures["replay_cases"].as_array().unwrap() {
        let command = &registry.commands[replay["command"].as_str().unwrap()];
        command.input.validate(&replay["input"]).unwrap();
        command
            .input
            .validate(&replay["conflicting_input"])
            .unwrap();
        // Asynchronous replay cases expect an admission handle, not an outcome.
        let outcome = replay["expected_outcome"].as_str().unwrap_or("accepted");
        assert!(command.outcomes.contains_key(outcome), "{replay}");
    }
}
