//! Catalog profile 7 work.ensure completion on disposable PostgreSQL through
//! the production service owners. The graph port is an in-memory fake; a
//! test flips the fake task's state the way the task lifecycle would.
mod common;
use aq_playbook_host::runner::{Intent, IntentStatus};
use aq_server::{
    playbooks::{
        domain::*,
        host_adapters::{HostResult, WorkGraph},
        review_domain::{CatalogProfile, HOST7_CAPABILITIES},
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

/// In-memory graph: creates nodes at version 1; replays by idempotency key.
#[derive(Default)]
struct FakeGraph {
    nodes: Mutex<BTreeMap<Uuid, Value>>,
    replies: Mutex<BTreeMap<String, (u16, Value)>>,
}
impl FakeGraph {
    /// The task lifecycle (outside the playbook) moves the node.
    fn set(&self, id: Uuid, field: &str, value: Value) {
        let mut nodes = self.nodes.lock().unwrap();
        let node = nodes.get_mut(&id).unwrap();
        node[field] = value;
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
        if let Some(reply) = self.replies.lock().unwrap().get(&key) {
            return Ok(reply.clone());
        }
        let mut results = vec![];
        for op in body["operations"].as_array().unwrap() {
            match op["op"].as_str().unwrap() {
                "create_node" => {
                    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
                    let mut node = op.clone();
                    node["version"] = json!(1);
                    self.nodes.lock().unwrap().insert(id, node.clone());
                    results.push(node);
                }
                "add_edge" => results.push(op.clone()),
                other => panic!("unexpected op {other}"),
            }
        }
        let reply = (
            200,
            json!({"change_set_id": Uuid::new_v4(), "results": results}),
        );
        self.replies.lock().unwrap().insert(key, reply.clone());
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
async fn host(f: &common::profiled::Fixture) -> Host {
    let l = &f.selection.lineage;
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
    let service = Service::new_host7(
        &f.state,
        Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: format!("fixture-{}", Uuid::new_v4()),
            workspace_id: l.workspace_id,
            project_id: l.project_id,
            owner_user_id: f.actor,
        },
        Arc::new(keys),
        graph.clone(),
        None,
        None,
    )
    .await
    .unwrap();
    service.initialize(f.actor).await.unwrap();
    let now = (f.state.clock)().timestamp_millis();
    let scope = GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(HOST7_CAPABILITIES.map(String::from)),
    };
    let grant = Uuid::new_v4();
    service
        .grant(f.actor, grant, scope.clone(), now + 10_000)
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
            command: CatalogProfile::Host7.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: CatalogProfile::Host7
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
    async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
        self.service
            .host_dispatch(&self.runtime, self.grant, 1, &self.intent(id, name, input))
            .await
            .unwrap()
    }
    async fn get(&self, handle: &str) -> HostResult {
        self.run(
            &format!("get-{}", Uuid::new_v4()),
            "operation.get",
            json!({"handle": handle}),
        )
        .await
    }
    async fn collect(&self) -> usize {
        self.service
            .collect_work_completions(&self.runtime, 2)
            .await
            .unwrap()
    }
    /// Every outbox event's (contract, payload), in order.
    async fn events(&self) -> Vec<(String, Value)> {
        self.service
            .outbox(&self.runtime, 0, 16)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, _, e)| (e.contract.name, e.payload))
            .collect()
    }
    /// The ensured task id behind an accepted work handle.
    fn task(&self, key: &str) -> Uuid {
        let nodes = self.graph.nodes.lock().unwrap();
        *nodes
            .iter()
            .find(|(_, n)| n["metadata"]["aq:logical_key"] == key)
            .unwrap()
            .0
    }
}
fn ensure_input(key: &str, kind: &str) -> Value {
    json!({"context": "ctx", "kind": kind, "lifetime": "independent",
        "logical_key": key, "subject": {"id": "spec-1", "revision": 0}, "visibility": "project"})
}

#[tokio::test]
async fn host7_work_ensure_completes_once_with_operation_updated() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let h = host(&f).await;
    let ensure = h
        .run(
            "ensure-1",
            "work.ensure",
            ensure_input("feature-x", "implementation"),
        )
        .await;
    assert_eq!(ensure.outcome, "accepted", "{ensure:?}");
    let handle = ensure.output["handle"].as_str().unwrap().to_string();
    let task = h.task("feature-x");
    // The ensured node is announced; no completion while the task is open.
    let events = h.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "aq.work.changed");
    assert_eq!(events[0].1["outcome"], "ensured");
    assert_eq!(h.collect().await, 0);
    assert_eq!(h.get(&handle).await.outcome, "running");

    h.graph.set(task, "task_state", json!("completed"));
    assert_eq!(h.collect().await, 1);
    let events = h.events().await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].0, "aq.operation.updated");
    let completion = json!({"outcome": "completed", "output": {
        "disposition": "accepted",
        "reference": format!("aq.playbook.work/{task}"),
        "subject": {"id": task.to_string(), "revision": 2}}});
    assert_eq!(events[1].1, completion);

    // Idempotent: further passes, reconciliation and a replayed dispatch
    // observe the retained completion and seal nothing new.
    h.graph.set(task, "task_state", json!("canceled"));
    assert_eq!(h.collect().await, 0);
    let get = h.get(&handle).await;
    assert_eq!(get.outcome, "completed");
    assert_eq!(get.output, completion);
    let replay = h
        .run(
            "ensure-1",
            "work.ensure",
            ensure_input("feature-x", "implementation"),
        )
        .await;
    assert_eq!(replay, ensure);
    assert_eq!(h.events().await.len(), 2);
}

#[tokio::test]
async fn host7_work_completion_never_infers_review_acceptance() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let h = host(&f).await;
    let mut handles = BTreeMap::new();
    for key in ["rejected", "silent", "canceled"] {
        let r = h.run(key, "work.ensure", ensure_input(key, "review")).await;
        assert_eq!(r.outcome, "accepted", "{r:?}");
        handles.insert(key, r.output["handle"].as_str().unwrap().to_string());
    }
    let rejected = h.task("rejected");
    h.graph.set(
        rejected,
        "metadata",
        json!({"aq:logical_key": "rejected", "aq:review_disposition": "changes_requested"}),
    );
    h.graph.set(rejected, "task_state", json!("completed"));
    h.graph
        .set(h.task("silent"), "task_state", json!("completed"));
    h.graph
        .set(h.task("canceled"), "task_state", json!("canceled"));
    assert_eq!(h.collect().await + h.collect().await, 3);
    let seen: Vec<_> = ["rejected", "silent", "canceled"]
        .iter()
        .map(|k| handles[k].clone())
        .collect();
    let mut outcomes = vec![];
    for handle in &seen {
        let r = h.get(handle).await;
        assert_eq!(r.outcome, "completed");
        outcomes.push((
            r.output["outcome"].as_str().unwrap().to_string(),
            r.output["output"]["disposition"]
                .as_str()
                .unwrap()
                .to_string(),
        ));
    }
    assert_eq!(
        outcomes,
        [
            ("completed".into(), "changes_requested".into()),
            ("completed".into(), "failed".into()),
            ("refused".into(), "failed".into()),
        ]
    );
    let updates = h
        .events()
        .await
        .into_iter()
        .filter(|(c, _)| c == "aq.operation.updated")
        .count();
    assert_eq!(updates, 3, "exactly one completion per work item");
}
