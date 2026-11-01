//! Actual PostgreSQL catalog ownership/projection boundaries. Feature-gated like
//! the existing storage suite; caller supplies an explicitly disposable database.
use super::*;
use aq_playbooks_runner::{
    CatalogError, CatalogKind, CatalogQuery,
    catalog_inspection::{CatalogRecord, MAX_CATALOG_BYTES, activation_key},
};
use std::{collections::BTreeSet, sync::atomic::Ordering};
#[path = "../tests/support/mod.rs"]
#[allow(dead_code)]
mod support;
use support::{artifact, boundary, client, database, namespace};

fn keys(active: &str) -> ProtectionKeyring {
    ProtectionKeyring::new(
        active,
        BTreeMap::from([("old".into(), [1; 32]), ("new".into(), [2; 32])]),
    )
    .unwrap()
}
fn configured_store(protected: bool) -> PostgresStorage {
    if protected {
        PostgresStorage::connect_protected(&database(), keys("old")).unwrap()
    } else {
        PostgresStorage::connect(&database()).unwrap()
    }
}
fn state(label: &str) -> State {
    let mut state = boundary(label);
    let digest = state.artifacts.keys().next().unwrap().clone();
    state.bindings.insert(
        "PRIVATE_BINDING_SENTINEL".into(),
        aq_playbooks_runner::Activation {
            artifact: digest,
            authority: aq_playbooks_runner::Authority {
                actor: "operator".into(),
                scope: "project".into(),
                capabilities: BTreeSet::new(),
            },
            revision: 1,
            controls: Default::default(),
        },
    );
    for run in state.runs.values_mut() {
        run.input = serde_json::json!("PRIVATE_INPUT_SENTINEL");
    }
    state
}
fn seeded(label: &str, protected: bool) -> (PostgresStorage, String, State) {
    let n = namespace(label);
    let state = state(&n);
    let store = configured_store(protected);
    store.acquire(&n, "owner").unwrap();
    store.compare_and_swap(&n, "owner", 0, &state).unwrap();
    (store, n, state)
}

#[test]
fn catalog_pages_are_atomic_and_unrelated_progress_preserves_metadata_stamp() {
    let (store, n, mut state) = seeded("catalog-pages", false);
    for name in ["one", "two"] {
        let value = artifact(name);
        state.artifacts.insert(value.digest.clone(), value);
    }
    store.compare_and_swap(&n, "owner", 1, &state).unwrap();
    let first = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 1),
        )
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert!(first.next_cursor.is_some());
    let exact = CatalogQuery {
        exact: Some(first.items[0].key.clone()),
        ..CatalogQuery::first(CatalogKind::Definitions, 25)
    };
    assert_eq!(
        store.inspect_catalog(&n, "owner", &exact).unwrap().items,
        first.items
    );
    let next = CatalogQuery {
        after: first.next_cursor.clone(),
        expected: Some(first.stamp.clone()),
        ..CatalogQuery::first(CatalogKind::Definitions, 1)
    };
    state.sequence += 1;
    store.compare_and_swap(&n, "owner", 2, &state).unwrap();
    let continued = store.inspect_catalog(&n, "owner", &next).unwrap();
    assert_eq!(continued.stamp, first.stamp);
    assert_eq!(continued.state_revision, 3);
    let binding = state.bindings.get_mut("PRIVATE_BINDING_SENTINEL").unwrap();
    binding.revision += 1;
    binding.controls.calls_enabled = false;
    store.compare_and_swap(&n, "owner", 3, &state).unwrap();
    assert_eq!(
        store.inspect_catalog(&n, "owner", &next).unwrap_err(),
        CatalogError::Conflict
    );
    let query = CatalogQuery {
        exact: Some(activation_key("PRIVATE_BINDING_SENTINEL")),
        ..CatalogQuery::first(CatalogKind::Activations, 1)
    };
    let page = store.inspect_catalog(&n, "owner", &query).unwrap();
    let CatalogRecord::Activation(binding) = &page.items[0].record else {
        panic!("activation")
    };
    assert_eq!(binding.revision, 2);
    assert!(!binding.controls.calls_enabled);
    assert!(
        !serde_json::to_string(&page)
            .unwrap()
            .contains("PRIVATE_INPUT_SENTINEL")
    );
}

