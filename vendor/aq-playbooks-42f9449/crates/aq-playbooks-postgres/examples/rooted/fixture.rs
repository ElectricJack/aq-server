// Synthetic fixtures copied from PB02; transition rules are not copied.
use aq_playbooks_model::*;
use aq_playbooks_runner::{storage::*, *};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn authority(scope: &str) -> Authority {
    Authority {
        actor: "pb02-synthetic".into(),
        scope: scope.into(),
        capabilities: BTreeSet::new(),
    }
}
fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "pb02.signal".into(),
        version: "1".into(),
        payload: Schema::string(512),
        trusted_producers: BTreeSet::from(["pb02".into()]),
        scopes: BTreeSet::from(["a".into(), "b".into(), "c".into()]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 86400000,
    }
}
fn event(id: &str) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "pb02".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "a".into(),
        correlation: BTreeMap::new(),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: json!("p".repeat(128)),
    }
}
fn ret() -> Step {
    Step {
        op: Operation::Return {
            outcome: "ok".into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn artifact(id: &str, scope: &str, mut first: Step) -> Artifact {
    if !matches!(&first.op, Operation::Return { .. }) {
        first.on_error = Some("return".into());
    }
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "pb02-benchmark-v1".into(),
        source: SourceBinding {
            digest: hash_bytes(id.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: id.into(),
                scope: scope.into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: id.into(),
            purpose: "Finite synthetic storage/scheduling experiment".into(),
            scope: scope.into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "first".into(),
                },
            )]),
            steps: if matches!(&first.op, Operation::Return { .. }) {
                BTreeMap::from([("first".into(), first)])
            } else {
                BTreeMap::from([("first".into(), first), ("return".into(), ret())])
            },
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([("pb02.signal".into(), event_contract())]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn install<S: Storage>(e: &Engine<S>, binding: &str, a: Artifact) {
    let scope = a.content.playbook.scope.clone();
    e.install(a.clone()).unwrap();
    e.activate_at(binding, &a.digest, authority(&scope), 0)
        .unwrap();
}
fn base() -> Engine<MemoryStorage> {
    let e = Engine::new(MemoryStorage::default(), "seed", "seed-owner").unwrap();
    for scope in ["a", "b", "c"] {
        install(
            &e,
            scope,
            artifact(&format!("return-{scope}"), scope, ret()),
        );
    }
    let child = artifact(
        "child-wait",
        "a",
        Step {
            op: Operation::Wait {
                event: None,
                correlation: BTreeMap::new(),
                minimum_revision: None,
                deadline_ms: 1_000_000,
                bind: "wake".into(),
            },
            edges: BTreeMap::from([("timeout".into(), "return".into())]),
            on_error: None,
        },
    );
    install(&e, "child", child.clone());
    let mut parent = artifact(
        "parent",
        "a",
        Step {
            op: Operation::Call {
                dependency: "child".into(),
                entry: "main".into(),
                input: Expr::literal(Value::Null),
                mode: CallMode::OwnedAsync,
                bind: "handle".into(),
            },
            edges: BTreeMap::from([("started".into(), "return".into())]),
            on_error: None,
        },
    );
    parent.content.dependencies.insert(
        "child".into(),
        CallableDependency {
            binding: "child".into(),
            selection: DependencySelection::Exact,
            artifact: child.digest.clone(),
            interfaces: child.content.playbook.entries.clone(),
            capabilities: BTreeSet::new(),
            scope: "a".into(),
            dependencies: BTreeSet::new(),
        },
    );
    parent = Artifact::seal(parent.content).unwrap();
    install(&e, "parent", parent);
    e
}

pub fn with_closed_intent(mut state: State) -> State {
    let run = state.runs.keys().next().unwrap().clone();
    let intent:Intent=serde_json::from_value(json!({"id":"unrelated-closed-intent","run":run,"step":"invoke","invocation":1,"deadline_at_ms":1000,"command":{"name":"command","version":"1","input":{"type":"null"},"outcomes":{"ok":{"type":"null"}},"capabilities":[],"scopes":["a"],"effect":"mutation","recovery":"keyed_idempotent","asynchronous":null,"cancellation":false,"public_result_fields":[],"sensitive_input_fields":[],"sensitive_output_fields":[],"max_payload_bytes":1024,"deadline_ms":1000},"input":null,"input_digest":digest(&Value::Null).unwrap(),"authority":{"actor":"actor","scope":"a","capabilities":[]},"status":"completed","cancel_requested":false,"cancel_dispatched":false,"handle":null,"result":["ok",null],"reason":null,"late_results":[],"completion_event":null})).unwrap();
    state.intents.insert(intent.id.clone(), intent);
    state.indexes.initialized = false;
    let store = MemoryStorage::default();
    store.acquire("fixture", "owner").unwrap();
    store
        .compare_and_swap("fixture", "owner", 0, &state)
        .unwrap();
    Engine::new(store, "fixture", "owner")
        .unwrap()
        .snapshot()
        .unwrap()
}
pub fn waiter_state() -> (State, EventEnvelope) {
    let e = base();
    let mut contract = event_contract();
    contract.name = "pb04.correlated".into();
    contract.correlation_fields.insert("invocation".into());
    let mut event = event("waiter");
    event.contract = ContractRef {
        name: contract.name.clone(),
        fingerprint: contract.fingerprint().unwrap(),
    };
    event
        .correlation
        .insert("invocation".into(), "fixture".into());
    let mut wait = artifact("event-wait", "a", ret());
    wait.content
        .registry
        .events
        .insert(contract.name.clone(), contract);
    wait.content.playbook.steps.insert(
        "first".into(),
        Step {
            op: Operation::Wait {
                event: Some(event.contract.clone()),
                correlation: BTreeMap::from([(
                    "invocation".into(),
                    Expr::literal(json!("fixture")),
                )]),
                minimum_revision: None,
                deadline_ms: 1_000_000,
                bind: "wake".into(),
            },
            edges: BTreeMap::from([
                ("timeout".into(), "return".into()),
                ("received".into(), "return".into()),
            ]),
            on_error: Some("return".into()),
        },
    );
    wait.content.playbook.steps.insert("return".into(), ret());
    let wait = Artifact::seal(wait.content).unwrap();
    install(&e, "event-wait", wait);
    let id = e
        .admit("event-wait", "main", Value::Null, authority("a"), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    (e.snapshot().unwrap(), event)
}
pub fn consumer_state() -> State {
    let e = base();
    let ev = event("consumer");
    let mut a = artifact("triggered", "a", ret());
    a.content.playbook.triggers.push(Trigger {
        id: "trigger".into(),
        event: ev.contract,
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    a = Artifact::seal(a.content).unwrap();
    install(&e, "triggered", a);
    e.snapshot().unwrap()
}
