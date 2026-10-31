#![cfg(feature = "postgres-tests")]
mod support;
use aq_playbooks_model::{
    Artifact, ContractRef, EventContract, EventEnvelope, Expr, Operation, Schema, Step, digest,
};
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::{indexes::TimerIndex, retention::Disposal, *};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Barrier},
};
use support::*;

fn keyring(active: &str, both: bool) -> ProtectionKeyring {
    let mut keys = BTreeMap::new();
    if both || active == "old" {
        keys.insert("old".into(), [1; 32]);
    }
    if both || active == "new" {
        keys.insert("new".into(), [2; 32]);
    }
    ProtectionKeyring::new(active, keys).unwrap()
}
fn protected(active: &str, both: bool) -> PostgresStorage {
    PostgresStorage::connect_protected(&database(), keyring(active, both)).unwrap()
}
fn secret_state(n: &str) -> State {
    let mut state = boundary(n);
    let mut content = state.artifacts.values().next().unwrap().content.clone();
    content.playbook.purpose = "SYNTHETIC_PROTECTED_SENTINEL".into();
    let artifact = Artifact::seal(content).unwrap();
    state.artifacts.clear();
    state
        .artifacts
        .insert(artifact.digest.clone(), artifact.clone());
    for run in state.runs.values_mut() {
        run.artifact = artifact.digest.clone();
        run.input = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
        run.output = Some(json!("SYNTHETIC_PROTECTED_SENTINEL"));
        run.reason = Some("SYNTHETIC_PROTECTED_SENTINEL".into());
    }
    let intent = state.intents.get_mut("intent").unwrap();
    intent.input = json!("SYNTHETIC_PROTECTED_SENTINEL");
    intent.result = Some(("ok".into(), json!("SYNTHETIC_PROTECTED_SENTINEL")));
    intent.reason = Some("SYNTHETIC_PROTECTED_SENTINEL".into());
    state.receipts[0].kind = "unclassified".into();
    state.receipts[0].detail = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
    state
}
fn contains_secret(bytes: &[u8]) -> bool {
    bytes
        .windows(b"SYNTHETIC_PROTECTED_SENTINEL".len())
        .any(|window| window == b"SYNTHETIC_PROTECTED_SENTINEL")
}