#[test]
fn protected_catalog_bytes_context_rotation_and_tampering_are_fenced() {
    let n = namespace("catalog-private");
    let mut state = state(&n);
    let mut content = state.artifacts.values().next().unwrap().content.clone();
    content.playbook.purpose = "PRIVATE_PURPOSE_SENTINEL".into();
    let value = Artifact::seal(content).unwrap();
    state.artifacts.clear();
    state.artifacts.insert(value.digest.clone(), value.clone());
    for run in state.runs.values_mut() {
        run.artifact = value.digest.clone();
    }
    state
        .bindings
        .get_mut("PRIVATE_BINDING_SENTINEL")
        .unwrap()
        .artifact = value.digest;
    let store = configured_store(true);
    store.acquire(&n, "owner").unwrap();
    store.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let original = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    assert!(
        serde_json::to_string(&original)
            .unwrap()
            .contains("PRIVATE_PURPOSE_SENTINEL")
    );
    let rows=client().query("SELECT item_key,plain_bytes,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1",&[&n]).unwrap();
    for row in rows {
        assert!(row.get::<_, Option<Vec<u8>>>(1).is_none());
        let ciphertext: Vec<u8> = row.get(2);
        let key: String = row.get(0);
        for secret in [
            "PRIVATE_PURPOSE_SENTINEL",
            "PRIVATE_BINDING_SENTINEL",
            "PRIVATE_INPUT_SENTINEL",
        ] {
            assert!(!key.contains(secret));
            assert!(
                !ciphertext
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes())
            );
        }
    }
    let stored = || {
        client().query("SELECT item_key,row_revision,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 ORDER BY kind,item_key", &[&n]).unwrap().into_iter().map(|row| (row.get::<_,String>(0),row.get::<_,i64>(1),row.get::<_,Vec<u8>>(2),row.get::<_,Vec<u8>>(3))).collect::<Vec<_>>()
    };
    let unchanged = stored();
    state.sequence += 1;
    store.compare_and_swap(&n, "owner", 1, &state).unwrap();
    assert_eq!(
        stored(),
        unchanged,
        "unrelated progress must not rewrite catalog envelopes"
    );
    store.release(&n, "owner").unwrap();
    drop(store);
    let rotated = PostgresStorage::connect_protected(&database(), keys("new")).unwrap();
    rotated.acquire(&n, "owner").unwrap();
    let before = rotated
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    rotated.reencrypt_namespace(&n, "owner").unwrap();
    let after = rotated
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    assert_eq!(after.stamp, before.stamp);
    assert!(after.state_revision > before.state_revision);
    {
        let mut conn = rotated.connection().unwrap();
        let mut tx = conn.client.transaction().unwrap();
        tx.query_one(WRITE_MARKER_SQL, &[&n]).unwrap();
        tx.execute("UPDATE aq_playbooks.catalog_rows SET encrypted_bytes=set_byte(encrypted_bytes,0,get_byte(encrypted_bytes,0)#1) WHERE namespace=$1",&[&n]).unwrap();
        tx.commit().unwrap();
    }
    assert!(
        rotated
            .inspect_catalog(
                &n,
                "owner",
                &CatalogQuery::first(CatalogKind::Definitions, 25)
            )
            .is_err()
    );
}

#[test]
fn lost_connection_and_successor_incarnation_cannot_use_cached_catalog() {
    let (store, n, _) = seeded("catalog-takeover", true);
    let page = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    let pid: i32 = client()
        .query_one(
            "SELECT owner_backend_pid FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n],
        )
        .unwrap()
        .get(0);
    client()
        .query_one("SELECT pg_terminate_backend($1)", &[&pid])
        .unwrap();
    assert!(
        store
            .inspect_catalog(
                &n,
                "owner",
                &CatalogQuery::first(CatalogKind::Definitions, 25)
            )
            .is_err()
    );
    let next = configured_store(true);
    next.acquire(&n, "owner").unwrap();
    let stale = CatalogQuery {
        expected: Some(page.stamp.clone()),
        ..CatalogQuery::first(CatalogKind::Definitions, 25)
    };
    assert_eq!(
        next.inspect_catalog(&n, "owner", &stale).unwrap_err(),
        CatalogError::Conflict
    );
    assert!(
        next.inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25)
        )
        .unwrap()
        .stamp
        .incarnation
            > page.stamp.incarnation
    );
}

