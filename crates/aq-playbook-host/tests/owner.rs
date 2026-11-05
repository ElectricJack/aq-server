use aq_playbook_host::{Error, Limits as HostLimits, Owner, Request};
use aq_playbooks_model::*;
use aq_playbooks_runner::{MemoryStorage, State, Storage, StorageError, VersionedState};

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
    owner
        .call(Request::Tick { now_ms: 2 }, DEADLINE)
        .await
        .unwrap();
    assert_eq!(
        inspect.load("pure").unwrap().state.runs[&run].status,
        aq_playbooks_runner::RunStatus::Completed
    );
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
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
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
#[derive(Clone)]
struct Blocked {
    store: MemoryStorage,
    gate: Gate,
}
impl Storage for Blocked {
    fn load(&self, n: &str) -> Result<VersionedState, StorageError> {
        self.store.load(n)
    }
    fn compare_and_swap(&self, n: &str, o: &str, v: u64, s: &State) -> Result<u64, StorageError> {
        self.store.compare_and_swap(n, o, v, s)
    }
    fn acquire(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.store.acquire(n, o)
    }
    fn release(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.store.release(n, o)
    }
    fn query(
        &self,
        n: &str,
        q: &aq_playbooks_runner::IndexQuery,
    ) -> Result<aq_playbooks_runner::IndexPage, StorageError> {
        self.gate.block();
        self.store.query(n, q)
    }
}
#[tokio::test(flavor = "current_thread")]
async fn blocked_storage_does_not_block_async_control_and_control_queue_is_reserved() {
    let gate = Gate::default();
    let backend = Blocked {
        store: MemoryStorage::default(),
        gate: gate.clone(),
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
    for _ in 0..100 {
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
        tokio::task::yield_now().await;
    }
    panic!("queued stop did not retire owner");
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_last_handle_never_joins_stalled_owner_on_async_thread() {
    let gate = Gate::default();
    let backend = Blocked {
        store: MemoryStorage::default(),
        gate: gate.clone(),
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

#[tokio::test]
async fn engine_admission_capacity_is_typed_and_keeps_existing_work() {
    use aq_playbooks_runner::KeyedAdmissionRequest;
    let store = MemoryStorage::default();
    let inspect = store.clone();
    let mut limits = HostLimits::default();
    limits.engine.max_runs = 1;
    let owner = Owner::start("bounded-admissions".into(), limits, move || Ok(store))
        .await
        .unwrap();
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
    let request = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "bounded".into(),
        expected_activation_revision: 1,
        entry: "main".into(),
        input: serde_json::Value::Null,
        authority: common::authority(),
    };
    let first = owner
        .call(
            Request::AdmitOnce {
                request: Box::new(request("one")),
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    assert!(matches!(
        owner
            .call(
                Request::AdmitOnce {
                    request: Box::new(request("two")),
                    now_ms: 2
                },
                DEADLINE
            )
            .await,
        Err(Error::Overloaded)
    ));
    let replay = owner
        .call(
            Request::AdmitOnce {
                request: Box::new(request("one")),
                now_ms: 3,
            },
            DEADLINE,
        )
        .await
        .unwrap();
    assert_eq!(first["receipt"], replay["receipt"]);
    assert_eq!(
        inspect.load("bounded-admissions").unwrap().state.runs.len(),
        1
    );
    owner.call(Request::Stop, DEADLINE).await.unwrap();
}
