//! Disposable PostgreSQL only. These tests never discover a production URL.
mod common;
use aq_playbook_host::{Limits, Owner, Request, protected};
use aq_playbooks_postgres::ProtectionKeyring;
use aq_playbooks_runner::{RunStatus, Storage};
use std::{collections::BTreeMap, time::Duration};
const TIMEOUT: Duration = Duration::from_secs(5);
fn url() -> String {
    let value = std::env::var("AQ_PLAYBOOK_TEST_DATABASE_URL")
        .expect("set AQ_PLAYBOOK_TEST_DATABASE_URL to scripts/test-db disposable URL");
    let config: value::Config = value.parse().expect("postgres config");
    assert!(
        config.get_hosts().iter().all(
            |h| matches!(h,postgres::config::Host::Tcp(s) if s=="127.0.0.1" || s=="localhost")
        )
    );
    assert_eq!(config.get_ports(), [55432]);
    value
}
mod value {
    pub type Config = postgres::Config;
}
fn backend(url: &str) -> aq_playbooks_postgres::PostgresStorage {
    let mut config: postgres::Config = url.parse().unwrap();
    config.connect_timeout(Duration::from_secs(2));
    let client = config.connect(postgres::NoTls).unwrap();
    protected::storage(
        client,
        ProtectionKeyring::new("test", BTreeMap::from([("test".into(), [7; 32])])).unwrap(),
        Duration::from_secs(2),
        Duration::from_millis(100),
    )
    .unwrap()
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires scripts/test-db disposable PostgreSQL"]
async fn protected_actual_engine_exclusive_owner_restart_and_key_failure() {
    let url = url();
    let ns = format!("aq12-{}", uuid::Uuid::new_v4());
    let connect = url.clone();
    let owner = Owner::start(ns.clone(), Limits::default(), move || Ok(backend(&connect)))
        .await
        .unwrap();
    let run = common::install(&owner).await;
    let connect = url.clone();
    assert!(
        Owner::start(ns.clone(), Limits::default(), move || Ok(backend(&connect)))
            .await
            .is_err()
    );
    owner
        .call(Request::Tick { now_ms: 2 }, TIMEOUT)
        .await
        .unwrap();
    owner.call(Request::Stop, TIMEOUT).await.unwrap();
    drop(owner);
    // Query stored protected rows directly for envelope proof, not engine data authority.
    let connect = url.clone();
    let check_ns = ns.clone();
    let expected_run = run.clone();
    tokio::task::spawn_blocking(move || {
        let store = backend(&connect);
        let snapshot = store.load(&check_ns).unwrap();
        assert_eq!(
            snapshot.state.runs[&expected_run].status,
            RunStatus::Completed
        );
    })
    .await
    .unwrap();
    let connect = url.clone();
    let restarted = Owner::start(ns.clone(), Limits::default(), move || Ok(backend(&connect)))
        .await
        .unwrap();
    let page = restarted
        .call(
            Request::Runs {
                after: None,
                limit: 10,
            },
            TIMEOUT,
        )
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    restarted.call(Request::Stop, TIMEOUT).await.unwrap();
    drop(restarted);
    tokio::task::spawn_blocking(move||{
        let mut client=postgres::Client::connect(&url,postgres::NoTls).unwrap();
        let row=client.query_one("SELECT snapshot, protection_key_id, encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",&[&ns]).unwrap();
        assert!(row.get::<_,Option<serde_json::Value>>(0).is_none());assert_eq!(row.get::<_,Option<String>>(1).as_deref(),Some("test"));assert!(!row.get::<_,Option<Vec<u8>>>(2).unwrap().is_empty());
        let wrong=aq_playbooks_postgres::PostgresStorage::from_client_protected(client,ProtectionKeyring::new("test",BTreeMap::from([("test".into(),[8;32])])).unwrap()).unwrap();assert!(wrong.load(&ns).is_err());
    }).await.unwrap();
}