#[test]
fn protected_snapshots_artifacts_and_safe_projection_have_no_plaintext_values() {
    let n = namespace("protected");
    let expected = secret_state(&n);
    let s = protected("old", false);
    assert!(s.protection_available());
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    let artifact = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let row=db.query_one("SELECT snapshot,encrypted_snapshot,protection_key_id FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n]).unwrap();
    assert!(row.get::<_, Option<Value>>(0).is_none());
    assert!(!contains_secret(&row.get::<_, Vec<u8>>(1)));
    assert_eq!(row.get::<_, String>(2), "old");
    assert_eq!(
        db.query_one(
            "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
            &[artifact]
        )
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    assert!(!contains_secret(&db.query_one("SELECT ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,artifact]).unwrap().get::<_,Vec<u8>>(0)));
    let projections:String=db.query_one("SELECT COALESCE(jsonb_agg(public_value)::text,'') FROM aq_playbooks.inspection_rows WHERE namespace=$1",&[&n]).unwrap().get(0);
    assert!(!contains_secret(projections.as_bytes()));
    assert_same(&s.load(&n).unwrap().state, &expected);
    drop(s);
    let unconfigured = store();
    assert!(unconfigured.load(&n).is_err());
    assert!(unconfigured.acquire(&n, "missing-key").is_err());
    let wrong = PostgresStorage::connect_protected(
        &database(),
        ProtectionKeyring::new("old", BTreeMap::from([("old".into(), [9; 32])])).unwrap(),
    )
    .unwrap();
    assert!(wrong.load(&n).is_err());
    assert!(wrong.acquire(&n, "wrong-key").is_err());
    // Public inspection deliberately exposes only the precomputed safe projection.
    let page = unconfigured
        .inspect(
            &n,
            &InspectionQuery {
                kind: InspectionKind::Runs,
                run: None,
                after: None,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(!contains_secret(&serde_json::to_vec(&page).unwrap()));
    let reopened = protected("old", false);
    reopened.acquire(&n, "correct-key").unwrap();
    assert_same(&reopened.load(&n).unwrap().state, &expected);
}

#[test]
fn authenticated_payload_tampering_never_falls_back_to_plaintext() {
    let n = namespace("tamper");
    let state = secret_state(&n);
    let s = protected("old", false);
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let mut db = client();
    let original: Vec<u8> = db
        .query_one(
            "SELECT encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n],
        )
        .unwrap()
        .get(0);
    let mut changed = original.clone();
    changed[0] ^= 1;
    db.execute(
        "UPDATE aq_playbooks.namespaces SET encrypted_snapshot=$2 WHERE namespace=$1",
        &[&n, &changed],
    )
    .unwrap();
    assert!(s.load(&n).is_err());
    assert!(s.compare_and_swap(&n, "owner", 1, &state).is_err());
    db.execute(
        "UPDATE aq_playbooks.namespaces SET encrypted_snapshot=$2 WHERE namespace=$1",
        &[&n, &original],
    )
    .unwrap();
    let a = state.artifacts.keys().next().unwrap();
    let original:Vec<u8>=db.query_one("SELECT ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,a]).unwrap().get(0);
    let mut changed = original.clone();
    changed[0] ^= 1;
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,a,&changed]).unwrap();
    // Another namespace's legitimate plaintext copy cannot bypass this namespace's AEAD.
    let other = namespace("plaintext-copy");
    let plain = store();
    plain.acquire(&other, "plain").unwrap();
    plain.compare_and_swap(&other, "plain", 0, &state).unwrap();
    assert!(s.load(&n).is_err());
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,a,&original]).unwrap();
    assert_same(&s.load(&n).unwrap().state, &state);
}

#[test]
fn rotation_reencrypts_snapshot_and_artifacts_and_old_backups_require_old_keys() {
    let n = namespace("rotation");
    let state = secret_state(&n);
    {
        let s = protected("old", false);
        s.acquire(&n, "first").unwrap();
        s.compare_and_swap(&n, "first", 0, &state).unwrap();
    }
    let mut db = client();
    let backup=db.query_one("SELECT revision,protection_key_id,protection_nonce,encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n]).unwrap();
    let artifact_backup=db.query_one("SELECT digest,key_id,nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1",&[&n]).unwrap();
    {
        let rotated = protected("new", true);
        rotated.acquire(&n, "rotation").unwrap();
        assert_eq!(rotated.reencrypt_namespace(&n, "rotation").unwrap(), 2);
    }
    assert_eq!(
        db.query_one(
            "SELECT protection_key_id FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n]
        )
        .unwrap()
        .get::<_, String>(0),
        "new"
    );
    assert_eq!(
        db.query_one(
            "SELECT key_id FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1",
            &[&n]
        )
        .unwrap()
        .get::<_, String>(0),
        "new"
    );
    assert_same(&protected("new", false).load(&n).unwrap().state, &state);
    assert!(protected("old", false).load(&n).is_err());
    let revision: i64 = backup.get(0);
    let key: String = backup.get(1);
    let nonce: Vec<u8> = backup.get(2);
    let ciphertext: Vec<u8> = backup.get(3);
    db.execute("UPDATE aq_playbooks.namespaces SET revision=$2,protection_key_id=$3,protection_nonce=$4,encrypted_snapshot=$5 WHERE namespace=$1",&[&n,&revision,&key,&nonce,&ciphertext]).unwrap();
    let digest: String = artifact_backup.get(0);
    let key: String = artifact_backup.get(1);
    let nonce: Vec<u8> = artifact_backup.get(2);
    let ciphertext: Vec<u8> = artifact_backup.get(3);
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET key_id=$3,nonce=$4,ciphertext=$5 WHERE namespace=$1 AND digest=$2",&[&n,&digest,&key,&nonce,&ciphertext]).unwrap();
    assert_same(&protected("old", false).load(&n).unwrap().state, &state);
    assert!(protected("new", false).load(&n).is_err());
}

fn indexed_state(n: &str) -> State {
    let mut state = boundary(n);
    state.runs.get_mut("parent").unwrap().authority.scope = "a".into();
    state.runs.get_mut("child").unwrap().authority.scope = "b".into();
    state.indexes.ready = BTreeMap::from([
        ("a".into(), BTreeMap::from([(2, "parent".into())])),
        ("b".into(), BTreeMap::from([(1, "child".into())])),
    ]);
    state.indexes.scope_cursor = Some("a".into());
    state.indexes.effects.insert(
        "dispatch".into(),
        BTreeMap::from([("a".into(), BTreeSet::from(["intent".into()]))]),
    );
    state
        .indexes
        .effect_cursor
        .insert("dispatch".into(), "a".into());
    state.indexes.timers.insert(
        "timer".into(),
        TimerIndex {
            key: "timer".into(),
            run: "child".into(),
            at_ms: 50,
            intent: None,
        },
    );
    state
        .indexes
        .due
        .insert(50, BTreeSet::from(["timer".into()]));
    let correlation = digest(&json!({"operation":"synthetic"})).unwrap();
    state
        .indexes
        .events
        .insert(correlation.clone(), BTreeMap::from([(7, "event".into())]));
    state
        .indexes
        .waiters
        .insert(correlation, BTreeSet::from(["parent".into()]));
    state.indexes.runs = state.runs.keys().cloned().collect();
    state
        .indexes
        .receipts
        .insert("parent".into(), BTreeMap::from([(1, 0)]));
    state.indexes.all_receipts.insert(1, 0);
    state
}
#[test]
fn native_indexes_match_memory_and_inspection_uses_bounded_keyset_pages() {
    let n = namespace("native-indexes");
    let state = indexed_state(&n);
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let key = state.indexes.events.keys().next().unwrap().clone();
    for query in [
        IndexQuery::Ready {
            scope_after: None,
            limit: 1,
        },
        IndexQuery::Ready {
            scope_after: Some("b".into()),
            limit: 2,
        },
        IndexQuery::DueTimers {
            now_ms: 49,
            limit: 2,
        },
        IndexQuery::DueTimers {
            now_ms: 50,
            limit: 2,
        },
        IndexQuery::Effects {
            kind: EffectQueueKind::Dispatch,
            scope_after: None,
            limit: 1,
        },
        IndexQuery::Effects {
            kind: EffectQueueKind::Completion,
            scope_after: None,
            limit: 1,
        },
        IndexQuery::CorrelatedEvents {
            key: key.clone(),
            after_sequence: 0,
            limit: 1,
        },
        IndexQuery::Waiters { key, limit: 1 },
        IndexQuery::Runs {
            after: Some("child".into()),
            limit: 1,
        },
        IndexQuery::Receipts {
            run: None,
            after_sequence: 0,
            limit: 1,
        },
        IndexQuery::Receipts {
            run: Some("parent".into()),
            after_sequence: 0,
            limit: 1,
        },
    ] {
        assert_eq!(
            serde_json::to_value(s.query(&n, &query).unwrap()).unwrap(),
            serde_json::to_value(state.indexes.query(&state, &query)).unwrap()
        );
    }
    let q = InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after: None,
        limit: 1,
    };
    let first = s.inspect(&n, &q).unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.revision, 1);
    assert!(first.next.is_some());
    let second = s
        .inspect(
            &n,
            &InspectionQuery {
                after: first.next,
                ..q
            },
        )
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next.is_none());
    assert_ne!(first.items[0]["id"], second.items[0]["id"]);
    let index_count:i64=client().query_one("SELECT count(*) FROM pg_indexes WHERE schemaname='aq_playbooks' AND tablename='runtime_index'",&[]).unwrap().get(0);
    assert!(index_count >= 4);
}

#[test]
fn native_same_scope_recovery_head_rotates_and_unchanged_history_rows_are_not_rewritten() {
    let n = namespace("recovery-fairness");
    let mut state = indexed_state(&n);
    state.indexes.effects.insert(
        "recovery".into(),
        BTreeMap::from([(
            "a".into(),
            BTreeSet::from(["intent-1".into(), "intent-2".into(), "intent-3".into()]),
        )]),
    );
    state
        .indexes
        .effect_item_cursor
        .insert("recovery:a".into(), "intent-1".into());
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let query = IndexQuery::Effects {
        kind: EffectQueueKind::Recovery,
        scope_after: None,
        limit: 1,
    };
    assert_eq!(s.query(&n, &query).unwrap().items[0].key, "intent-2");
    let before:String=client().query_one("SELECT xmin::text FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind='receipts' LIMIT 1",&[&n]).unwrap().get(0);
    state
        .indexes
        .effect_item_cursor
        .insert("recovery:a".into(), "intent-3".into());
    s.compare_and_swap(&n, "owner", 1, &state).unwrap();
    assert_eq!(s.query(&n, &query).unwrap().items[0].key, "intent-1");
    let after:String=client().query_one("SELECT xmin::text FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind='receipts' LIMIT 1",&[&n]).unwrap().get(0);
    assert_eq!(before, after);
    assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='effect:recovery'",&[&n]).unwrap().get::<_,i64>(0),1);
}

#[test]
fn disposal_quota_refuses_atomically_and_expired_unreferenced_artifact_metadata_is_collected() {
    let n = namespace("gc-quota");
    let first = artifact(&n);
    let second = artifact(&format!("{n}-second"));
    let original = State {
        artifacts: BTreeMap::from([
            (first.digest.clone(), first.clone()),
            (second.digest.clone(), second.clone()),
        ]),
        ..State::default()
    };
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &original).unwrap();
    let mut next = disposed(&first, 100);
    next.retention.policy.max_tombstones = 1;
    next.retention
        .artifact_disposals
        .insert(second.digest.clone(), 100);
    next.retention.disposals.push(Disposal {
        kind: "artifact".into(),
        key: second.digest.clone(),
        at_ms: 100,
    });
    assert!(
        s.compare_and_swap(&n, "owner", 1, &next)
            .unwrap_err()
            .to_string()
            .contains("quota")
    );
    assert_same(&s.load(&n).unwrap().state, &original);
    next.retention.policy.max_tombstones = 10;
    next.retention.policy.audit_ms = 1;
    next.retention.last_gc_at_ms = Some(100);
    s.compare_and_swap(&n, "owner", 1, &next).unwrap();
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
                &[&n]
            )
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    next.retention.disposals.clear();
    next.retention.artifact_disposals.clear();
    next.retention.last_gc_at_ms = Some(102);
    s.compare_and_swap(&n, "owner", 2, &next).unwrap();
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
                &[&n]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifact_catalog WHERE digest=ANY($1)",
                &[&vec![first.digest, second.digest]]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

#[test]
fn concurrent_inspection_revision_and_materialized_rows_commit_together() {
    let n = namespace("inspection-race");
    let s = Arc::new(store());
    s.acquire(&n, "owner").unwrap();
    let mut state = boundary(&n);
    state.runs.get_mut("parent").unwrap().visits = 1;
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let writer = {
        let s = s.clone();
        let n = n.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            for revision in 1..20 {
                state.runs.get_mut("parent").unwrap().visits = revision + 1;
                s.compare_and_swap(&n, "owner", revision, &state).unwrap();
            }
        })
    };
    let reader = store();
    barrier.wait();
    for _ in 0..40 {
        let page = reader
            .inspect(
                &n,
                &InspectionQuery {
                    kind: InspectionKind::Runs,
                    run: Some("parent".into()),
                    after: None,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(page.items[0]["visits"].as_u64(), Some(page.revision));
    }
    writer.join().unwrap();
}

fn disposed(artifact: &Artifact, at: i64) -> State {
    let mut state = State::default();
    state
        .retention
        .artifact_disposals
        .insert(artifact.digest.clone(), at);
    state.retention.disposals.push(Disposal {
        kind: "artifact".into(),
        key: artifact.digest.clone(),
        at_ms: at,
    });
    state
}
#[test]
fn artifact_gc_and_installation_race_preserves_shared_bytes_and_disposal_records() {
    let n = namespace("gc-a");
    let m = namespace("gc-b");
    let c = namespace("installer");
    let artifact = artifact(&n);
    let initial = State {
        artifacts: BTreeMap::from([(artifact.digest.clone(), artifact.clone())]),
        ..State::default()
    };
    let a = store();
    a.acquire(&n, "a").unwrap();
    a.compare_and_swap(&n, "a", 0, &initial).unwrap();
    let b = Arc::new(store());
    b.acquire(&m, "b").unwrap();
    b.compare_and_swap(&m, "b", 0, &initial).unwrap();
    a.compare_and_swap(&n, "a", 1, &disposed(&artifact, 100))
        .unwrap();
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let install = Arc::new(store());
    install.acquire(&c, "c").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let gc = {
        let b = b.clone();
        let m = m.clone();
        let artifact = artifact.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            b.compare_and_swap(&m, "b", 1, &disposed(&artifact, 101))
                .unwrap();
        })
    };
    let admitted = {
        let install = install.clone();
        let c = c.clone();
        let initial = initial.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            install.compare_and_swap(&c, "c", 0, &initial).unwrap();
        })
    };
    barrier.wait();
    gc.join().unwrap();
    admitted.join().unwrap();
    assert_same(&install.load(&c).unwrap().state, &initial);
    assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='artifact'",&[&n]).unwrap().get::<_,i64>(0),1);
}

