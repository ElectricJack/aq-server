//! Additive typed catalog owner messages retain the legacy request wire surface.
use aq_playbook_host::{
    CatalogCallError, Error, Limits, Owner, Request,
    runner::{
        CatalogError, CatalogKind, CatalogPage, CatalogQuery, MemoryStorage, State, Storage,
        StorageError, VersionedState,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
mod common;
const DEADLINE: Duration = Duration::from_secs(5);
#[derive(Clone, Default)]
struct Counted {
    inner: MemoryStorage,
    loads: Arc<AtomicUsize>,
    unsupported: Arc<AtomicBool>,
}
impl Storage for Counted {
    fn load(&self, n: &str) -> Result<VersionedState, StorageError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load(n)
    }
    fn acquire(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.acquire(n, o)
    }
    fn release(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.release(n, o)
    }
    fn compare_and_swap(&self, n: &str, o: &str, r: u64, s: &State) -> Result<u64, StorageError> {
        self.inner.compare_and_swap(n, o, r, s)
    }
    fn inspect_catalog(
        &self,
        n: &str,
        o: &str,
        q: &CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if self.unsupported.load(Ordering::SeqCst) {
            Err(CatalogError::Unsupported)
        } else {
            self.inner.inspect_catalog(n, o, q)
        }
    }
}
#[tokio::test]
async fn current_queries_delegate_without_snapshot_and_follow_actual_installation() {
    let store = Counted::default();
    let observed = store.clone();
    let owner = Owner::start("catalog-owner".into(), Limits::default(), move || Ok(store))
        .await
        .unwrap();
    assert!(owner.catalog_supported());
    let initial = owner
        .inspect_catalog(CatalogQuery::first(CatalogKind::Definitions, 25), DEADLINE)
        .await
        .unwrap();
    assert!(initial.items.is_empty());
    let artifact = common::artifact();
    let digest = artifact.digest.clone();
    owner
        .call(
            Request::Install {
                artifact: Box::new(artifact),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Activate {
                binding: "current-binding".into(),
                artifact: digest.clone(),
                authority: common::authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    let loads = observed.loads.load(Ordering::SeqCst);
    for _ in 0..2 {
        assert!(owner.catalog_supported());
        let page = owner
            .inspect_catalog(CatalogQuery::first(CatalogKind::Definitions, 25), DEADLINE)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].key, digest);
        assert!(page.stamp.metadata_revision > initial.stamp.metadata_revision);
        let bindings = owner
            .inspect_catalog(CatalogQuery::first(CatalogKind::Activations, 25), DEADLINE)
            .await
            .unwrap();
        assert_eq!(bindings.items.len(), 1);
    }
    assert_eq!(
        observed.loads.load(Ordering::SeqCst),
        loads,
        "neither support checks nor catalog reads may load State"
    );
    assert!(
        observed
            .inner
            .load("catalog-owner")
            .unwrap()
            .state
            .runs
            .is_empty()
    );
    observed.unsupported.store(true, Ordering::SeqCst);
    assert!(
        owner.catalog_supported(),
        "interface support is not cached current data/health"
    );
    assert!(matches!(
        owner
            .inspect_catalog(CatalogQuery::first(CatalogKind::Definitions, 25), DEADLINE)
            .await,
        Err(CatalogCallError::Catalog(CatalogError::Unsupported))
    ));
    owner.call(Request::Stop, DEADLINE).await.unwrap();
    assert!(!owner.catalog_supported());
}
#[derive(Default)]
struct Opaque(MemoryStorage);
impl Storage for Opaque {
    fn load(&self, n: &str) -> Result<VersionedState, StorageError> {
        self.0.load(n)
    }
    fn acquire(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.0.acquire(n, o)
    }
    fn release(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.0.release(n, o)
    }
    fn compare_and_swap(&self, n: &str, o: &str, r: u64, s: &State) -> Result<u64, StorageError> {
        self.0.compare_and_swap(n, o, r, s)
    }
}
#[tokio::test]
async fn unsupported_backend_is_not_advertised_and_legacy_request_json_stays_closed() {
    let owner = Owner::start("opaque-owner".into(), Limits::default(), || {
        Ok(Opaque::default())
    })
    .await
    .unwrap();
    assert!(!owner.catalog_supported());
    assert!(matches!(
        owner
            .inspect_catalog(CatalogQuery::first(CatalogKind::Definitions, 1), DEADLINE)
            .await,
        Err(CatalogCallError::Catalog(CatalogError::Unsupported))
    ));
    assert!(
        serde_json::from_value::<Request>(serde_json::json!({"operation":"catalog","query":{}}))
            .is_err()
    );
    assert!(matches!(
        serde_json::from_value::<Request>(serde_json::json!({"operation":"stop"})).unwrap(),
        Request::Stop
    ));
    owner.call(Request::Stop, DEADLINE).await.unwrap();
    assert!(matches!(
        owner.call(Request::Tick { now_ms: 1 }, DEADLINE).await,
        Err(Error::Stopped)
    ));
}

#[tokio::test]
async fn support_handle_observes_stop_and_does_not_prolong_owner_lifetime() {
    let store = Counted::default();
    let observed = store.clone();
    let owner = Owner::start("weak-support".into(), Limits::default(), move || Ok(store))
        .await
        .unwrap();
    let support = owner.catalog_support();
    assert!(support.available());
    drop(owner);
    assert!(!support.available());
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if observed.inner.acquire("weak-support", "successor").is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("weak support handle must not retain an owning client");
    observed.inner.release("weak-support", "successor").unwrap();
    assert!(!support.available());
}
