use aq_playbook_host::{Error, Limits as HostLimits, Owner, Request};
use aq_playbooks_model::*;
use aq_playbooks_runner::{IndexPage, IndexQuery, MemoryStorage, Storage, StorageError};
#[path = "common/observed.rs"]
mod observed;
use observed::{Observed, Probe};

use std::{
    collections::BTreeSet,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
mod common;
use common::{artifact, install};
const DEADLINE: Duration = Duration::from_secs(5);
#[tokio::test(flavor = "current_thread")]
async fn actual_engine_install_admit_tick_inspect_and_explicit_stop() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let owner = Owner::start("pure".into(), HostLimits::default(), move || Ok(store))
        .await
        .unwrap();
    let run = install(&owner).await;
    let live = Request::Runs {
        after: None,
        limit: 1,
    };
    let page = owner.call(live, DEADLINE).await.unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    owner
        .call(Request::Tick { now_ms: 2 }, DEADLINE)
        .await
        .unwrap();
    // A finished run leaves the State in the commit that finishes it;
    // inspection serves it from history.
    assert!(inspect.load("pure").unwrap().state.runs.get(&run).is_none());
    let page = owner
        .call(
            Request::Runs {
                after: None,
                limit: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    assert!(page["items"].as_array().unwrap().is_empty());
    let page = owner
        .call(
            Request::Inspect {
                query: aq_playbooks_runner::InspectionQuery {
                    kind: aq_playbooks_runner::InspectionKind::Runs,
                    run: Some(run.clone()),
                    after: None,
                    limit: 1,
                },
            },
            DEADLINE,
        )
        .await
        .unwrap();
    assert_eq!(page["items"][0]["id"], run.as_str());
    assert_eq!(page["items"][0]["status"], "completed");
    owner.call(Request::Stop, DEADLINE).await.unwrap();
    assert!(matches!(
        owner.call(Request::Tick { now_ms: 3 }, DEADLINE).await,
        Err(Error::Stopped)
    ));
}
#[tokio::test]
async fn unsupported_contracts_and_bounds_refused_before_install() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let owner = Owner::start("refuse".into(), HostLimits::default(), move || Ok(store))
        .await
        .unwrap();
    let mut a = artifact();
    a.content.registry.events.insert(
        "fake.event".into(),
        EventContract {
            sensitive_payload_fields: BTreeSet::new(),
            name: "fake.event".into(),
            version: "1".into(),
            payload: Schema::Null,
            trusted_producers: BTreeSet::new(),
            scopes: BTreeSet::new(),
            correlation_fields: BTreeSet::new(),
            subject_field: None,
            revision_field: None,
            max_payload_bytes: 100,
            retention_ms: 1,
        },
    );
    assert!(matches!(
        owner
            .call(
                Request::Install {
                    artifact: Box::new(a)
                },
                DEADLINE
            )
            .await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        owner
            .call(
                Request::Runs {
                    after: None,
                    limit: 101
                },
                DEADLINE
            )
            .await,
        Err(Error::Invalid)
    ));
    assert!(inspect.load("refuse").unwrap().state.artifacts.is_empty());
}
#[derive(Clone, Default)]
struct Gate {
    entered: Arc<AtomicBool>,
    state: Arc<(Mutex<bool>, Condvar)>,
}
impl Gate {
    fn block(&self) {
        self.entered.store(true, Ordering::Release);
        let (lock, cv) = &*self.state;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
    }
    fn release(&self) {
        let (lock, cv) = &*self.state;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
}
impl Probe for Gate {
    fn query(
        &self,
        store: &MemoryStorage,
        namespace: &str,
        query: &IndexQuery,
    ) -> Result<IndexPage, StorageError> {
        self.block();
        store.query(namespace, query)
    }
}
#[tokio::test(flavor = "current_thread")]
async fn blocked_storage_does_not_block_async_control_and_control_queue_is_reserved() {
    let gate = Gate::default();
    let backend = Observed {
        inner: MemoryStorage::default(),
        probe: gate.clone(),
    };
    let owner = Owner::start(
        "blocked".into(),
        HostLimits {
            work_queue: 1,
            control_queue: 1,
            ..Default::default()
        },
        move || Ok(backend),
    )
    .await
    .unwrap();
    let blocked = start_blocked_query(&owner, &gate).await;
    // Current-thread executor remains responsive despite synchronous storage hold.
    tokio::time::timeout(Duration::from_millis(100), async {
        tokio::task::yield_now().await;
    })
    .await
    .unwrap();
    assert!(matches!(
        owner
            .call(
                Request::Runs {
                    after: None,
                    limit: 1
                },
                Duration::from_millis(10)
            )
            .await,
        Err(Error::Unknown)
    ));
    assert!(matches!(
        owner
            .call(
                Request::Runs {
                    after: None,
                    limit: 1
                },
                DEADLINE
            )
            .await,
        Err(Error::Overloaded)
    ));
    assert!(matches!(
        owner.call(Request::Stop, Duration::from_millis(10)).await,
        Err(Error::Unknown)
    ));
    // Control remains queued independently but cannot interrupt the blocked call.
    assert!(matches!(
        owner.call(Request::Stop, DEADLINE).await,
        Err(Error::Overloaded)
    ));
    gate.release();
    blocked.await.unwrap().unwrap();
    wait_until_retired(&owner).await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_last_handle_never_joins_stalled_owner_on_async_thread() {
    let gate = Gate::default();
    let backend = Observed {
        inner: MemoryStorage::default(),
        probe: gate.clone(),
    };
    let owner = Owner::start("drop".into(), HostLimits::default(), move || Ok(backend))
        .await
        .unwrap();
    assert!(matches!(
        owner
            .call(
                Request::Runs {
                    after: None,
                    limit: 1
                },
                Duration::from_millis(10)
            )
            .await,
        Err(Error::Unknown)
    ));
    assert!(gate.entered.load(Ordering::Acquire));
    let started = std::time::Instant::now();
    drop(owner);
    assert!(started.elapsed() < Duration::from_millis(100));
    // The held call is still real. Release it explicitly for deterministic cleanup.
    gate.release();
}

#[tokio::test]
async fn changed_artifact_bytes_and_large_request_never_enter_namespace() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let owner = Owner::start("integrity".into(), HostLimits::default(), move || Ok(store))
        .await
        .unwrap();
    let mut changed = artifact();
    changed.content.playbook.purpose = "changed after seal".into();
    assert!(matches!(
        owner
            .call(
                Request::Install {
                    artifact: Box::new(changed)
                },
                DEADLINE
            )
            .await,
        Err(Error::Engine(_))
    ));
    let mut large = artifact();
    large.content.playbook.purpose = "x".repeat(300_000);
    assert!(matches!(
        owner
            .call(
                Request::Install {
                    artifact: Box::new(large)
                },
                DEADLINE
            )
            .await,
        Err(Error::Invalid)
    ));
    assert!(
        inspect
            .load("integrity")
            .unwrap()
            .state
            .artifacts
            .is_empty()
    );
}

// There are no namespace limits: the deprecated `max_runs` is accepted and
// ignored, so a second keyed admission is admitted while the first is live,
// and a replay of the first still returns its receipt.
#[tokio::test]
async fn ignored_engine_limits_admit_new_work_and_keep_existing_work() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let mut limits = HostLimits::default();
    limits.engine.max_runs = 1;
    let owner = Owner::start("bounded-admissions".into(), limits, move || Ok(store))
        .await
        .unwrap();
    activate(&owner).await;
    let first = owner
        .call(admit_once("one", None, 1), DEADLINE)
        .await
        .unwrap();
    owner
        .call(admit_once("two", None, 2), DEADLINE)
        .await
        .unwrap();
    let replay = owner
        .call(admit_once("one", None, 3), DEADLINE)
        .await
        .unwrap();
    assert_eq!(first["receipt"], replay["receipt"]);
    assert_eq!(replay["duplicate"], true);
    assert_eq!(
        inspect.load("bounded-admissions").unwrap().state.runs.len(),
        2
    );
    owner.call(Request::Stop, DEADLINE).await.unwrap();
}

// A request for a key the engine does not remember, whose request time is
// older than the admission window, is refused as `Error::Stale` and writes
// nothing. The owner stays usable for the requests behind it.
#[tokio::test]
async fn a_stale_keyed_admission_is_typed_and_writes_nothing() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let owner = Owner::start(
        "stale-admissions".into(),
        HostLimits::default(),
        move || Ok(store),
    )
    .await
    .unwrap();
    activate(&owner).await;
    let window = inspect
        .load("stale-admissions")
        .unwrap()
        .state
        .retention
        .policy
        .admission_replay_ms as i64;
    let now = 2 * window;
    let stale = owner
        .call(admit_once("old", Some(now - window), now), DEADLINE)
        .await;
    assert!(
        matches!(&stale, Err(Error::Stale(text)) if text.starts_with("stale request")),
        "{stale:?}"
    );
    let state = inspect.load("stale-admissions").unwrap().state;
    assert!(state.runs.is_empty() && state.keyed_admissions.get("old").is_none());
    let fresh = owner
        .call(admit_once("new", Some(now - window + 1), now), DEADLINE)
        .await
        .unwrap();
    assert_eq!(fresh["duplicate"], false);
    owner.call(Request::Stop, DEADLINE).await.unwrap();
}