#[test]
fn actual_committed_reply_loss_holds_catalog_until_verified_boundary() {
    let (store, n, mut state) = seeded("catalog-unknown", true);
    let before = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Activations, 25),
        )
        .unwrap();
    state
        .bindings
        .get_mut("PRIVATE_BINDING_SENTINEL")
        .unwrap()
        .revision += 1;
    store
        .lose_catalog_commit_reply
        .store(true, Ordering::SeqCst);
    assert!(store.compare_and_swap(&n, "owner", 1, &state).is_err());
    assert_eq!(
        store
            .inspect_catalog(
                &n,
                "owner",
                &CatalogQuery::first(CatalogKind::Activations, 25)
            )
            .unwrap_err(),
        CatalogError::Ownership
    );
    // A verified CAS under the same actual owner can restore read certainty. It
    // does not turn this inspection into a receipt for the uncertain mutation.
    store.compare_and_swap(&n, "owner", 2, &state).unwrap();
    let stale = CatalogQuery {
        expected: Some(before.stamp),
        ..CatalogQuery::first(CatalogKind::Activations, 25)
    };
    assert_eq!(
        store.inspect_catalog(&n, "owner", &stale).unwrap_err(),
        CatalogError::Conflict
    );
    let page = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Activations, 25),
        )
        .unwrap();
    let CatalogRecord::Activation(binding) = &page.items[0].record else {
        panic!("activation")
    };
    assert_eq!(binding.revision, 2);
}

#[test]
fn projection_writes_roll_back_with_failed_state_commit_and_null_heads_are_refused() {
    let (store, n, mut state) = seeded("catalog-rollback", true);
    let before = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Activations, 25),
        )
        .unwrap();
    let bytes:Vec<u8>=client().query_one("SELECT encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind='activations'",&[&n]).unwrap().get(0);
    state
        .bindings
        .get_mut("PRIVATE_BINDING_SENTINEL")
        .unwrap()
        .revision += 1;
    store
        .fail_catalog_before_snapshot
        .store(true, Ordering::SeqCst);
    assert!(store.compare_and_swap(&n, "owner", 1, &state).is_err());
    let query = CatalogQuery {
        expected: Some(before.stamp.clone()),
        ..CatalogQuery::first(CatalogKind::Activations, 25)
    };
    let after = store.inspect_catalog(&n, "owner", &query).unwrap();
    assert_eq!(after.items, before.items);
    assert_eq!(after.state_revision, before.state_revision);
    assert_eq!(client().query_one("SELECT encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind='activations'",&[&n]).unwrap().get::<_,Vec<u8>>(0),bytes);
    let mut db = client();
    let mut tx = support::marked(&mut db, &n);
    assert!(
        tx.execute(
            "UPDATE aq_playbooks.namespaces SET catalog_metadata_revision=NULL WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
}

#[test]
fn byte_pages_are_bounded_over_private_history_and_no_snapshot_read_is_needed() {
    let (store, n, mut state) = seeded("catalog-bounds", true);
    for index in 0..26 {
        let mut value = artifact(&format!("large-{index}"));
        value.content.playbook.purpose = "\0".repeat(4096);
        let value = Artifact::seal(value.content).unwrap();
        state.artifacts.insert(value.digest.clone(), value);
    }
    state.runs.get_mut("parent").unwrap().input =
        serde_json::json!("PRIVATE_HISTORY_SENTINEL".repeat(20000));
    store.compare_and_swap(&n, "owner", 1, &state).unwrap();
    store.set_profiling(true);
    let mut query = CatalogQuery::first(CatalogKind::Definitions, 25);
    let mut seen = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = store.inspect_catalog(&n, "owner", &query).unwrap();
        pages += 1;
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_CATALOG_BYTES);
        assert!(
            !serde_json::to_string(&page)
                .unwrap()
                .contains("PRIVATE_HISTORY_SENTINEL")
        );
        for item in &page.items {
            assert!(seen.insert(item.key.clone()));
        }
        let Some(cursor) = page.next_cursor else {
            break;
        };
        query.after = Some(cursor);
        query.expected = Some(page.stamp);
        assert!(pages < 5);
    }
    assert_eq!(seen.len(), 27);
    let profile = store.profiling_snapshot().unwrap();
    assert!(!profile.operations.contains_key("load"));
    assert!(!profile.operations.contains_key("cas"));
    assert_eq!(profile.operations["catalog_read"].artifact_verifications, 0);
    assert_eq!(profile.operations["catalog_read"].completed, pages);
}

