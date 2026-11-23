use super::adapters::{FakeGraph, FakeRouting};
use crate::common;
use aq_playbook_host::{
    model::digest,
    runner::{Intent, IntentStatus},
};
use aq_role_settings::delivery::DeliveryLimits;
use aq_routing::classify::Options;
use aq_server::playbooks::{
    domain::{
        GrantScope, Result,
        delivery_decisions::Settled,
        delivery_store::{DeliveryPort, Latest, Pending},
    },
    host_adapters::DeliverySettlement,
    host_adapters::HostResult,
    review_domain::{
        CatalogProfile, DELIVERY_COMMANDS, HOST9_CAPABILITIES, HOST10_CAPABILITIES,
        HOST10_DELIVERY_CAPABILITIES, HOST10_DELIVERY_COMMANDS, HOST10_FINGERPRINT, HOST10_VERSION,
    },
    service::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, atomic::Ordering},
};
use uuid::Uuid;

/// Everything a Host10 grant may name.
fn host10_capabilities() -> BTreeSet<String> {
    HOST10_CAPABILITIES
        .iter()
        .chain(&HOST10_DELIVERY_CAPABILITIES)
        .map(|c| c.to_string())
        .collect()
}

#[test]
fn host10_is_the_pinned_catalog_and_adds_the_event_and_the_result_facts() {
    let registry = CatalogProfile::Host10.registry();
    let manifest: Value = serde_json::from_str(include_str!(
        "../../../../../vendor/aq-host-catalog-2.3.0/manifest.json"
    ))
    .unwrap();
    assert_eq!(manifest["version"], HOST10_VERSION);
    assert_eq!(manifest["fingerprint"], HOST10_FINGERPRINT);
    assert_eq!(
        manifest["supersedes"]["fingerprint"],
        super::HOST9_FINGERPRINT
    );
    assert_eq!(
        digest(&(HOST10_VERSION, &registry)).unwrap(),
        HOST10_FINGERPRINT
    );
    let host10 = CatalogProfile::Host10;
    assert_eq!((host10.number(), host10.config_name()), (10, "host_v10"));
    assert_eq!(CatalogProfile::from_number(10), Some(host10));
    assert!(host10.is_host2() && host10.has_routing() && host10.is_delivery());
    assert!(!CatalogProfile::Host9.is_delivery());
    assert!(registry.events.contains_key("aq.work.result_ready"));
    assert_eq!(registry.commands["work.read"].version, "2.3.0");
    // Only work.read changed from 2.2.0; the routing contracts are Host9's.
    let host9 = CatalogProfile::Host9.registry();
    for (name, contract) in &host9.commands {
        if name != "work.read" {
            assert_eq!(registry.commands.get(name), Some(contract), "{name}");
        }
    }
    for (name, contract) in &host9.events {
        assert_eq!(registry.events.get(name), Some(contract), "{name}");
    }
    // The Host9 grant scope is admitted, and the served commands are Host9's
    // names and the six asynchronous delivery commands.
    let scope = GrantScope {
        workspace_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        recipients: vec![],
        capabilities: HOST9_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
    };
    assert!(host10.validate_scope(&scope).is_ok());
    // Host10 serves Host9's commands and the delivery commands implemented so far.
    let served: BTreeSet<&str> = host10.commands().iter().copied().collect();
    let mut expected: BTreeSet<&str> = CatalogProfile::Host9.commands().iter().copied().collect();
    expected.extend(HOST10_DELIVERY_COMMANDS);
    expected.extend(DELIVERY_COMMANDS);
    assert_eq!(served, expected);
    for name in HOST10_DELIVERY_COMMANDS {
        let contract = &registry.commands[name];
        assert_eq!(contract.capabilities, BTreeSet::from([name.to_string()]));
    }
    for name in DELIVERY_COMMANDS {
        let contract = &registry.commands[name];
        assert!(contract.asynchronous.is_some(), "{name}");
        assert!(contract.capabilities.contains(name), "{name}");
    }
    // Their capabilities are admitted in a Host10 grant and not in a Host9 one.
    let delivery = GrantScope {
        capabilities: host10_capabilities(),
        ..scope.clone()
    };
    assert!(host10.validate_scope(&delivery).is_ok());
    assert!(CatalogProfile::Host9.validate_scope(&delivery).is_err());
    // A Host9 namespace is not reused.
    assert!(host10.namespace_refusal(1, Some((9, 1))).is_some());
    // The pin lists the vendored files byte for byte.
    let pin: Value = serde_json::from_str(include_str!(
        "../../../../../vendor/aq-host-catalog-2.3.0-source.json"
    ))
    .unwrap();
    super::fixture::assert_pinned_files(&pin, HOST10_FINGERPRINT, "aq-host-catalog-2.3.0");
}