// The configured engine windows are written into the namespace at open; an
// open with the same values writes nothing more.
#[tokio::test]
async fn the_owner_applies_configured_event_and_admission_windows() {
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let limits = HostLimits {
        engine_retention: aq_playbook_host::engine_retention::EngineRetention {
            event_replay_ms: 600_000,
            admission_replay_ms: 600_000,
            ..Default::default()
        },
        ..HostLimits::default()
    };
    for _ in 0..2 {
        let store = store.clone();
        let owner = Owner::start("windows".into(), limits.clone(), move || Ok(store))
            .await
            .unwrap();
        let policy = inspect.load("windows").unwrap().state.retention.policy;
        assert_eq!(
            (policy.event_replay_ms, policy.admission_replay_ms),
            (600_000, 600_000)
        );
        assert_eq!((policy.replay_keep, policy.history_keep), (256, 10_000));
        owner.call(Request::Stop, DEADLINE).await.unwrap();
    }
    let zero = HostLimits {
        engine_retention: aq_playbook_host::engine_retention::EngineRetention {
            replay_keep: 0,
            ..Default::default()
        },
        ..HostLimits::default()
    };
    let refused = Owner::start("windows".into(), zero, move || Ok(store)).await;
    assert!(matches!(refused, Err(Error::Invalid)));
}

