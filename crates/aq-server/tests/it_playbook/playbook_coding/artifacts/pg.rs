//! `PgLedger` against a real database (`DATABASE_URL`, see `scripts/test-db`),
//! driven through `activate`/`rollback` with a fake engine catalog.
use super::*;
use aq_server::test_support::{TestDb, store_pool};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Engine stand-in. `activate` yields between deciding and switching so
/// callers that are not serialized interleave inside the critical section.
#[derive(Default)]
struct Engine {
    active: Mutex<HashMap<String, String>>,
    switches: Mutex<usize>,
}
impl Engine {
    fn switches(&self) -> usize {
        *self.switches.lock().unwrap()
    }
}
impl Catalog for Engine {
    fn registry(&self) -> Registry {
        Registry::default()
    }
    async fn install(&self, _: Artifact) -> Result<()> {
        Ok(())
    }
    async fn activate(&self, binding: &str, digest: &str) -> Result<()> {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        self.active
            .lock()
            .unwrap()
            .insert(binding.into(), digest.into());
        *self.switches.lock().unwrap() += 1;
        Ok(())
    }
    async fn current(&self, binding: &str) -> Result<Option<String>> {
        Ok(self.active.lock().unwrap().get(binding).cloned())
    }
}

/// A ledger for a fresh playbook namespace (the table's foreign key).
async fn namespace_ledger(db: &TestDb, name: &str) -> PgLedger {
    let store = db.store().await;
    let owner = match store
        .bootstrap_owner(
            "owner@localhost",
            "dev",
            "owner@localhost",
            chrono::Utc::now(),
        )
        .await
        .unwrap()
    {
        Some(user) => user.id,
        None => sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(store_pool(&store))
            .await
            .unwrap(),
    };
    let namespace_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest)
         VALUES($1,1,$2,$3,$4,$5,repeat('c',64),repeat('s',64))",
    )
    .bind(namespace_id)
    .bind(name)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(owner)
    .execute(store_pool(&store))
    .await
    .unwrap();
    PgLedger {
        pool: store_pool(&store).clone(),
        namespace_id,
    }
}

fn entries(history: &[Record]) -> Vec<(Kind, &str, Option<&str>)> {
    history
        .iter()
        .map(|r| (r.kind, r.artifact.as_str(), r.previous.as_deref()))
        .collect()
}

async fn assert_install_replays_first_record(ledger: &PgLedger) {
    // First writer wins per op id; install rows stay out of history.
    let install = Record {
        op_id: "i1".into(),
        kind: Kind::Install,
        binding: "b".into(),
        artifact: "d0".into(),
        previous: None,
    };
    assert_eq!(ledger.insert(install.clone()).await.unwrap(), install);
    let other = Record {
        artifact: "dX".into(),
        ..install.clone()
    };
    assert_eq!(ledger.insert(other).await.unwrap(), install);
    assert_eq!(ledger.get("i1").await.unwrap(), Some(install));
    assert_eq!(ledger.get("missing").await.unwrap(), None);
}

async fn assert_replays_and_conflicts_do_not_switch(engine: &Engine, ledger: &PgLedger) {
    // Replay returns the recorded result without touching the engine.
    let again = activate(engine, ledger, "a2", "b", "d2").await.unwrap();
    assert_eq!(
        (&again["previous"], &again["duplicate"]),
        (&json!("d1"), &json!(true))
    );
    let again = rollback(engine, ledger, "r1", "b").await.unwrap();
    assert_eq!(
        (&again["artifact"], &again["duplicate"]),
        (&json!("d1"), &json!(true))
    );
    assert_eq!(engine.switches(), 5);

    // Reusing an op id for a different request is a conflict.
    for refused in [
        activate(engine, ledger, "a2", "b", "dX").await,
        activate(engine, ledger, "a2", "other", "d2").await,
        activate(engine, ledger, "r1", "b", "d1").await,
        rollback(engine, ledger, "a3", "b").await,
    ] {
        assert!(matches!(refused, Err(Error::Conflict)), "{refused:?}");
    }
    assert_eq!(engine.switches(), 5);
}

