use aq_playbook_host::runner::{
    CatalogError, CatalogPage, CatalogQuery, HistoryItem, IndexPage, IndexQuery, InspectionQuery,
    MemoryStorage, State, Storage, StorageError, VersionedState,
};

pub trait Probe: Send + Sync {
    fn loaded(&self) {}
    fn supports_catalog(&self) -> bool {
        false
    }
    fn query(
        &self,
        store: &MemoryStorage,
        namespace: &str,
        query: &IndexQuery,
    ) -> Result<IndexPage, StorageError> {
        self.loaded();
        let state = store.load(namespace)?.state;
        Ok(state.indexes.query(&state, query))
    }
}

#[derive(Clone, Default)]
pub struct Observed<P> {
    pub inner: MemoryStorage,
    pub probe: P,
}
impl<P: Probe> Storage for Observed<P> {
    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError> {
        self.probe.loaded();
        self.inner.load(namespace)
    }
    fn query(&self, namespace: &str, query: &IndexQuery) -> Result<IndexPage, StorageError> {
        self.probe.query(&self.inner, namespace, query)
    }
    fn inspect_catalog(
        &self,
        namespace: &str,
        owner: &str,
        query: &CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if self.probe.supports_catalog() {
            self.inner.inspect_catalog(namespace, owner, query)
        } else {
            Err(CatalogError::Unsupported)
        }
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        self.inner.acquire(namespace, owner)
    }
    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        self.inner.release(namespace, owner)
    }
    fn persists_history(&self) -> bool {
        self.inner.persists_history()
    }
    fn history_page(
        &self,
        namespace: &str,
        query: &InspectionQuery,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        self.inner.history_page(namespace, query)
    }
    fn delete_history(
        &self,
        namespace: &str,
        owner: &str,
        before_ms: i64,
    ) -> Result<u64, StorageError> {
        self.inner.delete_history(namespace, owner, before_ms)
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        revision: u64,
        state: &State,
    ) -> Result<u64, StorageError> {
        self.inner
            .compare_and_swap(namespace, owner, revision, state)
    }
}