async fn activate(owner: &Owner) {
    let a = artifact();
    owner
        .call(
            Request::Install {
                artifact: Box::new(a.clone()),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Activate {
                binding: "bounded".into(),
                artifact: a.digest.clone(),
                authority: common::authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
}

fn admit_once(key: &str, requested_at_ms: Option<i64>, now_ms: i64) -> Request {
    Request::AdmitOnce {
        request: Box::new(aq_playbooks_runner::KeyedAdmissionRequest {
            key: key.into(),
            binding: "bounded".into(),
            expected_activation_revision: 1,
            entry: "main".into(),
            input: serde_json::Value::Null,
            authority: common::authority(),
        }),
        requested_at_ms,
        now_ms,
    }
}

async fn start_blocked_query(
    owner: &Owner,
    gate: &Gate,
) -> tokio::task::JoinHandle<Result<serde_json::Value, Error>> {
    let first = owner.clone();
    let blocked = tokio::spawn(async move {
        first
            .call(
                Request::Runs {
                    after: None,
                    limit: 1,
                },
                DEADLINE,
            )
            .await
    });
    while !gate.entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    blocked
}

async fn wait_until_retired(owner: &Owner) {
    // The owner thread retires asynchronously; bound the wait by time, not by
    // a count of yields, so a loaded machine (nextest runs tests in parallel
    // processes) cannot exhaust it first.
    let retire_by = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < retire_by {
        if matches!(
            owner
                .call(
                    Request::Runs {
                        after: None,
                        limit: 1
                    },
                    DEADLINE
                )
                .await,
            Err(Error::Stopped)
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("queued stop did not retire owner");
}
