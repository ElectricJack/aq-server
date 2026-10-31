//! Isolated PB04 rooted-state experiment; no production schema/API changes.
//! Uses a fresh disposable database marker. Never a qualified Storage backend.
#[path = "rooted/admission.rs"]
mod admission;
#[path = "rooted/checks.rs"]
mod checks;
#[path = "rooted/tree.rs"]
mod tree;
#[allow(dead_code, unused_imports)]
mod fixture {
    include!("rooted/fixture.rs");
    pub fn history_state(history: usize) -> State {
        let e = base();
        for i in 0..history {
            let id = e
                .admit("a", "main", Value::Null, authority("a"), i as i64)
                .unwrap();
            e.advance(&id, i as i64).unwrap();
            e.admit_event("pb02", event(&format!("history-{i}")), i as i64)
                .unwrap();
        }
        e.snapshot().unwrap()
    }
    pub fn envelope(id: &str) -> EventEnvelope {
        event(id)
    }
}
use aq_playbooks_model::{canonical_bytes, hash_bytes};
use aq_playbooks_runner::{storage::*, *};
use postgres::{Client, NoTls};
use serde_json::json;
use std::time::Instant;
use tree::{Link, Result, Tree};
fn full_engine(state: &State) -> Result<Engine<MemoryStorage>> {
    let store = MemoryStorage::default();
    store.acquire("reference", "owner")?;
    store.compare_and_swap("reference", "owner", 0, state)?;
    Ok(Engine {
        store,
        namespace: "reference".into(),
        owner: "owner".into(),
        config: EngineConfig::default(),
        metrics: metrics::Metrics::default(),
    })
}
fn main() -> Result<()> {
    if cfg!(debug_assertions) {
        return Err("release prototype only".into());
    }
    let mut client = Client::connect(&std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")?, NoTls)?;
    let marker: String = client
        .query_one("SELECT token FROM public.aq_pb02_disposable", &[])?
        .get(0);
    if marker != std::env::var("AQ_PB02_DISPOSABLE_TOKEN")? {
        return Err("disposable marker".into());
    }
    client.batch_execute("CREATE TABLE pb04_nodes(hash text PRIMARY KEY,payload bytea NOT NULL); CREATE TABLE pb04_root(singleton bool PRIMARY KEY CHECK(singleton),owner text NOT NULL,revision bigint NOT NULL,root bytea NOT NULL);
    CREATE FUNCTION pb04_probe_json(raw bytea) RETURNS jsonb LANGUAGE plpgsql IMMUTABLE STRICT AS $$
    BEGIN RETURN convert_from(raw,'UTF8')::jsonb;
    EXCEPTION WHEN invalid_text_representation OR character_not_in_repertoire OR untranslatable_character THEN RETURN NULL;
    END $$;")?;
    println!(
        "{}",
        json!({"scenario":"prototype_configuration","authority":"isolated root row under transaction lock; one fixed owner; no lease/failover qualification","format":"prototype-only AVL JSON tree; arrays opaque","budgets_us":{"event_admission":25000},"samples":24,"batched_reads":std::env::var_os("AQ_PB04_BATCHED").is_some(),"protection":"unsupported; synthetic unprotected only","limits":"no consumer/waiter fastpath only; all other operations fallback; no production Storage implementation"})
    );
    if std::env::var_os("AQ_PB04_CHECKS_ONLY").is_some() {
        return checks::run(&mut client);
    }
    for history in [0, 32, 128, 512] {
        let state = fixture::history_state(history);
        if state
            .events
            .values()
            .flat_map(|e| &e.destinations)
            .any(|d| d.run.is_none())
        {
            return Err("genesis queued destinations unsupported".into());
        }
        // Genesis trusts no native SQL projection: source-derived indexes must
        // match relevant waiter/trigger maps before establishing this lineage.
        let mut rebuild = state.clone();
        rebuild.indexes.initialized = false;
        let audit = Engine::new(full_engine(&rebuild)?.store, "reference", "owner")?.snapshot()?;
        if canonical_bytes(&audit.indexes.waiters)? != canonical_bytes(&state.indexes.waiters)?
            || canonical_bytes(&audit.indexes.triggers)?
                != canonical_bytes(&state.indexes.triggers)?
        {
            return Err("genesis lookup/source drift".into());
        }
        let reference = full_engine(&state)?;
        let mut tx = client.transaction()?;
        tx.execute("DELETE FROM pb04_root", &[])?;
        tx.execute("DELETE FROM pb04_nodes", &[])?;
        let mut tree = Tree::new(&mut tx);
        let root = tree.build(&serde_json::to_value(&state)?)?;
        if root.bytes != canonical_bytes(&state)?.len() as u64 {
            return Err("genesis exact quota mismatch".into());
        }
        tree.flush()?;
        drop(tree);
        tx.execute(
            "INSERT INTO pb04_root VALUES(true,'owner',0,$1)",
            &[&canonical_bytes(&root)?],
        )?;
        tx.commit()?;
        for sample in 0..24 {
            let event = fixture::envelope(&format!("prototype-{sample}"));
            let started = Instant::now();
            let mut tx = client.transaction()?;
            let row = tx.query_one(
                "SELECT owner,revision,root FROM pb04_root WHERE singleton FOR UPDATE",
                &[],
            )?;
            let owner: String = row.get(0);
            if owner != "owner" {
                return Err("owner lost".into());
            }
            let revision: i64 = row.get(1);
            let root: Link = serde_json::from_slice(&row.get::<_, Vec<u8>>(2))?;
            let mut tree = Tree::new(&mut tx);
            let planned = admission::plan(&mut tree, &root, "pb02", event.clone(), 0, 10_000)?;
            let next = admission::patch(&mut tree, &root, &planned.before, &planned.after)?;
            admission::quota(&mut tree, &next, &EngineConfig::default())?;
            tree.flush()?;
            let work = serde_json::to_value(&tree.work)?;
            drop(tree);
            let changed=tx.execute("UPDATE pb04_root SET root=$1,revision=revision+1 WHERE singleton AND owner=$2 AND revision=$3",&[&canonical_bytes(&next)?,&owner,&revision])?;
            if changed != 1 {
                return Err("root fence".into());
            }
            tx.commit()?;
            let elapsed = started.elapsed().as_micros();
            // Full reference and audit intentionally outside admission timing.
            let expected = reference.admit_event("pb02", event, 10_000)?;
            if canonical_bytes(&expected)? != canonical_bytes(&planned.disposition)? {
                return Err("canonical disposition mismatch".into());
            }
            let mut tx = client.transaction()?;
            let mut tree = Tree::new(&mut tx);
            tree.prefetch_subtree(&next)?;
            let actual = tree.value(&next)?;
            let bytes = canonical_bytes(&actual)?;
            if bytes != canonical_bytes(&reference.snapshot()?)? || next.bytes != bytes.len() as u64
            {
                return Err("full reference/exact quota mismatch".into());
            }
            let state_hash = hash_bytes(&bytes);
            drop(tree);
            tx.rollback()?;
            println!(
                "{}",
                json!({"scenario":"rooted_no_consumers","history":history,"sample":sample,"wall_us":elapsed,"budget_pass":elapsed<25000,"work":work,"canonical_state_bytes":next.bytes,"state_hash":state_hash,"full_reference_equal":true})
            );
        }
        // Cold full audit and stable root survive a new connection, never cache.
        let mut cold = Client::connect(&std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")?, NoTls)?;
        let row = cold.query_one("SELECT root FROM pb04_root WHERE singleton", &[])?;
        let root: Link = serde_json::from_slice(&row.get::<_, Vec<u8>>(0))?;
        let mut tx = cold.transaction()?;
        let mut tree = Tree::new(&mut tx);
        tree.prefetch_subtree(&root)?;
        assert_eq!(
            canonical_bytes(&tree.value(&root)?)?,
            canonical_bytes(&reference.snapshot()?)?
        );
        println!(
            "{}",
            json!({"scenario":"cold_full_audit","history":history,"full_reference_equal":true})
        );
    }
    checks::run(&mut client)?;
    Ok(())
}