#[test]
fn restricted_role_can_read_catalog_but_cannot_read_snapshot_or_artifact_columns() {
    let (store, n, _) = seeded("catalog-role", true);
    let role = namespace("catalogrole");
    assert!(
        role.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    let mut admin = client();
    admin.batch_execute(&format!("CREATE ROLE {role} NOLOGIN; GRANT USAGE ON SCHEMA aq_playbooks TO {role}; GRANT SELECT(namespace,revision,owner,incarnation,owner_backend_pid,writer_format,catalog_projected_revision,catalog_metadata_revision,catalog_supported,protection_key_id,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count),UPDATE(owner) ON aq_playbooks.namespaces TO {role}; GRANT SELECT ON aq_playbooks.catalog_rows TO {role};")).unwrap();
    {
        store
            .connection()
            .unwrap()
            .client
            .batch_execute(&format!("SET ROLE {role}"))
            .unwrap();
    }
    let page = store.inspect_catalog(
        &n,
        "owner",
        &CatalogQuery::first(CatalogKind::Definitions, 25),
    );
    let snapshot = store.load(&n);
    {
        store
            .connection()
            .unwrap()
            .client
            .batch_execute("RESET ROLE")
            .unwrap();
    }
    admin
        .batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE {role};"))
        .unwrap();
    assert_eq!(page.unwrap().items.len(), 1);
    assert!(snapshot.is_err());
}

#[test]
fn database_fences_format1_writer_before_state_or_projection_can_change() {
    let (store, n, _) = seeded("catalog-old-writer", false);
    let mut old = client();
    let before = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    let mut tx = old.transaction().unwrap();
    tx.query_one("SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','1',true)",&[&n]).unwrap();
    assert!(tx.execute("UPDATE aq_playbooks.namespaces SET revision=revision+1,writer_format=1 WHERE namespace=$1",&[&n]).is_err());
    drop(tx);
    let after = store
        .inspect_catalog(
            &n,
            "owner",
            &CatalogQuery::first(CatalogKind::Definitions, 25),
        )
        .unwrap();
    assert_eq!(after.state_revision, before.state_revision);
    assert_eq!(after.stamp, before.stamp);
    assert_eq!(after.items, before.items);
}

#[test]
fn owner_cannot_move_foreign_namespace_rows_or_rebind_catalog_addresses() {
    let (owner, own, _) = seeded("catalog-own-identity", true);
    let (foreign, other, _) = seeded("catalog-foreign-identity", true);
    let mut conn = owner.connection().unwrap();
    for (sql, values) in [
        (
            "UPDATE aq_playbooks.catalog_rows SET namespace=$1 WHERE namespace=$2",
            vec![&own, &other],
        ),
        (
            "UPDATE aq_playbooks.catalog_rows SET kind='activations' WHERE namespace=$1 AND kind='definitions'",
            vec![&own],
        ),
        (
            "UPDATE aq_playbooks.catalog_rows SET item_key='sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff' WHERE namespace=$1 AND kind='definitions'",
            vec![&own],
        ),
    ] {
        let mut tx = conn.client.transaction().unwrap();
        tx.query_one(WRITE_MARKER_SQL, &[&own]).unwrap();
        let params: Vec<&(dyn postgres::types::ToSql + Sync)> =
            values.into_iter().map(|value| value as _).collect();
        assert!(tx.execute(sql, &params).is_err());
    }
    drop(conn);
    for (store, namespace) in [(owner, own), (foreign, other)] {
        assert_eq!(
            store
                .inspect_catalog(
                    &namespace,
                    "owner",
                    &CatalogQuery::first(CatalogKind::Definitions, 25)
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }
}

#[test]
fn missing_rows_refuse_reads_and_cas_without_repair_and_counts_are_guarded() {
    let (store, n, state) = seeded("catalog-missing-row", true);
    let query = CatalogQuery::first(CatalogKind::Definitions, 25);
    let before = store.inspect_catalog(&n, "owner", &query).unwrap();
    let head = || {
        client().query_one("SELECT revision,catalog_projected_revision,catalog_definition_count,catalog_actual_definition_count,catalog_activation_count,catalog_actual_activation_count FROM aq_playbooks.namespaces WHERE namespace=$1", &[&n]).unwrap()
    };
    let original = head();
    for column in [
        "revision",
        "catalog_projected_revision",
        "catalog_definition_count",
        "catalog_actual_definition_count",
        "catalog_activation_count",
        "catalog_actual_activation_count",
    ] {
        assert_eq!(original.get::<_, i64>(column), 1);
    }
    {
        let mut conn = store.connection().unwrap();
        // Row and trigger-maintained count disappear together on rollback.
        let mut tx = conn.client.transaction().unwrap();
        tx.query_one(WRITE_MARKER_SQL, &[&n]).unwrap();
        tx.execute(
            "DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind='definitions'",
            &[&n],
        )
        .unwrap();
        assert_eq!(tx.query_one("SELECT catalog_actual_definition_count FROM aq_playbooks.namespaces WHERE namespace=$1", &[&n]).unwrap().get::<_,i64>(0),0);
        tx.rollback().unwrap();
        // TRUNCATE cannot bypass row transitions, even from the live owner.
        let mut tx = conn.client.transaction().unwrap();
        tx.query_one(WRITE_MARKER_SQL, &[&n]).unwrap();
        assert!(
            tx.batch_execute("TRUNCATE aq_playbooks.catalog_rows")
                .is_err()
        );
    }
    assert_eq!(
        store.inspect_catalog(&n, "owner", &query).unwrap().items,
        before.items
    );
    {
        let mut conn = store.connection().unwrap();
        let mut tx = conn.client.transaction().unwrap();
        tx.query_one(WRITE_MARKER_SQL, &[&n]).unwrap();
        tx.execute(
            "DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind='definitions'",
            &[&n],
        )
        .unwrap();
        tx.commit().unwrap();
        // A direct counter/header reset cannot disguise the missing row.
        for sql in [
            "UPDATE aq_playbooks.namespaces SET catalog_actual_definition_count=1 WHERE namespace=$1",
            "UPDATE aq_playbooks.namespaces SET catalog_definition_count=0 WHERE namespace=$1",
        ] {
            let mut tx = conn.client.transaction().unwrap();
            tx.query_one(WRITE_MARKER_SQL, &[&n]).unwrap();
            assert!(tx.execute(sql, &[&n]).is_err());
        }
    }
    assert!(
        matches!(store.inspect_catalog(&n, "owner", &query),Err(CatalogError::Backend(reason)) if reason.contains("row count mismatch"))
    );
    assert!(
        matches!(store.compare_and_swap(&n,"owner",1,&state),Err(StorageError::Backend(reason)) if reason.contains("row count mismatch"))
    );
    let missing = head();
    assert_eq!(missing.get::<_, i64>("revision"), 1);
    assert_eq!(missing.get::<_, i64>("catalog_definition_count"), 1);
    assert_eq!(missing.get::<_, i64>("catalog_actual_definition_count"), 0);
    assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind='definitions'", &[&n]).unwrap().get::<_,i64>(0),0, "CAS must refuse rather than repair missing rows");
}