#[test]
fn forged_disposal_cannot_erase_unresolved_children_or_external_effects() {
    let n = namespace("unresolved-gc");
    let original = boundary(&n);
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &original).unwrap();
    let a = original.artifacts.values().next().unwrap();
    let mut next = disposed(a, 100);
    next.sequence = original.sequence;
    for id in ["parent", "child"] {
        next.retention.disposals.push(Disposal {
            kind: "run".into(),
            key: id.into(),
            at_ms: 100,
        });
    }
    assert!(s.compare_and_swap(&n, "owner", 1, &next).is_err());
    assert_same(&s.load(&n).unwrap().state, &original);
}

#[test]
fn enabling_protection_removes_plaintext_only_after_every_referencing_namespace_is_protected() {
    let n = namespace("upgrade-one");
    let m = namespace("upgrade-two");
    let artifact = artifact(&n);
    let state = State {
        artifacts: BTreeMap::from([(artifact.digest.clone(), artifact.clone())]),
        ..State::default()
    };
    for namespace in [&n, &m] {
        let plain = store();
        plain.acquire(namespace, "plain").unwrap();
        plain
            .compare_and_swap(namespace, "plain", 0, &state)
            .unwrap();
    }
    {
        let encrypted = protected("old", false);
        encrypted.acquire(&n, "upgrade").unwrap();
        encrypted.reencrypt_namespace(&n, "upgrade").unwrap();
    }
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_same(&store().load(&m).unwrap().state, &state);
    {
        let encrypted = protected("old", false);
        encrypted.acquire(&m, "upgrade").unwrap();
        encrypted.reencrypt_namespace(&m, "upgrade").unwrap();
    }
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_same(&protected("old", false).load(&n).unwrap().state, &state);
    assert_same(&protected("old", false).load(&m).unwrap().state, &state);
}

