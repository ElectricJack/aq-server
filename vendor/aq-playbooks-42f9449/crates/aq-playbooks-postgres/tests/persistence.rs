#![cfg(feature = "postgres-tests")]
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

mod support;
use support::*;

#[test]
fn exclusive_connection_ownership_and_incarnations() {
    let n = namespace("ownership");
    let a = store();
    let b = store();
    assert_eq!(a.load(&n).unwrap().revision, 0);
    a.acquire(&n, "owner").unwrap();
    a.acquire(&n, "owner").unwrap();
    let first = a.incarnation(&n).unwrap().unwrap();
    assert!(matches!(
        b.acquire(&n, "owner"),
        Err(StorageError::Ownership)
    ));
    assert!(matches!(
        b.acquire(&n, "other"),
        Err(StorageError::Ownership)
    ));
    assert!(matches!(
        a.release(&n, "wrong"),
        Err(StorageError::Ownership)
    ));
    b.acquire(&namespace("different_namespace"), "owner")
        .unwrap();
    a.release(&n, "owner").unwrap();
    assert!(matches!(
        a.compare_and_swap(&n, "owner", 0, &State::default()),
        Err(StorageError::Ownership)
    ));
    b.acquire(&n, "replacement").unwrap();
    assert_eq!(b.incarnation(&n).unwrap(), Some(first + 1));
    b.compare_and_swap(&n, "replacement", 0, &State::default())
        .unwrap();
}

#[test]
fn concurrent_compare_and_swap_has_one_winner() {
    let n = namespace("cas");
    let s = Arc::new(store());
    s.acquire(&n, "owner").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = (1..=2)
        .map(|value| {
            let s = s.clone();
            let n = n.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let state = State {
                    sequence: value,
                    ..State::default()
                };
                barrier.wait();
                s.compare_and_swap(&n, "owner", 0, &state)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(StorageError::Conflict)))
            .count(),
        1
    );
    assert_eq!(s.load(&n).unwrap().revision, 1);
}

