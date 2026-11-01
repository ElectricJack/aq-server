#![cfg(feature = "postgres-tests")]
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
#[allow(dead_code)]
mod support;
use support::*;
fn authority() -> Authority {
    Authority {
        actor: "operator".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    }
}
fn protected() -> PostgresStorage {
    PostgresStorage::connect_protected(
        &database(),
        ProtectionKeyring::new("test", BTreeMap::from([("test".into(), [7; 32])])).unwrap(),
    )
    .unwrap()
}
#[test]
fn protected_keyed_receipt_survives_restart_and_cannot_be_rewritten() {
    let n = namespace("keyed");
    let e = Engine::new(protected(), &n, "one").unwrap();
    let a = artifact("keyed");
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate("test", &digest, authority()).unwrap();
    let r = KeyedAdmissionRequest {
        key: "operation-one".into(),
        binding: "test".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["test"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    };
    let first = e.admit_once(r.clone(), 1).unwrap();
    e.release().unwrap();
    drop(e);
    let restarted = Engine::new(protected(), &n, "two").unwrap();
    assert_eq!(
        restarted.admit_once(r.clone(), 99).unwrap().receipt,
        first.receipt
    );
    assert_eq!(restarted.snapshot().unwrap().runs.len(), 1);
    let mut changed = r;
    changed.authority.actor = "foreign".into();
    assert!(matches!(
        restarted.admit_once(changed, 100),
        Err(Error::Conflict(_))
    ));
    let snapshot = restarted.store.load(&n).unwrap();
    let mut clear = snapshot.state;
    clear.keyed_admissions.clear();
    assert!(
        restarted
            .store
            .compare_and_swap(&n, "two", snapshot.revision, &clear)
            .is_err()
    );
    let row=client().query_one("SELECT snapshot,encrypted_snapshot,writer_format FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n]).unwrap();
    assert!(row.get::<_, Option<Value>>(0).is_none());
    assert!(!row.get::<_, Option<Vec<u8>>>(1).unwrap().is_empty());
    assert_eq!(row.get::<_, i32>(2), 2);
}
#[test]
fn already_open_unmarked_writer_is_fenced_and_transaction_marker_cannot_leak() {
    // Hold a client before schema/format enrollment, representing an old live
    // connection. Execute the old CAS SQL shape after a new writer enrolls.
    let mut old = client();
    let n = namespace("old-writer");
    let e = Engine::new(protected(), &n, "owner").unwrap();
    let revision = e.store.load(&n).unwrap().revision;
    assert!(
        old.execute(
            "UPDATE aq_playbooks.namespaces SET revision=revision+1 WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
    assert_eq!(e.store.load(&n).unwrap().revision, revision);
    // A correct marker is namespace-specific AND transaction-local, not sticky.
    let mut tx = old.transaction().unwrap();
    tx.query_one("SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','2',true)",&[&n]).unwrap();
    tx.execute(
        "UPDATE aq_playbooks.namespaces SET updated_at=updated_at WHERE namespace=$1",
        &[&n],
    )
    .unwrap();
    tx.commit().unwrap();
    assert!(
        old.execute(
            "UPDATE aq_playbooks.namespaces SET updated_at=updated_at WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
    let mut tx = old.transaction().unwrap();
    tx.query_one("SELECT set_config('aq_playbooks.writer_namespace','wrong-namespace',true),set_config('aq_playbooks.writer_format','2',true)",&[]).unwrap();
    assert!(
        tx.execute(
            "UPDATE aq_playbooks.namespaces SET updated_at=updated_at WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
    drop(tx);
    // Even a marked new transaction cannot downgrade or delete the receipt owner.
    let mut tx = old.transaction().unwrap();
    tx.query_one("SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','2',true)",&[&n]).unwrap();
    assert!(
        tx.execute(
            "UPDATE aq_playbooks.namespaces SET writer_format=0 WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
    drop(tx);
    assert!(
        old.execute(
            "DELETE FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n]
        )
        .is_err()
    );
    assert_eq!(e.store.load(&n).unwrap().revision, revision);
}