/// What the server records of results: pending results, the latest result
/// of a task and which projects it runs.
#[derive(Default)]
struct Port {
    pending: Mutex<Vec<Pending>>,
    latest: Mutex<BTreeMap<Uuid, Latest>>,
    bound: Mutex<BTreeSet<Uuid>>,
    /// Every (task, result) recorded, the latest and earlier ones.
    known: Mutex<BTreeSet<(Uuid, Uuid)>>,
}
#[async_trait::async_trait]
impl DeliveryPort for Port {
    async fn pending(&self, project: Option<Uuid>, limit: u32) -> Result<Vec<Pending>> {
        let all = self.pending.lock().unwrap();
        let of = |p: &&Pending| project.is_none_or(|id| id == p.project_id);
        Ok(all
            .iter()
            .filter(of)
            .take(limit as usize)
            .copied()
            .collect())
    }
    async fn latest(&self, task: Uuid) -> Result<Option<Latest>> {
        Ok(self.latest.lock().unwrap().get(&task).cloned())
    }
    async fn bound(&self, project: Uuid) -> Result<bool> {
        Ok(self.bound.lock().unwrap().contains(&project))
    }
    async fn has_result(&self, task: Uuid, result: Uuid) -> Result<bool> {
        Ok(self.known.lock().unwrap().contains(&(task, result)))
    }
}

/// The server's settlement as `work.complete` and `work.return` call it:
/// answers what the test sets, and counts the calls.
pub(super) struct Settle {
    pub answer: Mutex<Settled>,
    /// (`complete` or the return reason, result) per call.
    pub calls: Mutex<Vec<(String, Uuid)>>,
}
impl Default for Settle {
    fn default() -> Self {
        Self {
            answer: Mutex::new(Settled::Completed),
            calls: Mutex::default(),
        }
    }
}
#[async_trait::async_trait]
impl DeliverySettlement for Settle {
    async fn complete(&self, result: Uuid) -> Result<Settled> {
        self.calls.lock().unwrap().push(("complete".into(), result));
        Ok(*self.answer.lock().unwrap())
    }
    async fn send_back(&self, result: Uuid, reason: &str) -> Result<Settled> {
        self.calls.lock().unwrap().push((reason.into(), result));
        Ok(*self.answer.lock().unwrap())
    }
}

struct D {
    f: common::profiled::Fixture,
    service: Service,
    runtime: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
    graph: Arc<FakeGraph>,
    routing: Arc<FakeRouting>,
    port: Arc<Port>,
    settle: Arc<Settle>,
    other_project: Uuid,
}
async fn delivery(installation_routing: bool) -> D {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let l = f.selection.lineage.clone();
    let graph = Arc::new(FakeGraph::default());
    let other_project = Uuid::new_v4();
    let routing = Arc::new(FakeRouting {
        graph: graph.clone(),
        located: Mutex::default(),
        // Nothing is on the automatic router: delivery does not depend on it.
        on_router: Mutex::default(),
        attempts: Mutex::new((1, 2)),
        options: Mutex::new(Options::default()),
        writes: Mutex::default(),
    });
    let port = Arc::new(Port::default());
    let settle = Arc::new(Settle::default());
    port.bound
        .lock()
        .unwrap()
        .extend([l.project_id, other_project]);
    let mut service = Service::new_host2(
        &f.state,
        super::fixture::namespace(&f, "host10"),
        super::fixture::keys(),
        CatalogProfile::Host10,
        graph.clone(),
        None,
        None,
        None,
    )
    .await
    .unwrap()
    .with_routing(routing.clone())
    .with_delivery(port.clone())
    .with_settlement(settle.clone());
    service.installation_routing = installation_routing;
    // The tests advance the clock by hours, so use the maximum 24-hour grant.
    let grant =
        super::fixture::Grant::initialize(&f, &service, host10_capabilities(), 86_400_000).await;
    D {
        f,
        service,
        runtime: grant.runtime,
        grant: grant.id,
        scope: grant.scope,
        graph,
        routing,
        port,
        settle,
        other_project,
    }
}