#[test]
fn event_gc_replay_tombstones_survive_restart_and_explicit_new_epoch_has_new_identity() {
    let n = namespace("event-retention");
    let mut content = artifact(&n).content;
    let contract = EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "fixture.changed".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: BTreeSet::from(["fixture.producer".into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1,
    };
    content
        .registry
        .events
        .insert(contract.name.clone(), contract.clone());
    let artifact = Artifact::seal(content).unwrap();
    let authority = Authority {
        actor: "fixture".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let event = EventEnvelope {
        producer: "fixture.producer".into(),
        event_id: "stable-id".into(),
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::new(),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: Value::Null,
    };
    let mut engine = Engine::new(store(), &n, "first").unwrap();
    engine.install(artifact.clone()).unwrap();
    engine
        .activate("retained", &artifact.digest, authority)
        .unwrap();
    engine
        .set_retention_policy(retention::RetentionPolicy {
            event_replay_ms: 1,
            terminal_run_ms: 1,
            audit_ms: 1,
            max_tombstones: 1,
        })
        .unwrap();
    for epoch in 0..4 {
        let now = epoch as i64 * 100;
        assert!(
            !engine
                .admit_event_in_epoch("fixture.producer", event.clone(), epoch, now)
                .unwrap()
                .duplicate
        );
        assert_eq!(engine.collect_garbage(now + 10, 100).unwrap().events, 1);
        assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='event'",&[&n]).unwrap().get::<_,i64>(0),1);
        engine.release().unwrap();
        engine = Engine::new(store(), &n, format!("restarted-{epoch}")).unwrap();
        assert!(
            engine
                .admit_event_in_epoch("fixture.producer", event.clone(), epoch, now + 11)
                .is_err()
        );
        engine
            .advance_producer_epoch(
                "fixture.producer",
                epoch + 1,
                "operator",
                "new authenticated replay identity",
                now + 12,
            )
            .unwrap();
        assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='event'",&[&n]).unwrap().get::<_,i64>(0),0);
        assert!(
            engine
                .admit_event_in_epoch("fixture.producer", event.clone(), epoch, now + 13)
                .is_err()
        );
    }
}

#[test]
fn version_one_schema_upgrades_without_losing_state_and_owner_materializes_indexes() {
    let database_name = namespace("v1_database");
    let mut admin = client();
    admin
        .batch_execute(&format!("CREATE DATABASE {database_name}"))
        .unwrap();
    let mut config: postgres::Config = database().parse().unwrap();
    config.dbname(&database_name);
    let n = namespace("legacy");
    let expected = boundary(&n);
    let artifact = expected.artifacts.values().next().unwrap();
    {
        let mut old = config.connect(postgres::NoTls).unwrap();
        old.batch_execute("CREATE SCHEMA aq_playbooks; CREATE TABLE aq_playbooks.schema_migrations(version integer PRIMARY KEY,checksum bytea NOT NULL,applied_at timestamptz NOT NULL DEFAULT clock_timestamp());").unwrap();
        let sql = include_str!("../migrations/0001_initial.sql");
        old.batch_execute(sql).unwrap();
        old.execute(
            "INSERT INTO aq_playbooks.schema_migrations(version,checksum) VALUES(1,$1)",
            &[&Sha256::digest(sql.as_bytes()).to_vec()],
        )
        .unwrap();
        let mut snapshot = serde_json::to_value(&expected).unwrap();
        snapshot["artifacts"] = json!({});
        old.execute(
            "INSERT INTO aq_playbooks.namespaces(namespace,revision,snapshot) VALUES($1,1,$2)",
            &[&n, &snapshot],
        )
        .unwrap();
        old.execute(
            "INSERT INTO aq_playbooks.artifacts(digest,canonical_bytes) VALUES($1,$2)",
            &[
                &artifact.digest,
                &aq_playbooks_model::canonical_bytes(artifact).unwrap(),
            ],
        )
        .unwrap();
        old.execute(
            "INSERT INTO aq_playbooks.namespace_artifacts(namespace,digest) VALUES($1,$2)",
            &[&n, &artifact.digest],
        )
        .unwrap();
    }
    {
        let storage =
            PostgresStorage::from_client(config.connect(postgres::NoTls).unwrap()).unwrap();
        assert_same(&storage.load(&n).unwrap().state, &expected);
        assert!(
            storage
                .inspect(
                    &n,
                    &InspectionQuery {
                        kind: InspectionKind::Runs,
                        run: None,
                        after: None,
                        limit: 10
                    }
                )
                .is_err()
        );
        let engine = Engine::new(storage, &n, "upgrade-owner").unwrap();
        assert_eq!(
            engine
                .store
                .query(
                    &n,
                    &IndexQuery::Runs {
                        after: None,
                        limit: 10
                    }
                )
                .unwrap()
                .items
                .len(),
            2
        );
        assert_eq!(
            engine
                .store
                .inspect(
                    &n,
                    &InspectionQuery {
                        kind: InspectionKind::Runs,
                        run: None,
                        after: None,
                        limit: 10
                    }
                )
                .unwrap()
                .items
                .len(),
            2
        );
        engine.release().unwrap();
    }
    admin
        .batch_execute(&format!("DROP DATABASE {database_name} WITH (FORCE)"))
        .unwrap();
}

#[test]
fn actual_engine_requires_protection_before_admission_and_encrypts_sensitive_dispatch_and_output() {
    let n = namespace("sensitive-engine");
    let mut content = artifact(&n).content;
    let mut command = boundary(&n).intents["intent"].command.clone();
    command.name = "fixture.secret".into();
    let schema = Schema::object(BTreeMap::from([("secret".into(), Schema::string(256))]));
    command.input = schema.clone();
    command.outcomes = BTreeMap::from([("ok".into(), schema.clone())]);
    command.sensitive_input_fields = BTreeSet::from(["secret".into()]);
    command.sensitive_output_fields = BTreeSet::from(["secret".into()]);
    command.public_result_fields.clear();
    let reference = ContractRef {
        name: command.name.clone(),
        fingerprint: command.fingerprint().unwrap(),
    };
    content
        .registry
        .commands
        .insert(command.name.clone(), command);
    content.playbook.steps.insert(
        "invoke".into(),
        Step {
            op: Operation::Invoke {
                command: reference,
                input: Expr::reference("input"),
                bind: "private_result".into(),
                retry: None,
            },
            edges: BTreeMap::from([("ok".into(), "return".into())]),
            on_error: Some("failed".into()),
        },
    );
    content.playbook.steps.insert(
        "failed".into(),
        Step {
            op: Operation::Return {
                outcome: "failed".into(),
                output: Expr::literal(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    let entry = content.playbook.entries.get_mut("main").unwrap();
    entry.start = "invoke".into();
    entry.input = schema;
    entry.outcomes.insert("failed".into(), Schema::Null);
    let artifact = Artifact::seal(content).unwrap();
    artifact.verify().unwrap();
    let authority = Authority {
        actor: "fixture".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let input = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
    let plain = Engine::new(store(), namespace("no-protection"), "plain").unwrap();
    plain.install(artifact.clone()).unwrap();
    plain
        .activate("sensitive", &artifact.digest, authority.clone())
        .unwrap();
    assert!(
        plain
            .admit("sensitive", "main", input.clone(), authority.clone(), 0)
            .is_err()
    );
    assert!(plain.snapshot().unwrap().runs.is_empty());
    struct SecretHost {
        calls: usize,
    }
    impl Host for SecretHost {
        fn authorize(&self, a: &Authority, c: &aq_playbooks_model::CommandContract) -> bool {
            c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
        }
        fn dispatch(&mut self, i: &Intent) -> DispatchResult {
            self.calls += 1;
            assert_eq!(i.input["secret"], "SYNTHETIC_PROTECTED_SENTINEL");
            DispatchResult::Completed {
                outcome: "ok".into(),
                output: i.input.clone(),
            }
        }
        fn reconcile(&mut self, i: &Intent) -> DispatchResult {
            self.dispatch(i)
        }
    }
    {
        let engine = Engine::new(protected("old", false), &n, "protected").unwrap();
        engine.install(artifact.clone()).unwrap();
        engine
            .activate("sensitive", &artifact.digest, authority.clone())
            .unwrap();
        let root = engine
            .admit("sensitive", "main", input, authority, 0)
            .unwrap();
        let mut host = SecretHost { calls: 0 };
        engine.drive(&mut host, 0, 20).unwrap();
        let state = engine.snapshot().unwrap();
        assert_eq!(state.runs[&root].outcome.as_deref(), Some("ok"));
        assert_eq!(host.calls, 1);
        assert_eq!(
            state.runs[&root].variables["private_result"]["secret"],
            "SYNTHETIC_PROTECTED_SENTINEL"
        );
        let page = engine
            .store
            .inspect(
                &n,
                &InspectionQuery {
                    kind: InspectionKind::Receipts,
                    run: Some(root),
                    after: None,
                    limit: 100,
                },
            )
            .unwrap();
        assert!(!contains_secret(&serde_json::to_vec(&page).unwrap()));
        assert!(
            page.items
                .iter()
                .all(|receipt| receipt["detail"].get("input_digest").is_none())
        );
    }
    let restored = Engine::new(protected("old", false), &n, "reopened").unwrap();
    assert_eq!(
        restored
            .snapshot()
            .unwrap()
            .intents
            .values()
            .next()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .1["secret"],
        "SYNTHETIC_PROTECTED_SENTINEL"
    );
    assert!(store().load(&n).is_err());
}

#[test]
fn unchanged_commit_reuses_only_freshly_verified_bytes_and_has_bounded_value_free_profile() {
    let n = namespace("verified-reuse");
    let expected = boundary(&n);
    let s = store();
    assert!(s.profiling_snapshot().is_none());
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    s.set_profiling(true);
    s.compare_and_swap(&n, "owner", 1, &expected).unwrap();
    let profile = s.profiling_snapshot().unwrap();
    let cas = &profile.operations["cas"];
    assert_eq!(cas.completed, 1);
    assert_eq!(cas.artifact_verifications, 1);
    assert_eq!(cas.artifact_canonicalizations, 0);
    assert_eq!(cas.unchanged_artifacts_skipped, 1);
    assert_eq!(cas.stages["artifact_locks"].sql_calls, 1);
    assert!(!cas.stages.contains_key("artifact_write"));
    let serialized = serde_json::to_string(&profile).unwrap();
    assert!(!serialized.contains(&n));
    assert!(cas.stages.len() <= 16);
    let digest = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let bytes: Vec<u8> = db
        .query_one(
            "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
            &[digest],
        )
        .unwrap()
        .get(0);
    // This dedicated test connection bypasses the immutable-row trigger only for
    // its own corruption injection; other concurrent tests keep their triggers.
    db.batch_execute("SET session_replication_role=replica")
        .unwrap();
    db.execute(
        "UPDATE aq_playbooks.artifacts SET canonical_bytes=$2 WHERE digest=$1",
        &[digest, &b"corrupt".as_slice()],
    )
    .unwrap();
    db.batch_execute("SET session_replication_role=origin")
        .unwrap();
    assert!(s.compare_and_swap(&n, "owner", 2, &expected).is_err());
    db.batch_execute("SET session_replication_role=replica")
        .unwrap();
    db.execute(
        "UPDATE aq_playbooks.artifacts SET canonical_bytes=$2 WHERE digest=$1",
        &[digest, &bytes],
    )
    .unwrap();
    db.batch_execute("SET session_replication_role=origin")
        .unwrap();
    s.compare_and_swap(&n, "owner", 2, &expected).unwrap();
    drop(s);
    let cold = store();
    cold.acquire(&n, "new-owner").unwrap();
    cold.set_profiling(true);
    cold.compare_and_swap(&n, "new-owner", 3, &expected)
        .unwrap();
    assert_eq!(
        cold.profiling_snapshot().unwrap().operations["cas"].artifact_verifications,
        1
    );
}

#[test]
fn protected_reuse_preserves_nonce_but_explicit_same_key_refresh_and_tamper_checks_remain() {
    let n = namespace("protected-reuse");
    let expected = secret_state(&n);
    let s = protected("old", false);
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    let digest = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let artifact_envelope = |db: &mut postgres::Client| {
        let r=db.query_one("SELECT nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,digest]).unwrap();
        (r.get::<_, Vec<u8>>(0), r.get::<_, Vec<u8>>(1))
    };
    let original = artifact_envelope(&mut db);
    s.set_profiling(true);
    s.compare_and_swap(&n, "owner", 1, &expected).unwrap();
    assert_eq!(artifact_envelope(&mut db), original);
    assert_eq!(
        s.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        1
    );
    s.set_profiling(true);
    s.reencrypt_namespace(&n, "owner").unwrap();
    let refreshed = artifact_envelope(&mut db);
    assert_ne!(refreshed.0, original.0);
    assert_eq!(
        s.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        0
    );
    let mut corrupted = refreshed.1.clone();
    corrupted[0] ^= 1;
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,digest,&corrupted]).unwrap();
    assert!(s.compare_and_swap(&n, "owner", 3, &expected).is_err());
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,digest,&refreshed.1]).unwrap();
    let mut forged = expected.clone();
    forged
        .artifacts
        .values_mut()
        .next()
        .unwrap()
        .content
        .playbook
        .purpose = "changed under same address".into();
    assert!(s.compare_and_swap(&n, "owner", 3, &forged).is_err());
    assert_eq!(s.load(&n).unwrap().revision, 3);
    assert_same(&s.load(&n).unwrap().state, &expected);
    let profile = serde_json::to_vec(&s.profiling_snapshot()).unwrap();
    assert!(!contains_secret(&profile));
}

#[test]
fn ordered_batch_catalog_locks_serialize_multi_artifact_installation_with_gc() {
    for iteration in 0..8 {
        let n = namespace(&format!("batch-gc-{iteration}"));
        let other = namespace(&format!("batch-install-{iteration}"));
        let artifacts = [artifact(&format!("{n}-one")), artifact(&format!("{n}-two"))];
        let before = State {
            artifacts: artifacts
                .iter()
                .map(|a| (a.digest.clone(), a.clone()))
                .collect(),
            ..State::default()
        };
        let mut removed = State::default();
        for a in &artifacts {
            removed
                .retention
                .artifact_disposals
                .insert(a.digest.clone(), 10);
        }
        let a = store();
        let b = store();
        a.acquire(&n, "gc").unwrap();
        b.acquire(&other, "installer").unwrap();
        a.compare_and_swap(&n, "gc", 0, &before).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                barrier.wait();
                a.compare_and_swap(&n, "gc", 1, &removed)
            });
            let installer = scope.spawn(|| {
                barrier.wait();
                b.compare_and_swap(&other, "installer", 0, &before)
            });
            barrier.wait();
            dropper.join().unwrap().unwrap();
            installer.join().unwrap().unwrap();
        });
        assert!(a.load(&n).unwrap().state.artifacts.is_empty());
        assert_same(&b.load(&other).unwrap().state, &before);
        let mut db = client();
        assert_eq!(db.query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='artifact'",&[&n]).unwrap().get::<_,i64>(0),2);
        for artifact in &artifacts {
            let bytes: Vec<u8> = db
                .query_one(
                    "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
                    &[&artifact.digest],
                )
                .unwrap()
                .get(0);
            assert_eq!(
                bytes,
                aq_playbooks_model::canonical_bytes(artifact).unwrap()
            );
        }
    }
}

#[test]
fn ordinary_commit_with_new_active_key_refreshes_unchanged_artifacts() {
    let n = namespace("ordinary-key-change");
    let expected = secret_state(&n);
    let digest = expected.artifacts.keys().next().unwrap();
    {
        let original = protected("old", false);
        original.acquire(&n, "old-owner").unwrap();
        original
            .compare_and_swap(&n, "old-owner", 0, &expected)
            .unwrap();
    }
    let current = protected("new", true);
    current.acquire(&n, "new-owner").unwrap();
    current.set_profiling(true);
    current
        .compare_and_swap(&n, "new-owner", 1, &expected)
        .unwrap();
    assert_eq!(
        current.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        0
    );
    let key: String = client()
        .query_one(
            "SELECT key_id FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",
            &[&n, digest],
        )
        .unwrap()
        .get(0);
    assert_eq!(key, "new");
    drop(current);
    assert_same(&protected("new", false).load(&n).unwrap().state, &expected);
    assert!(protected("old", false).load(&n).is_err());
}

#[test]
fn mixed_existing_and_new_catalog_batches_commit_without_lock_inversion() {
    for iteration in 0..8 {
        let n = namespace(&format!("mixed-left-{iteration}"));
        let other = namespace(&format!("mixed-right-{iteration}"));
        let existing_left = artifact(&format!("{n}-existing"));
        let existing_right = artifact(&format!("{other}-existing"));
        let fresh = artifact(&format!("{n}-new-shared"));
        let left = State {
            artifacts: BTreeMap::from([(existing_left.digest.clone(), existing_left.clone())]),
            ..State::default()
        };
        let right = State {
            artifacts: BTreeMap::from([(existing_right.digest.clone(), existing_right.clone())]),
            ..State::default()
        };
        let after = State {
            artifacts: [existing_left, existing_right, fresh]
                .into_iter()
                .map(|a| (a.digest.clone(), a))
                .collect(),
            ..State::default()
        };
        let a = store();
        let b = store();
        a.acquire(&n, "left").unwrap();
        b.acquire(&other, "right").unwrap();
        a.compare_and_swap(&n, "left", 0, &left).unwrap();
        b.compare_and_swap(&other, "right", 0, &right).unwrap();
        let barrier = Barrier::new(3);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                a.compare_and_swap(&n, "left", 1, &after)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                b.compare_and_swap(&other, "right", 1, &after)
            });
            barrier.wait();
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });
        assert_same(&a.load(&n).unwrap().state, &after);
        assert_same(&b.load(&other).unwrap().state, &after);
        for namespace in [&n, &other] {
            assert_eq!(
                client()
                    .query_one(
                        "SELECT count(*) FROM aq_playbooks.namespace_artifacts WHERE namespace=$1",
                        &[namespace]
                    )
                    .unwrap()
                    .get::<_, i64>(0),
                3
            );
        }
    }
}
