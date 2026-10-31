//! Real disposable-PG contract checks; no tests require an existing database.
use super::{admission, fixture, full_engine, tree::*};
use aq_playbooks_model::{canonical_bytes, digest};
use aq_playbooks_runner::{Engine, EngineConfig};
use postgres::Client;
use serde_json::json;
fn passed(name: &str) {
    println!(
        "{}",
        json!({"scenario":"prototype_check","name":name,"passed":true})
    );
}
pub fn run(client: &mut Client) -> Result<()> {
    let state = fixture::with_closed_intent(fixture::history_state(32));
    assert!(
        !state.runs.is_empty()
            && !state.events.is_empty()
            && !state.receipts.is_empty()
            && !state.intents.is_empty()
    );
    // A full genesis source audit is mandatory. SQL mirrors never certify waiters.
    for (kind, (mut source, event)) in [
        ("waiter", fixture::waiter_state()),
        (
            "consumer",
            (fixture::consumer_state(), fixture::envelope("fallback")),
        ),
    ] {
        for (id, r) in &state.runs {
            source.runs.entry(id.clone()).or_insert_with(|| r.clone());
        }
        source.events.extend(state.events.clone());
        source.receipts.extend(state.receipts.clone());
        source.intents.extend(state.intents.clone());
        // Keep original relevant source subscription; initialize other unrelated
        // records through the canonical rebuild before rooted certification.
        source.indexes.initialized = false;
        let e = full_engine(&source)?;
        let source = Engine::new(e.store, "reference", "owner")?.snapshot()?;
        assert!(
            if kind == "waiter" {
                !source.indexes.waiters.is_empty()
            } else {
                !source.indexes.triggers.is_empty()
            },
            "missing canonical relevant {kind}: {}",
            serde_json::to_string(&source.runs)?
        );
        let mut tx = client.transaction()?;
        let mut tree = Tree::new(&mut tx);
        let root = tree.build(&serde_json::to_value(&source)?)?;
        let result = admission::plan(&mut tree, &root, "pb02", event, 0, 10_000);
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains(&format!("fallback: relevant {kind}"))
        );
        drop(tree);
        tx.rollback()?;
        passed(&format!(
            "source_{kind}_with_unrelated_history_is_explicit_fallback"
        ));
    }
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let root = tree.build(&serde_json::to_value(&state)?)?;
    tree.flush()?;
    drop(tree);
    tx.execute("DELETE FROM pb04_root", &[])?;
    tx.execute(
        "INSERT INTO pb04_root VALUES(true,'owner',0,$1)",
        &[&canonical_bytes(&root)?],
    )?;
    tx.commit()?;
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let event = fixture::envelope("unrelated-history");
    let planned = admission::plan(&mut tree, &root, "pb02", event.clone(), 0, 10_000)?;
    let next = admission::patch(&mut tree, &root, &planned.before, &planned.after)?;
    let reference = full_engine(&state)?;
    reference.admit_event("pb02", event, 10_000)?;
    tree.prefetch_subtree(&next)?;
    assert_eq!(
        canonical_bytes(&tree.value(&next)?)?,
        canonical_bytes(&reference.snapshot()?)?
    );
    assert_eq!(
        next.bytes,
        canonical_bytes(&reference.snapshot()?)?.len() as u64
    );
    drop(tree);
    tx.rollback()?;
    passed("full_reference_with_unrelated_runs_events_receipts_intents");
    // Existing event error/duplicate behavior remains the canonical engine's.
    let old = state.events.values().next().unwrap().envelope.clone();
    for (name, event, epoch, now) in [
        ("exact_duplicate", old.clone(), 0, 10_000),
        (
            "same_id_changed_bytes",
            {
                let mut e = old.clone();
                e.payload = json!("changed");
                e
            },
            0,
            10_000,
        ),
        ("expired_replay", old.clone(), 0, i64::MAX),
        ("retired_epoch", old.clone(), 1, 10_000),
        (
            "forged_producer",
            {
                let mut e = fixture::envelope("forged");
                e.producer = "forged".into();
                e
            },
            0,
            10_000,
        ),
    ] {
        let mut tx = client.transaction()?;
        let mut tree = Tree::new(&mut tx);
        let result = admission::plan(&mut tree, &root, "pb02", event.clone(), epoch, now);
        let expected = full_engine(&state)?.admit_event_in_epoch("pb02", event, epoch, now);
        match (result, expected) {
            (Ok(plan), Ok(value)) => {
                assert_eq!(
                    canonical_bytes(&plan.disposition)?,
                    canonical_bytes(&value)?
                );
                let next = admission::patch(&mut tree, &root, &plan.before, &plan.after)?;
                assert_eq!(root, next);
            }
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
            _ => return Err("canonical result mismatch".into()),
        }
        drop(tree);
        tx.rollback()?;
        passed(name);
    }
    // Failed exact global quota does not publish a root or node writes.
    let count_before: i64 = client
        .query_one("SELECT count(*) FROM pb04_nodes", &[])?
        .get(0);
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let plan = admission::plan(
        &mut tree,
        &root,
        "pb02",
        fixture::envelope("quota"),
        0,
        10_000,
    )?;
    let next = admission::patch(&mut tree, &root, &plan.before, &plan.after)?;
    let config = EngineConfig {
        max_state_bytes: root.bytes as usize,
        ..EngineConfig::default()
    };
    assert!(admission::quota(&mut tree, &next, &config).is_err());
    tree.flush()?;
    drop(tree);
    tx.rollback()?;
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM pb04_nodes", &[])?
            .get::<_, i64>(0),
        count_before
    );
    assert_eq!(
        client
            .query_one("SELECT root FROM pb04_root", &[])?
            .get::<_, Vec<u8>>(0),
        canonical_bytes(&root)?
    );
    passed("quota_and_staged_nodes_rollback_atomically");
    // Root-path corruption must fail before planning. Never trust address alone.
    let original: Vec<u8> = client
        .query_one(
            "SELECT payload FROM pb04_nodes WHERE hash=$1",
            &[&root.hash],
        )?
        .get(0);
    let mut corrupt = original.clone();
    corrupt.push(b' ');
    let mut tx = client.transaction()?;
    tx.execute(
        "UPDATE pb04_nodes SET payload=$1 WHERE hash=$2",
        &[&corrupt, &root.hash],
    )?;
    let mut tree = Tree::new(&mut tx);
    assert!(
        admission::plan(
            &mut tree,
            &root,
            "pb02",
            fixture::envelope("corrupt"),
            0,
            10_000
        )
        .is_err()
    );
    drop(tree);
    tx.rollback()?;
    passed("touched_raw_bytes_corruption_fails");
    // Untouched history is NOT scanned on admission. A later explicit full audit
    // must fail: this is the intentional touched-integrity/full-audit boundary.
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let receipts = tree.path(&root, &["receipts"])?.unwrap();
    drop(tree);
    let payload: Vec<u8> = tx
        .query_one(
            "SELECT payload FROM pb04_nodes WHERE hash=$1",
            &[&receipts.hash],
        )?
        .get(0);
    let mut corrupt = payload;
    corrupt.push(b' ');
    tx.execute(
        "UPDATE pb04_nodes SET payload=$1 WHERE hash=$2",
        &[&corrupt, &receipts.hash],
    )?;
    let mut tree = Tree::new(&mut tx);
    let plan = admission::plan(
        &mut tree,
        &root,
        "pb02",
        fixture::envelope("untouched"),
        0,
        10_000,
    )?;
    let next = admission::patch(&mut tree, &root, &plan.before, &plan.after)?;
    admission::quota(&mut tree, &next, &EngineConfig::default())?;
    assert!(tree.prefetch_subtree(&next).is_err());
    drop(tree);
    tx.rollback()?;
    passed("untouched_history_corruption_detected_by_explicit_full_audit");
    // Speculative SQL lookahead must not make malformed unrelated bytes into
    // authority or widen this planner's certified logical dependency set.
    for payload in [vec![0xff, 0xfe], b"{invalid json".to_vec()] {
        let mut tx = client.transaction()?;
        tx.execute(
            "UPDATE pb04_nodes SET payload=$1 WHERE hash=$2",
            &[&payload, &receipts.hash],
        )?;
        let mut tree = Tree::new(&mut tx);
        let plan = admission::plan(
            &mut tree,
            &root,
            "pb02",
            fixture::envelope("malformed-neighbor"),
            0,
            10_000,
        )?;
        let next = admission::patch(&mut tree, &root, &plan.before, &plan.after)?;
        admission::quota(&mut tree, &next, &EngineConfig::default())?;
        assert!(tree.prefetch_subtree(&next).is_err());
        drop(tree);
        tx.rollback()?;
    }
    passed("malformed_speculative_neighbors_remain_untrusted_and_full_audit_fails");
    // Existing hash-address rows must be rechecked even if staged payload matches.
    let mut tx = client.transaction()?;
    tx.execute(
        "UPDATE pb04_nodes SET payload=$1 WHERE hash=$2",
        &[&corrupt, &root.hash],
    )?;
    let mut tree = Tree::new(&mut tx);
    tree.build(&serde_json::to_value(&state)?)?;
    assert!(tree.flush().is_err());
    drop(tree);
    tx.rollback()?;
    passed("deduplicated_existing_bytes_cannot_hide_corruption");
    // Canonical punctuation, escaped keys/unicode, empty arrays and AVL rotations.
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let mut value = json!({});
    let mut root = tree.build(&value)?;
    for i in 0..96 {
        let key = format!("escaped\\\"\n-é-{:03}", (i * 37) % 97);
        let leaf = json!({"n":i,"empty":[],"unicode":"雪","nested":{"empty":{}}});
        value
            .as_object_mut()
            .unwrap()
            .insert(key.clone(), leaf.clone());
        root = tree.set(&root, &[&key], &leaf)?;
        assert_eq!(root.bytes, canonical_bytes(&value)?.len() as u64);
        assert_eq!(tree.value(&root)?, value);
    }
    drop(tree);
    tx.rollback()?;
    passed("exact_canonical_delimiters_escapes_empty_containers_and_rotations");
    // Read-set certification refuses synchronization that could touch omitted data.
    for field in ["dirty_runs", "dirty_intents"] {
        let mut value = serde_json::to_value(&state)?;
        value["indexes"][field] = json!(["omitted"]);
        let mut tx = client.transaction()?;
        let mut tree = Tree::new(&mut tx);
        let root = tree.build(&value)?;
        assert!(
            admission::plan(
                &mut tree,
                &root,
                "pb02",
                fixture::envelope("dirty"),
                0,
                10_000
            )
            .err()
            .unwrap()
            .to_string()
            .contains("fallback: initialization")
        );
        drop(tree);
        tx.rollback()?;
    }
    passed("omitted_dirty_record_sources_require_full_reference_fallback");
    // Exact epoch identity used by canonical event keys is not approximated.
    let mut changed = state.clone();
    changed.retention.producer_epochs.insert("pb02".into(), 3);
    let ev = fixture::envelope("epoch-three");
    let expected = full_engine(&changed)?.admit_event_in_epoch("pb02", ev.clone(), 3, 10_000)?;
    let mut tx = client.transaction()?;
    let mut tree = Tree::new(&mut tx);
    let root = tree.build(&serde_json::to_value(&changed)?)?;
    let plan = admission::plan(&mut tree, &root, "pb02", ev.clone(), 3, 10_000)?;
    assert_eq!(plan.disposition.key, digest(&("pb02", 3u64, &ev.event_id))?);
    assert_eq!(
        canonical_bytes(&plan.disposition)?,
        canonical_bytes(&expected)?
    );
    drop(tree);
    tx.rollback()?;
    passed("selected_producer_epoch_matches_canonical_identity");
    Ok(())
}