impl D {
    async fn pool(&self) -> sqlx::PgPool {
        self.f.db.pool().await
    }
    fn clock(&self) -> i64 {
        (self.f.state.clock)().timestamp_millis()
    }
    fn later(&self, ms: i64) {
        self.f.time.fetch_add(ms, Ordering::SeqCst);
    }
    async fn read(&self, task: Uuid) -> HostResult {
        let input = json!({"subject": {"id": task.to_string(), "revision": 1}});
        self.run(&format!("read-{}", Uuid::new_v4()), "work.read", input)
            .await
    }
    async fn run(&self, id: &str, name: &str, input: Value) -> HostResult {
        let task = input["task"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok());
        let mut intent = self.intent(id, name, input);
        if DELIVERY_COMMANDS.contains(&name)
            && let Some(task) =
                task.filter(|task| self.port.latest.lock().unwrap().contains_key(task))
        {
            intent.run = self.event_run(task).await;
        }
        let result = self.try_run(&intent).await.unwrap();
        // Every answer is one the catalog declares.
        (intent.command.outcomes[&result.outcome].validate(&result.output)).unwrap();
        result
    }
    async fn event_run(&self, task: Uuid) -> String {
        self.activated().await;
        self.asked().await;
        let (_, id, event) = self
            .service
            .outbox(&self.runtime, 0, 16)
            .await
            .unwrap()
            .into_iter()
            .find(|(_, _, event)| {
                event.contract.name == "aq.work.result_ready"
                    && event.payload["task_id"] == task.to_string()
            })
            .unwrap();
        let key = digest(&(&event.producer, 0_u64, &event.event_id)).unwrap();
        let run = format!("event-{key}");
        self.service
            .acknowledge_event(
                &self.runtime,
                id,
                aq_playbook_host::runner::EventDisposition {
                    key,
                    duplicate: false,
                    destinations: 1,
                },
                false,
            )
            .await
            .unwrap();
        self.service
            .record_delivery_runs(&self.runtime, id, &event, &json!([run]))
            .await
            .unwrap();
        run
    }

    /// Dispatches `intent` (a replay when it was dispatched before).
    async fn try_run(&self, intent: &Intent) -> Result<HostResult> {
        (self.service)
            .host_dispatch(&self.runtime, self.grant, 1, intent)
            .await
    }
    fn intent(&self, id: &str, name: &str, input: Value) -> Intent {
        let registry = CatalogProfile::Host10.registry();
        Intent {
            id: id.into(),
            run: "synthetic-run".into(),
            step: name.replace('.', "-"),
            invocation: 1,
            deadline_at_ms: self.clock() + 5000,
            command: registry.commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: CatalogProfile::Host10
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
            dispatch_sequence: None,
        }
    }
    /// An artifact is active on the delivery binding.
    async fn activated(&self) {
        sqlx::query("INSERT INTO playbook_engine_bindings(namespace_id,generation,binding,artifact,at_ms) VALUES($1,1,'aq.delivery','sha256:delivery-1',1) ON CONFLICT DO NOTHING")
            .bind(self.service.namespace.namespace_id)
            .execute(&self.pool().await)
            .await
            .unwrap();
    }
    async fn limits(&self, limits: DeliveryLimits) {
        let now = self.clock();
        let saved = aq_role_settings::save_delivery_limits(
            aq_role_settings::Writer::new(&self.pool().await, self.f.actor, now)
                .edit(0, Uuid::new_v4()),
            &limits,
        )
        .await;
        saved.unwrap();
    }
    /// A task whose current attempt stopped with a recorded result of `project`.
    fn stopped(&self, project: Uuid, outcome: &str) -> (Uuid, Pending) {
        let task = self.graph.task("Stopped with a result", json!({}));
        let pending = Pending {
            task_id: task,
            workspace_id: self.scope.workspace_id,
            project_id: project,
            attempt_id: Uuid::new_v4(),
            result_id: Uuid::new_v4(),
        };
        let latest = Latest {
            workspace_id: pending.workspace_id,
            project_id: project,
            attempt_id: pending.attempt_id,
            result_id: pending.result_id,
            outcome: outcome.into(),
            changed: true,
            head: "a".repeat(40),
            base: "b".repeat(40),
        };
        self.port.pending.lock().unwrap().push(pending);
        self.port.latest.lock().unwrap().insert(task, latest);
        (self.port.known.lock().unwrap()).insert((task, pending.result_id));
        (task, pending)
    }
    async fn asked(&self) -> usize {
        (self.service.collect_result_ready(&self.runtime, 8).await).unwrap()
    }
    async fn events(&self) -> Vec<Value> {
        let page = self.service.outbox(&self.runtime, 0, 16).await.unwrap();
        page.into_iter()
            .filter(|e| e.2.contract.name == "aq.work.result_ready")
            .map(|e| e.2.payload)
            .collect()
    }
    /// main moved to `head` in `project`.
    async fn main_moved(&self, project: Uuid, head: &str) {
        let sql = "INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,$3) ON CONFLICT(project_id) DO UPDATE SET head=$2,observed_at_ms=$3";
        let query = sqlx::query(sql).bind(project).bind(head).bind(self.clock());
        query.execute(&self.pool().await).await.unwrap();
    }
    /// The task's pull request, as the pull_request commands record it.
    async fn pull_request(&self, task: Uuid, checks: &str, failed: Value) {
        let sql = "INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,updated_at_ms) VALUES($1,7,'https://example.test/pr/7','open','mergeable',$2,$3,$4) ON CONFLICT(task_id) DO UPDATE SET checks_state=$2,checks_failed=$3,updated_at_ms=$4";
        let query = sqlx::query(sql).bind(task).bind(checks).bind(failed);
        query
            .bind(self.clock())
            .execute(&self.pool().await)
            .await
            .unwrap();
    }
    async fn decided(&self, task: Uuid, result: Uuid, decision: &str) {
        let sql = "INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,$3,NULL,$4)";
        let query = sqlx::query(sql).bind(result).bind(task).bind(decision);
        query
            .bind(self.clock())
            .execute(&self.pool().await)
            .await
            .unwrap();
    }
}