#[tokio::test]
async fn pg_ledger_replays_conflicts_and_orders_history() {
    let db = TestDb::create().await;
    let ledger = namespace_ledger(&db, "ns-a").await;
    let engine = Engine::default();

    assert_install_replays_first_record(&ledger).await;

    let first = activate(&engine, &ledger, "a1", "b", "d1").await.unwrap();
    assert_eq!(first["previous"], Value::Null);
    assert_eq!(first["duplicate"], false);
    activate(&engine, &ledger, "a2", "b", "d2").await.unwrap();
    let back = rollback(&engine, &ledger, "r1", "b").await.unwrap();
    assert_eq!(
        (&back["artifact"], &back["previous"]),
        (&json!("d1"), &json!("d2"))
    );
    activate(&engine, &ledger, "a3", "b", "d3").await.unwrap();
    activate(&engine, &ledger, "c1", "other", "o1")
        .await
        .unwrap();
    assert_eq!(engine.switches(), 5);

    assert_replays_and_conflicts_do_not_switch(&engine, &ledger).await;

    // History is one binding's activate/rollback rows in commit order, and
    // survives a new ledger handle on the same namespace.
    let reopened = PgLedger {
        pool: ledger.pool.clone(),
        namespace_id: ledger.namespace_id,
    };
    let history = reopened.history("b").await.unwrap();
    assert_eq!(
        entries(&history),
        [
            (Kind::Activate, "d1", None),
            (Kind::Activate, "d2", Some("d1")),
            (Kind::Rollback, "d1", Some("d2")),
            (Kind::Activate, "d3", Some("d1")),
        ]
    );
    assert_eq!(
        history.iter().map(|r| r.op_id.as_str()).collect::<Vec<_>>(),
        ["a1", "a2", "r1", "a3"]
    );
    assert_eq!(
        entries(&reopened.history("other").await.unwrap()),
        [(Kind::Activate, "o1", None)]
    );

    // Namespaces are isolated: the same op id is free in another one.
    let n = namespace_ledger(&db, "ns-b").await;
    assert_eq!(n.get("a1").await.unwrap(), None);
    assert!(n.history("b").await.unwrap().is_empty());
    let fresh = activate(&engine, &n, "a1", "b", "d9").await.unwrap();
    assert_eq!(
        (&fresh["previous"], &fresh["duplicate"]),
        (&json!("d3"), &json!(false))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_activate_and_rollback_serialize_per_binding() {
    const N: usize = 8;
    let db = TestDb::create().await;
    let ledger = Arc::new(namespace_ledger(&db, "ns-race").await);
    let engine = Arc::new(Engine::default());

    let tasks: Vec<_> = (0..N)
        .map(|i| {
            let (ledger, engine) = (ledger.clone(), engine.clone());
            tokio::spawn(async move {
                activate(&*engine, &*ledger, &format!("a{i}"), "b", &format!("d{i}"))
                    .await
                    .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    // Each activation saw the one committed before it: the history is one
    // chain, and the engine ends on its last link.
    let history = ledger.history("b").await.unwrap();
    assert_eq!(history.len(), N);
    assert_eq!(history[0].previous, None);
    for pair in history.windows(2) {
        assert_eq!(pair[1].previous.as_deref(), Some(pair[0].artifact.as_str()));
    }
    let top = history[N - 1].artifact.clone();
    assert_eq!(engine.current("b").await.unwrap(), Some(top));
    assert_eq!(engine.switches(), N);

    // N-1 rollbacks, each raced by a retry of the same op id: every rollback
    // pops exactly one entry and every retry replays it.
    let tasks: Vec<_> = (1..N)
        .flat_map(|i| [i, i])
        .map(|i| {
            let (ledger, engine) = (ledger.clone(), engine.clone());
            tokio::spawn(async move {
                rollback(&*engine, &*ledger, &format!("r{i}"), "b")
                    .await
                    .unwrap()
            })
        })
        .collect();
    let mut duplicates = 0;
    for t in tasks {
        duplicates += usize::from(t.await.unwrap()["duplicate"] == true);
    }
    assert_eq!(duplicates, N - 1);
    assert_eq!(engine.switches(), 2 * N - 1);
    let bottom = history[0].artifact.clone();
    assert_eq!(engine.current("b").await.unwrap(), Some(bottom));
    let history = ledger.history("b").await.unwrap();
    let mut expected: Vec<_> = history[..N].iter().map(|r| r.artifact.clone()).collect();
    for r in &history[N..] {
        let current = expected.pop();
        assert_eq!(
            (r.previous.clone(), Some(r.artifact.clone())),
            (current, expected.last().cloned())
        );
    }
    assert!(matches!(
        rollback(&*engine, &*ledger, "r-last", "b").await,
        Err(Error::Conflict)
    ));
}
