//! Atomic namespace storage. A successful CAS commits *all* snapshots, receipts,
//! waits, child admissions and effect intents, or none of them. Backends must
//! serialize ingress with expiry through this same revision. External dispatch
//! is deliberately excluded from transactions.
//!
//! A commit also carries `State::settled`: the cohorts it removes. A backend
//! writes their public history items in the same atomic commit, or the commit
//! fails, and never stores `settled` with the State.
use crate::{HistoryItem, SettledCohort, State, history};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum StorageError {
    #[error("concurrent namespace update")]
    Conflict,
    #[error("namespace ownership unavailable or lost")]
    Ownership,
    #[error("storage backend: {0}")]
    Backend(String),
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VersionedState {
    pub revision: u64,
    pub state: State,
}
pub trait Storage: Send + Sync {
    /// No full-load fallback: backends/wrappers must opt into the leased bounded
    /// catalog read contract explicitly.
    fn inspect_catalog(
        &self,
        _namespace: &str,
        _owner: &str,
        _query: &crate::CatalogQuery,
    ) -> Result<crate::CatalogPage, crate::CatalogError> {
        Err(crate::CatalogError::Unsupported)
    }
    /// Live state plus history. History is read after the state, so an item
    /// that left the state in between is found in history.
    fn inspect(
        &self,
        namespace: &str,
        query: &crate::InspectionQuery,
    ) -> Result<crate::InspectionPage, StorageError> {
        let versioned = self.load(namespace)?;
        let history = self.history_page(namespace, query)?;
        crate::inspection::page_with_history(&versioned.state, versioned.revision, query, history)
    }
    /// Whether commits write `State::settled` as history. `Engine::new`
    /// refuses a backend that does not, so a wrapper that forgets to forward
    /// the history methods fails closed.
    fn persists_history(&self) -> bool {
        false
    }
    /// At most `query.limit + 1` history items matching `query`, in key order.
    fn history_page(
        &self,
        _namespace: &str,
        _query: &crate::InspectionQuery,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        Ok(vec![])
    }
    /// Delete the history of runs finished before `before_ms`; returns the
    /// number of history items deleted.
    fn delete_history(
        &self,
        _namespace: &str,
        _owner: &str,
        _before_ms: i64,
    ) -> Result<u64, StorageError> {
        Err(StorageError::Backend("history deletion unsupported".into()))
    }
    fn protection_available(&self) -> bool {
        false
    }
    fn query(
        &self,
        namespace: &str,
        query: &crate::IndexQuery,
    ) -> Result<crate::IndexPage, StorageError> {
        let state = self.load(namespace)?.state;
        Ok(state.indexes.query(&state, query))
    }
    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError>;
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> Result<u64, StorageError>;
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError>;
    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError>;
    /// Versioned bounded transition capability (D10). The default reports
    /// `Unsupported` without running the plan; the engine then uses the full
    /// `load`/`compare_and_swap` path.
    fn transition_v1(
        &self,
        _namespace: &str,
        _owner: &str,
        _request: &mut crate::transition::TransitionRequest<'_>,
    ) -> crate::Result<crate::transition::TransitionOutcome> {
        Ok(crate::transition::TransitionOutcome::Unsupported)
    }
    /// Whether `transition_v1` is available for `namespace`.
    fn transition_v1_support(&self, _namespace: &str) -> Result<bool, StorageError> {
        Ok(false)
    }
}
#[derive(Clone, Default)]
pub struct MemoryStorage {
    inner: Arc<Mutex<MemoryNamespaces>>,
    /// Also keep every settled cohort in full (for tests).
    retain_settled: bool,
}
type MemoryNamespaces = BTreeMap<String, MemoryNamespace>;
#[derive(Default)]
struct MemoryNamespace {
    owner: Option<String>,
    incarnation: u64,
    versioned: VersionedState,
    catalog: Option<Arc<crate::catalog_inspection::CatalogProjection>>,
    catalog_projected_revision: u64,
    catalog_metadata_revision: u64,
    history: history::HistoryRows,
    settled: Vec<SettledCohort>,
}
impl MemoryStorage {
    /// Like `default()`, and also keeps each settled cohort in full, so tests
    /// can read what a commit removed from the State.
    pub fn retaining_settled() -> Self {
        Self {
            retain_settled: true,
            ..Self::default()
        }
    }
    /// The cohorts committed as settled in `namespace`, oldest first. Empty
    /// unless built with [`MemoryStorage::retaining_settled`].
    pub fn settled_cohorts(&self, namespace: &str) -> Vec<SettledCohort> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(namespace)
            .map(|entry| entry.settled.clone())
            .unwrap_or_default()
    }
    /// A run as it was when its cohort settled (see `settled_cohorts`).
    pub fn settled_run(&self, namespace: &str, id: &str) -> Option<crate::Run> {
        self.settled_cohorts(namespace)
            .into_iter()
            .flat_map(|cohort| cohort.runs)
            .find(|run| run.id == id)
    }
}
impl Storage for MemoryStorage {
    fn persists_history(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        namespace: &str,
        query: &crate::InspectionQuery,
    ) -> Result<Vec<HistoryItem>, StorageError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        match guard.get(namespace) {
            Some(entry) => history::page(&entry.history, query),
            None => query.validate().map(|()| vec![]),
        }
    }
    fn delete_history(
        &self,
        namespace: &str,
        owner: &str,
        before_ms: i64,
    ) -> Result<u64, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard
            .get_mut(namespace)
            .filter(|entry| entry.owner.as_deref() == Some(owner))
            .ok_or(StorageError::Ownership)?;
        entry
            .settled
            .retain(|cohort| cohort.finished_at_ms >= before_ms);
        Ok(history::delete_before(&mut entry.history, before_ms))
    }
    fn inspect_catalog(
        &self,
        namespace: &str,
        owner: &str,
        query: &crate::CatalogQuery,
    ) -> Result<crate::CatalogPage, crate::CatalogError> {
        use std::ops::Bound::{Excluded, Included};
        query.validate()?;
        if !crate::catalog_inspection::valid_context(namespace)
            || !crate::catalog_inspection::valid_context(owner)
        {
            return Err(crate::CatalogError::Ownership);
        }
        let guard = self
            .inner
            .lock()
            .map_err(|e| crate::CatalogError::Backend(e.to_string()))?;
        let entry = guard
            .get(namespace)
            .filter(|entry| entry.owner.as_deref() == Some(owner))
            .ok_or(crate::CatalogError::Ownership)?;
        if entry.catalog_projected_revision != entry.versioned.revision {
            return Err(crate::CatalogError::Backend(
                "catalog projection revision mismatch".into(),
            ));
        }
        let catalog = entry
            .catalog
            .as_ref()
            .ok_or(crate::CatalogError::Unsupported)?;
        let rows: Vec<_> = if let Some(key) = &query.exact {
            catalog
                .get(&(query.kind, key.clone()))
                .cloned()
                .into_iter()
                .collect()
        } else {
            catalog
                .range((
                    Excluded((query.kind, query.after.clone().unwrap_or_default())),
                    Included((query.kind, "~".into())),
                ))
                .take(query.limit + 1)
                .map(|(_, item)| item.clone())
                .collect()
        };
        crate::catalog_inspection::make_page(
            query,
            crate::CatalogStamp {
                namespace: namespace.into(),
                kind: query.kind,
                incarnation: entry.incarnation,
                metadata_revision: entry.catalog_metadata_revision,
            },
            entry.versioned.revision,
            rows,
            false,
        )
    }
    fn query(
        &self,
        namespace: &str,
        query: &crate::IndexQuery,
    ) -> Result<crate::IndexPage, StorageError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        Ok(guard
            .get(namespace)
            .map(|entry| {
                entry
                    .versioned
                    .state
                    .indexes
                    .query(&entry.versioned.state, query)
            })
            .unwrap_or_default())
    }
    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?
            .get(namespace)
            .map(|x| x.versioned.clone())
            .unwrap_or_default())
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard.entry(namespace.into()).or_default();
        if entry.owner.as_deref().is_some_and(|x| x != owner) {
            return Err(StorageError::Ownership);
        }
        if entry.owner.is_none() {
            entry.incarnation = entry.incarnation.checked_add(1).ok_or_else(|| {
                StorageError::Backend("memory owner incarnation exhausted".into())
            })?;
        }
        entry.owner = Some(owner.into());
        Ok(())
    }
    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard.entry(namespace.into()).or_default();
        if entry.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        entry.owner = None;
        Ok(())
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> Result<u64, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard.entry(namespace.into()).or_default();
        if entry.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        if entry.versioned.revision != expected_revision {
            return Err(StorageError::Conflict);
        }
        // Nothing leaves the State without its history, an expired window
        // or a legacy disposal, even for arbitrary trait callers.
        crate::retention::verify_removals(&entry.versioned.state, state)?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?;
        let items = crate::history_items(&state.settled);
        history::check_new(&entry.history, &items)?;
        history::check_not_live(state, &items)?;
        history::check_unrecorded(
            &entry.history,
            &history::new_live_keys(&entry.versioned.state, state),
        )?;
        // An old otherwise admissible State can exceed this new metadata
        // profile. Preserve existing mutations while refusing catalog reads.
        let catalog = crate::catalog_inspection::project(state).ok().map(Arc::new);
        if entry.catalog_metadata_revision == 0 || entry.catalog.as_deref() != catalog.as_deref() {
            entry.catalog_metadata_revision = revision;
        }
        entry.catalog = catalog;
        entry.catalog_projected_revision = revision;
        history::insert(
            &mut entry.history,
            items,
            state.retention.policy.history_keep,
        );
        let mut stored = state.clone();
        let settled = std::mem::take(&mut stored.settled);
        if self.retain_settled {
            entry.settled.extend(settled);
        }
        entry.versioned = VersionedState {
            revision,
            state: stored,
        };
        Ok(entry.versioned.revision)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delete_history_of_an_unknown_namespace_creates_no_entry() {
        let store = MemoryStorage::default();
        assert_eq!(
            store.delete_history("unknown", "w", 10),
            Err(StorageError::Ownership)
        );
        assert!(!store.inner.lock().unwrap().contains_key("unknown"));
    }
}
