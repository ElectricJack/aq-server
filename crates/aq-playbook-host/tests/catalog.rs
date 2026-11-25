//! Additive typed catalog owner messages retain the legacy request wire surface.
use aq_playbook_host::{
    CatalogCallError, Error, Limits, Owner, Request,
    runner::{CatalogError, CatalogKind, CatalogQuery, Storage},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
mod common;
const DEADLINE: Duration = Duration::from_secs(5);
#[path = "common/observed.rs"]
mod observed;
use observed::{Observed, Probe};
#[derive(Clone, Default)]
struct CatalogProbe {
    loads: Arc<AtomicUsize>,
    unsupported: Arc<AtomicBool>,
}
impl Probe for CatalogProbe {
    fn loaded(&self) {
        self.loads.fetch_add(1, Ordering::SeqCst);
    }
    fn supports_catalog(&self) -> bool {
        !self.unsupported.load(Ordering::SeqCst)
    }
}
type Counted = Observed<CatalogProbe>;
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
    let digest = install_current(&owner).await;
    let loads = observed.probe.loads.load(Ordering::SeqCst);
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
        observed.probe.loads.load(Ordering::SeqCst),
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
    observed.probe.unsupported.store(true, Ordering::SeqCst);
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
struct Opaque;
impl Probe for Opaque {}
#[tokio::test]
async fn unsupported_backend_is_not_advertised_and_legacy_request_json_stays_closed() {
    let owner = Owner::start("opaque-owner".into(), Limits::default(), || {
        Ok(Observed::<Opaque>::default())
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

async fn install_current(owner: &Owner) -> String {
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
    digest
}
