//! Atomic namespace storage. A successful CAS commits *all* snapshots, receipts,
//! waits, child admissions and effect intents, or none of them. Backends must
//! serialize ingress with expiry through this same revision. External dispatch
//! is deliberately excluded from transactions.
use crate::State;
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
    fn inspect(
        &self,
        namespace: &str,
        query: &crate::InspectionQuery,
    ) -> Result<crate::InspectionPage, StorageError> {
        let versioned = self.load(namespace)?;
        crate::inspection::page(&versioned.state, versioned.revision, query)
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
}
#[derive(Clone, Default)]
pub struct MemoryStorage {
    inner: Arc<Mutex<MemoryNamespaces>>,
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
}
impl Storage for MemoryStorage {
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
        // Artifact addresses are append-only even for arbitrary trait callers.
        crate::retention::verify_artifact_removals(&entry.versioned.state, state)?;
        crate::admission::verify_receipt_retention(&entry.versioned.state, state)?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?;
        // An old otherwise admissible State can exceed this new metadata
        // profile. Preserve existing mutations while refusing catalog reads.
        let catalog = crate::catalog_inspection::project(state).ok().map(Arc::new);
        if entry.catalog_metadata_revision == 0 || entry.catalog.as_deref() != catalog.as_deref() {
            entry.catalog_metadata_revision = revision;
        }
        entry.catalog = catalog;
        entry.catalog_projected_revision = revision;
        entry.versioned = VersionedState {
            revision,
            state: state.clone(),
        };
        Ok(entry.versioned.revision)
    }
}