#[test]
fn complete_boundary_survives_reopen_and_artifacts_are_stored_once() {
    let n = namespace("roundtrip");
    let expected = boundary(&n);
    let digest = expected.artifacts.keys().next().unwrap();
    {
        let s = store();
        s.acquire(&n, "first").unwrap();
        assert_eq!(s.compare_and_swap(&n, "first", 0, &expected).unwrap(), 1);
    }
    let s = store();
    s.acquire(&n, "second").unwrap();
    let loaded = s.load(&n).unwrap();
    assert_eq!(loaded.revision, 1);
    assert_same(&loaded.state, &expected);
    let mut db = client();
    let row=db.query_one("SELECT snapshot->'artifacts', (SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$2) FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n,&digest]).unwrap();
    assert_eq!(row.get::<_, Value>(0), json!({}));
    assert_eq!(row.get::<_, i64>(1), 1);
    let other = namespace("same_artifact");
    s.acquire(&other, "second").unwrap();
    s.compare_and_swap(&other, "second", 0, &expected).unwrap();
    let row = db
        .query_one(
            "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
            &[&digest],
        )
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
}

#[test]
fn artifacts_cannot_be_replaced_removed_or_corrupted() {
    let n = namespace("immutability");
    let expected = boundary(&n);
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    let mut changed = expected.clone();
    changed.artifacts.clear();
    assert!(s.compare_and_swap(&n, "owner", 1, &changed).is_err());
    changed = expected.clone();
    changed
        .artifacts
        .values_mut()
        .next()
        .unwrap()
        .content
        .playbook
        .purpose = "changed".into();
    assert!(s.compare_and_swap(&n, "owner", 1, &changed).is_err());
    let digest = expected.artifacts.keys().next().unwrap();
    assert!(
        client()
            .execute(
                "UPDATE aq_playbooks.artifacts SET canonical_bytes='bad'::bytea WHERE digest=$1",
                &[&digest]
            )
            .is_err()
    );
    assert!(
        client()
            .execute(
                "DELETE FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&digest]
            )
            .is_err()
    );
    let loaded = s.load(&n).unwrap();
    assert_eq!(loaded.revision, 1);
    assert_same(&loaded.state, &expected);
}

#[test]
fn failure_after_artifact_inserts_rolls_back_the_entire_boundary() {
    let n = namespace("rollback");
    let s = store();
    s.acquire(&n, "owner").unwrap();
    let expected = boundary(&n);
    let digest = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    // A real PostgreSQL failure at the last write proves preceding blob and
    // reference writes roll back along with receipts, children and intent state.
    db.batch_execute(&format!("CREATE FUNCTION aq_playbooks.{n}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.namespace='{n}' THEN RAISE EXCEPTION 'injected boundary failure'; END IF; RETURN NEW; END; $$; CREATE TRIGGER {n} BEFORE UPDATE OF snapshot ON aq_playbooks.namespaces FOR EACH ROW EXECUTE FUNCTION aq_playbooks.{n}();")).unwrap();
    assert!(s.compare_and_swap(&n, "owner", 0, &expected).is_err());
    db.batch_execute(&format!(
        "DROP TRIGGER {n} ON aq_playbooks.namespaces; DROP FUNCTION aq_playbooks.{n}();"
    ))
    .unwrap();
    let loaded = s.load(&n).unwrap();
    assert_eq!(loaded.revision, 0);
    assert_same(&loaded.state, &State::default());
    assert_eq!(
        db.query_one(
            "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
            &[&digest]
        )
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    assert_eq!(s.compare_and_swap(&n, "owner", 0, &expected).unwrap(), 1);
    assert_same(&s.load(&n).unwrap().state, &expected);
}

#[test]
fn terminated_database_session_cannot_continue_writing() {
    let n = namespace("lost_session");
    let s = store();
    s.acquire(&n, "owner").unwrap();
    let replacement = store();
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
        s.compare_and_swap(&n, "owner", 0, &State::default())
            .is_err()
    );
    replacement.acquire(&n, "replacement").unwrap();
    assert_eq!(replacement.incarnation(&n).unwrap(), Some(2));
}

#[test]
fn abrupt_process_helper() {
    let Ok(n) = std::env::var("AQ_PLAYBOOKS_CRASH_HELPER_NAMESPACE") else {
        return;
    };
    let s = store();
    s.acquire(&n, "crashing-runtime").unwrap();
    s.compare_and_swap(&n, "crashing-runtime", 0, &boundary(&n))
        .unwrap();
    let mut db = client();
    db.batch_execute("BEGIN").unwrap();
    db.execute("UPDATE aq_playbooks.namespaces SET revision=2,snapshot=jsonb_set(snapshot,'{sequence}','999') WHERE namespace=$1",&[&n]).unwrap();
    println!("CRASH_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}

#[test]
fn abrupt_process_death_preserves_commit_and_rolls_back_inflight_transaction() {
    let n = namespace("process_crash");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "abrupt_process_helper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AQ_PLAYBOOKS_CRASH_HELPER_NAMESPACE", &n)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match output.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line.contains("CRASH_READY") {
                        let _ = ready_tx.send(());
                        break;
                    }
                }
            }
        }
    });
    if ready_rx.recv_timeout(Duration::from_secs(15)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("crash helper failed to establish its committed boundary within 15 seconds");
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    let reopened = store();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match reopened.acquire(&n, "recovery-runtime") {
            Ok(()) => break,
            Err(StorageError::Ownership) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(e) => panic!("reacquire after process death: {e}"),
        }
    }
    let loaded = reopened.load(&n).unwrap();
    assert_eq!(loaded.revision, 1);
    assert_same(&loaded.state, &boundary(&n));
    assert_eq!(reopened.incarnation(&n).unwrap(), Some(2));
}
