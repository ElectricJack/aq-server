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
type MemoryNamespaces = BTreeMap<String, (Option<String>, VersionedState)>;
impl Storage for MemoryStorage {
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
            .map(|entry| entry.1.state.indexes.query(&entry.1.state, query))
            .unwrap_or_default())
    }
    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError> {
        Ok(self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?
            .get(namespace)
            .map(|x| x.1.clone())
            .unwrap_or_default())
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard.entry(namespace.into()).or_default();
        if entry.0.as_deref().is_some_and(|x| x != owner) {
            return Err(StorageError::Ownership);
        }
        entry.0 = Some(owner.into());
        Ok(())
    }
    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let entry = guard.entry(namespace.into()).or_default();
        if entry.0.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        entry.0 = None;
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
        if entry.0.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        if entry.1.revision != expected_revision {
            return Err(StorageError::Conflict);
        }
        // Artifact addresses are append-only even for arbitrary trait callers.
        crate::retention::verify_artifact_removals(&entry.1.state, state)?;
        entry.1 = VersionedState {
            revision: expected_revision
                .checked_add(1)
                .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?,
            state: state.clone(),
        };
        Ok(entry.1.revision)
    }
}