// `work.complete`, `work.return`, `work.hold` and `supervisor.notify`.
mod decisions;

#[tokio::test]
async fn a_stopped_result_is_announced_once_and_again_after_the_decision_repeat_setting() {
    let h = delivery(false).await;
    // The repeat is Settings' (`decision_repeat_ms`), not a constant.
    h.limits(DeliveryLimits {
        decision_repeat_ms: 30_000,
        ..Default::default()
    })
    .await;
    let (task, pending) = h.stopped(h.scope.project_id, "success");
    // No artifact is active on the delivery binding: nothing is sent.
    assert_eq!((h.asked().await, h.events().await.len()), (0, 0));
    h.activated().await;
    assert_eq!(h.asked().await, 1);
    let events = h.events().await;
    assert_eq!(
        events,
        [
            json!({"task_id": task.to_string(), "project_id": pending.project_id.to_string(),
            "attempt_id": pending.attempt_id.to_string(), "result_id": pending.result_id.to_string(),
            "version": 3})
        ]
    );
    // Level-triggered: not again until the setting has passed.
    assert_eq!(h.asked().await, 0);
    h.later(29_999);
    assert_eq!(h.asked().await, 0);
    h.later(1);
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.events().await.len(), 2);
}

#[tokio::test]
async fn a_result_with_a_decision_is_not_announced_and_a_new_result_is_at_once() {
    let h = delivery(false).await;
    h.activated().await;
    let (task, pending) = h.stopped(h.scope.project_id, "success");
    assert_eq!(h.asked().await, 1);
    // A playbook decided on the result (complete, return or hold): never again.
    h.decided(task, pending.result_id, "held").await;
    h.later(10_000_000);
    assert_eq!(h.asked().await, 0);
    // The task is sent back and its next attempt stops with another result.
    let next = Pending {
        result_id: Uuid::new_v4(),
        attempt_id: Uuid::new_v4(),
        ..pending
    };
    *h.port.pending.lock().unwrap() = vec![pending, next];
    assert_eq!(h.asked().await, 1);
    let events = h.events().await;
    assert_eq!(events[1]["result_id"], next.result_id.to_string());
    assert_eq!(h.asked().await, 0);
}

#[tokio::test]
async fn nothing_is_announced_while_a_run_that_read_the_task_is_unfinished() {
    let h = delivery(false).await;
    h.activated().await;
    let (task, _) = h.stopped(h.scope.project_id, "success");
    assert_eq!(h.asked().await, 1);
    // The run the event started reads the task and has not finished.
    assert_eq!(h.read(task).await.outcome, "observed");
    // Neither the repeat interval nor main moving announces it again...
    h.later(120_000);
    h.main_moved(h.scope.project_id, &"c".repeat(40)).await;
    assert_eq!(h.asked().await, 0);
    // ...until the run is old enough (the longest check wait and the repeat, Settings) to be given up.
    h.later(3_700_000);
    assert_eq!(h.asked().await, 1);
}

#[tokio::test]
async fn a_change_of_main_or_of_the_pull_request_announces_again_at_once() {
    let h = delivery(false).await;
    h.activated().await;
    let (task, _) = h.stopped(h.scope.project_id, "success");
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.asked().await, 0);
    // Main moves.
    h.main_moved(h.scope.project_id, &"c".repeat(40)).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.asked().await, 0);
    // A pull request appears, and its checks finish.
    h.pull_request(task, "pending", json!([])).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.asked().await, 0);
    h.pull_request(task, "failed", json!(["unit"])).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.events().await.len(), 4);
}

