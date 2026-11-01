use aq_playbooks_model::*;
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::*;
use postgres::{Client, NoTls};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
pub fn database() -> String {
    std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL").expect("set AQ_PLAYBOOKS_TEST_DATABASE_URL to a disposable PostgreSQL database, or run scripts/test-postgres.sh")
}
pub fn namespace(label: &str) -> String {
    format!(
        "test_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        label
    )
}
pub fn store() -> PostgresStorage {
    PostgresStorage::connect(&database()).unwrap()
}
pub fn client() -> Client {
    Client::connect(&database(), NoTls).unwrap()
}
pub fn artifact(label: &str) -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "storage-fixture-v1".into(),
        source: SourceBinding {
            digest: hash_bytes(label.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: label.into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: label.into(),
            purpose: "Synthetic persistence fixture".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "return".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "return".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(Value::Null),
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
pub fn boundary(label: &str) -> State {
    // Distinct records ensure a database boundary preserves the relationship
    // among progress, parent/child admission, receipts, waits and outgoing work.
    let a = artifact(label);
    let authority = Authority {
        actor: "synthetic-test".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let parent = Run {
        id: "parent".into(),
        root: "parent".into(),
        parent: None,
        origin: None,
        artifact: a.digest.clone(),
        entry: "main".into(),
        catalog_revision: 0,
        dependencies: BTreeMap::new(),
        authority: authority.clone(),
        step: "join".into(),
        status: RunStatus::Waiting,
        input: Value::Null,
        variables: BTreeMap::new(),
        outcome: None,
        output: None,
        reason: None,
        admitted_at_ms: 1,
        visits: 2,
        depth: 0,
        children: vec!["child".into()],
        pending: Some(Pending::Child {
            child: "child".into(),
            bind: "answer".into(),
        }),
        loops: vec![],
        attempts: BTreeMap::new(),
        pending_return: None,
        pending_failure: false,
        terminal_sequence: None,
    };
    let mut child = parent.clone();
    child.id = "child".into();
    child.parent = Some("parent".into());
    child.children.clear();
    child.depth = 1;
    child.pending = Some(Pending::Effect {
        intent: "intent".into(),
    });
    let command = CommandContract {
        name: "fixture.ensure".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: BTreeSet::new(),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 1000,
    };
    let mut state = State::default();
    state.artifacts.insert(a.digest.clone(), a);
    state.runs.insert("parent".into(), parent);
    state.runs.insert("child".into(), child);
    state.receipts.push(Receipt {
        sequence: 1,
        run: "parent".into(),
        step: "start".into(),
        at_ms: 1,
        kind: "child_admitted".into(),
        detail: json!({"child":"child"}),
    });
    state.intents.insert(
        "intent".into(),
        Intent {
            id: "intent".into(),
            run: "child".into(),
            step: "ensure".into(),
            invocation: 1,
            deadline_at_ms: 60_000,
            command,
            input: Value::Null,
            input_digest: digest(&Value::Null).unwrap(),
            authority,
            status: IntentStatus::Dispatched,
            cancel_requested: false,
            cancel_dispatched: false,
            late_results: vec![],
            completion_event: None,
            handle: Some("host-operation".into()),
            result: None,
            reason: None,
        },
    );
    state.waits.insert(
        "wait".into(),
        WaitRecord {
            id: "wait".into(),
            run: "child".into(),
            step: "ensure".into(),
            event: None,
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), "host-operation".into())]),
            minimum_revision: Some(3),
            deadline: 1000,
            bind: "output".into(),
            winner: None,
            expired: false,
        },
    );
    state.sequence = 42;
    state
}
pub fn assert_same(actual: &State, expected: &State) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

/// Explicit operator corruption fixtures still pass the writer-format fence so
/// their original AEAD/revision assertions exercise the intended deeper layer.
pub fn marked<'a>(client: &'a mut Client, namespace: &str) -> postgres::Transaction<'a> {
    let mut tx = client.transaction().unwrap();
    tx.query_one("SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','2',true)",&[&namespace]).unwrap();
    tx
}
