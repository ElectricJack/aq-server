//! D5 host adapters on disposable PostgreSQL through the production service
//! owners. The graph port is an in-memory CAS fake (no real graph network);
//! operation.cancel reaches the actual B3 native cancellation owner.
use crate::common;
use aq_playbook_host::runner::{Intent, IntentStatus};
use aq_server::playbooks::{
    domain::*,
    host_adapters::{HostResult, WorkGraph},
    review_domain::CatalogProfile,
    service::*,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    tree: Mutex<Option<Value>>,
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
    async fn activation_tree(&self, _: Uuid, _: Uuid) -> Result<Value> {
        self.tree.lock().unwrap().clone().ok_or(Error::Unknown)
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
            match stage_operation(&mut staged, &key, op) {
                Ok(result) => results.push(result),
                Err(status) => return Ok((status, json!({}))),
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

/// Apply to the private staged map; the caller commits only a complete batch.
fn stage_operation(
    nodes: &mut BTreeMap<Uuid, Value>,
    key: &str,
    op: &Value,
) -> std::result::Result<Value, u16> {
    match op["op"].as_str().unwrap() {
        "create_node" => {
            let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
            if nodes.contains_key(&id) {
                return Err(409);
            }
            let mut node = op.clone();
            node["version"] = json!(1);
            nodes.insert(id, node.clone());
            Ok(node)
        }
        "update_node" => update_staged_node(nodes, key, op),
        "add_edge" => Ok(op.clone()),
        other => panic!("unexpected op {other}"),
    }
}

fn update_staged_node(
    nodes: &mut BTreeMap<Uuid, Value>,
    key: &str,
    op: &Value,
) -> std::result::Result<Value, u16> {
    let id: Uuid = serde_json::from_value(op["id"].clone()).unwrap();
    let node = nodes.get_mut(&id).ok_or(404_u16)?;
    if node["version"] != op["expected_version"] {
        return Err(409);
    }
    let patch = op["patch"].as_object().unwrap();
    if let Some(state) = patch.get("task_state") {
        assert!(
            key.starts_with("aq-playbook-activate-"),
            "no lifecycle shortcut"
        );
        assert_eq!(node["task_state"], "draft");
        assert_eq!(state, "active");
    }
    for (k, v) in patch {
        node[k] = v.clone();
    }
    if !patch.is_empty() {
        node["version"] = json!(node["version"].as_i64().unwrap() + 1);
    }
    Ok(node.clone())
}

struct Host {
    service: Service,
    profile: CatalogProfile,
    runtime: RuntimeOwner,
    grant: Uuid,
    scope: GrantScope,
    now: i64,
    graph: Arc<FakeGraph>,
}
async fn host(state: &aq_server::AppState, actor: Uuid, workspace: Uuid, project: Uuid) -> Host {
    host_with_profile(state, actor, workspace, project, CatalogProfile::Host5).await
}
async fn host_with_profile(
    state: &aq_server::AppState,
    actor: Uuid,
    workspace: Uuid,
    project: Uuid,
    profile: CatalogProfile,
) -> Host {
    let keys = super::playbook_fixtures::keyring();
    let graph = Arc::new(FakeGraph::default());
    let namespace = Namespace {
        namespace_id: Uuid::new_v4(),
        generation: 1,
        name: format!("fixture-{}", Uuid::new_v4()),
        workspace_id: workspace,
        project_id: project,
        owner_user_id: actor,
    };
    let service = if profile == CatalogProfile::Host5 {
        Service::new_host5(state, namespace, Arc::new(keys), graph.clone()).await
    } else {
        Service::new_host2(
            state,
            namespace,
            Arc::new(keys),
            profile,
            graph.clone(),
            None,
            None,
            None,
        )
        .await
    }
    .unwrap();
    service.initialize(actor).await.unwrap();
    let now = (state.clock)().timestamp_millis();
    let mut scope = GrantScope {
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
    if profile == CatalogProfile::Host11 {
        scope
            .capabilities
            .extend(["work.activate", "decision.request", "operation.get"].map(String::from));
    }
    let grant = Uuid::new_v4();
    service
        .grant(actor, grant, scope.clone(), now + 10_000)
        .await
        .unwrap();
    let runtime = service.adopt_runtime(Uuid::new_v4(), 0).await.unwrap();
    Host {
        service,
        profile,
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
            command: self.profile.registry().commands[name].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: self.profile.authority(self.grant, 1, &self.scope).unwrap(),
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
async fn host11_activates_only_subtrees_of_ensured_tasks_and_retains_counts() {
    let f = common::profiled::fixture().await;
    common::profiled::insert(&f).await;
    let lineage = &f.selection.lineage;
    let h = host_with_profile(
        &f.state,
        f.actor,
        lineage.workspace_id,
        lineage.project_id,
        CatalogProfile::Host11,
    )
    .await;
    let activate = |intent, id, revision| {
        h.intent(
            intent,
            "work.activate",
            json!({"subject":{"id":id,"revision":revision}}),
        )
    };
    let foreign = h
        .run(&activate("foreign", Uuid::new_v4(), 1))
        .await
        .unwrap();
    assert_eq!(foreign.outcome, "refused");
    assert_eq!(foreign.output["reason"], "not_ensured");
    assert_eq!(h.graph.mutations(), 0);
    let ensured = h
        .run(&h.intent("ensure", "work.ensure", ensure_input("planner", "context")))
        .await
        .unwrap();
    assert_eq!(ensured.outcome, "accepted");
    let (task, root) = {
        let nodes = h.graph.nodes.lock().unwrap();
        let (task, root) = nodes.first_key_value().unwrap();
        (*task, root.clone())
    };
    let child = Uuid::new_v4();
    let mut root = root;
    root["workspace_id"] = json!(lineage.workspace_id);
    root["deleted"] = json!(false);
    let draft = json!({"id":child,"workspace_id":lineage.workspace_id,
        "kind":"task","deleted":false,"task_state":"draft","version":4});
    h.graph.nodes.lock().unwrap().insert(child, draft.clone());
    *h.graph.tree.lock().unwrap() = Some(json!({"node":root,"children":[
        {"node":draft,"children":[],"truncated":false}
    ],"truncated":false}));
    let intent = activate("activate", task, 1);
    let result = h.run(&intent).await.unwrap();
    assert_eq!(result.outcome, "activated");
    assert_eq!(result.output, json!({"activated":1,"skipped":0}));
    assert_eq!(h.graph.version(child), 5);
    assert_eq!(h.graph.version(task), 1);
    assert_eq!(h.graph.mutations(), 2);
    assert_eq!(h.run(&intent).await.unwrap(), result);
    assert_eq!(h.graph.mutations(), 2);

    let decision = h
        .run(&h.intent(
            "decision",
            "decision.request",
            json!({"subject":{"id":task,"revision":1}, "question":"Is the plan ready?",
                "choices":["approved","rejected"], "responders":[format!("user:{}", f.actor)],
                "expires_at_ms":h.now + 4000}),
        ))
        .await
        .unwrap();
    assert_eq!(decision.outcome, "accepted");
    let handle = serde_json::from_value(decision.output["handle"].clone()).unwrap();
    let note = "Add the rollback steps.";
    let answer = h
        .service
        .answer_decision(f.actor, handle, "rejected", Some(note))
        .await
        .unwrap();
    assert_eq!(answer.outcome, "rejected");
    assert_eq!(answer.output["note"], note);
    let retained = h
        .run(&h.intent("read-decision", "operation.get", json!({"handle": handle})))
        .await
        .unwrap();
    assert_eq!(retained.outcome, "completed");
    assert_eq!(
        retained.output,
        json!({"outcome":"rejected","output":answer.output})
    );
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
        "../../../../vendor/aq-host-catalog-1.0.0/fixtures.json"
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

#[cfg(test)]
mod fake_graph_tests {
    use super::*;

    #[tokio::test]
    async fn rejected_batch_preserves_nodes_and_does_not_reserve_replay_key() {
        let existing = Uuid::new_v4();
        let added = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let original = json!({"id": existing, "version": 1, "title": "original"});
        let cases = [
            (json!({"op": "create_node", "id": existing}), 409),
            (
                json!({"op": "update_node", "id": missing, "expected_version": 1, "patch": {}}),
                404,
            ),
            (
                json!({"op": "update_node", "id": existing, "expected_version": 1, "patch": {}}),
                409,
            ),
        ];
        for (rejected, status) in cases {
            let graph = FakeGraph::default();
            graph
                .nodes
                .lock()
                .unwrap()
                .insert(existing, original.clone());
            let create = json!({"op": "create_node", "id": added});
            let request = json!({"idempotency_key": "batch", "operations": [
                create.clone(),
                {"op": "update_node", "id": existing, "expected_version": 1, "patch": {"title": "changed"}},
                rejected
            ]});
            assert_eq!(
                graph.mutate(Uuid::nil(), request).await.unwrap(),
                (status, json!({}))
            );
            assert_eq!(
                *graph.nodes.lock().unwrap(),
                BTreeMap::from([(existing, original.clone())])
            );
            assert!(graph.replies.lock().unwrap().is_empty());
            let retry = json!({"idempotency_key": "batch", "operations": [create]});
            assert_eq!(graph.mutate(Uuid::nil(), retry).await.unwrap().0, 200);
            assert_eq!(graph.version(added), 1);
            assert_eq!(graph.mutations(), 2);
        }
    }

    #[tokio::test]
    async fn successful_replay_keeps_reply_and_rejects_changed_request() {
        let graph = FakeGraph::default();
        let id = Uuid::new_v4();
        let edge = json!({"op": "add_edge", "source": id, "target": id});
        let request = json!({"idempotency_key": "replay", "operations": [
            {"op": "create_node", "id": id}, edge.clone()
        ]});
        let reply = graph.mutate(Uuid::nil(), request.clone()).await.unwrap();
        assert_eq!(reply.0, 200);
        assert_eq!(reply.1["results"][1], edge);
        assert_eq!(
            graph.mutate(Uuid::nil(), request.clone()).await.unwrap(),
            reply
        );
        let mut changed = request;
        changed["operations"][0]["title"] = json!("changed");
        assert_eq!(
            graph.mutate(Uuid::nil(), changed).await.unwrap(),
            (409, json!({}))
        );
        assert_eq!(graph.mutations(), 1);
        assert_eq!(graph.version(id), 1);
    }

    #[tokio::test]
    async fn empty_patch_keeps_version_and_activation_advances_it() {
        let graph = FakeGraph::default();
        let id = Uuid::new_v4();
        graph
            .nodes
            .lock()
            .unwrap()
            .insert(id, json!({"id": id, "version": 1, "task_state": "draft"}));
        let request = json!({"idempotency_key": "aq-playbook-activate-fixture", "operations": [
            {"op": "update_node", "id": id, "expected_version": 1, "patch": {}},
            {"op": "update_node", "id": id, "expected_version": 1, "patch": {"task_state": "active"}}
        ]});
        let (status, reply) = graph.mutate(Uuid::nil(), request).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(reply["results"][0]["version"], 1);
        assert_eq!(reply["results"][1]["version"], 2);
        assert_eq!(reply["results"][1]["task_state"], "active");
        assert_eq!(graph.version(id), 2);
    }
}