#[tokio::test]
async fn only_the_namespaces_project_is_announced_unless_installation_routing_is_on() {
    for installation in [false, true] {
        let h = delivery(installation).await;
        h.activated().await;
        h.stopped(h.scope.project_id, "success");
        h.stopped(h.other_project, "success");
        let announced = h.asked().await;
        assert_eq!(announced, if installation { 2 } else { 1 });
    }
}

#[tokio::test]
async fn work_read_gives_the_result_facts_for_a_routed_and_a_non_routed_project() {
    let h = delivery(false).await;
    let own = h.scope.project_id;
    // The router serves the namespace's project only; the other is not on it.
    h.routing.on_router.lock().unwrap().insert(own);
    let (task, _) = h.stopped(own, "success");
    let (elsewhere, _) = h.stopped(h.other_project, "failed");
    h.main_moved(own, &"c".repeat(40)).await;
    h.main_moved(h.other_project, &"d".repeat(40)).await;
    let sql = "INSERT INTO delivery_tasks(task_id,returns) VALUES($1,2)";
    sqlx::query(sql)
        .bind(task)
        .execute(&h.pool().await)
        .await
        .unwrap();
    for (task, outcome, base_main) in [(task, "success", 'c'), (elsewhere, "failed", 'd')] {
        let read = h.read(task).await;
        assert_eq!(read.outcome, "observed", "{}", read.output);
        let facts = &read.output["facts"];
        assert_eq!(facts["outcome"], outcome);
        assert_eq!(facts["changed"], true);
        assert_eq!(facts["branch"], format!("aq/{task}"));
        assert_eq!(facts["head"], "a".repeat(40));
        assert_eq!(facts["base"], "b".repeat(40));
        assert_eq!(facts["main_head"], base_main.to_string().repeat(40));
        assert_eq!(facts["pull_request"], Value::Null);
        // The facts of 2.2.0 are still there.
        assert_eq!(facts["title"], "Stopped with a result");
        assert_eq!(
            (&facts["charged_failures"], &facts["ended_attempts"]),
            (&json!(1), &json!(2))
        );
    }
    assert_eq!(h.read(task).await.output["facts"]["returns"], 2);
    assert_eq!(h.read(elsewhere).await.output["facts"]["returns"], 0);
    // A pull request, once one is known.
    h.pull_request(task, "failed", json!(["unit", "lint"]))
        .await;
    let read = h.read(task).await;
    assert_eq!(
        read.output["facts"]["pull_request"],
        json!({"number": 7, "url": "https://example.test/pr/7", "state": "open",
            "mergeable": "mergeable", "checks": {"state": "failed", "failed": ["unit", "lint"]}})
    );
}

#[tokio::test]
async fn work_read_of_a_task_without_a_result_has_empty_result_facts_and_refuses_what_it_cannot_serve()
 {
    let h = delivery(false).await;
    let task = h.graph.task("Not run yet", json!({}));
    let facts = h.read(task).await.output["facts"].clone();
    assert_eq!(
        (
            &facts["outcome"],
            &facts["changed"],
            &facts["branch"],
            &facts["head"],
            &facts["base"]
        ),
        (
            &json!(""),
            &json!(false),
            &json!(""),
            &json!(""),
            &json!("")
        )
    );
    assert_eq!(
        (&facts["returns"], &facts["pull_request"]),
        (&json!(0), &Value::Null)
    );
    // A task of a project the server does not run, one that does not exist, an id that is no id.
    let stranger = h.graph.task("Elsewhere", json!({}));
    h.routing
        .located
        .lock()
        .unwrap()
        .insert(stranger, (Uuid::new_v4(), Uuid::new_v4()));
    let refused = h.read(stranger).await;
    assert_eq!(
        (refused.outcome.as_str(), &refused.output["reason"]),
        ("refused", &json!("not_on_router"))
    );
    let missing = h.read(Uuid::new_v4()).await;
    assert_eq!(missing.output["reason"], "unknown_task");
    let odd = h
        .run(
            "odd",
            "work.read",
            json!({"subject": {"id": "not-a-task", "revision": 1}}),
        )
        .await;
    assert_eq!(
        (odd.outcome.as_str(), &odd.output["reason"]),
        ("refused", &json!("unknown_task"))
    );
}

// ------------------------------------------------------------------
// The asynchronous delivery commands: main.sync, branch.publish and
// pull_request.*. A fake forge and central git stand behind the port; the
// adapters over GitHub and aq-git are tested in `aq-delivery`.
pub(super) mod commands;
