//! What the authoring runs need from the server that hosts them: the engine
//! to install and activate on, the grant it holds for a source, internal
//! work to submit to, and its configuration.
use crate::{Config, Host, Result};
use aq_internal_work::Request;
use aq_playbooks_model::Artifact;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// The engine and the grants of the namespace playbooks run in.
pub trait Deployer: Send + Sync {
    /// Installs the artifact; installing the same artifact again is no change.
    fn install(&self, artifact: &Artifact) -> impl Future<Output = Result<()>> + Send;
    /// The live grant the server holds for `source` (ruling R2): the one it
    /// has when its capabilities are these, else a new one in its place. It
    /// never expires. Returns the grant and its revision.
    fn ensure_grant(
        &self,
        source: &str,
        capabilities: &BTreeSet<String>,
    ) -> impl Future<Output = Result<(Uuid, i64)>> + Send;
    /// Revokes the source's grant, if it has one.
    fn revoke_grant(&self, source: &str) -> impl Future<Output = Result<()>> + Send;
    /// Makes the installed artifact `digest` the one `binding` runs, under
    /// the source's grant. No change when it already is.
    fn activate(
        &self,
        source: &str,
        binding: &str,
        digest: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// Everything the runs need, on one value: the server's state.
pub trait Server: Host + Deployer {
    fn config(&self) -> Config;
    /// The catalog this server uses for this source.
    fn catalog(&self, source: &crate::SourceRecord) -> Result<aq_playbooks_model::Registry> {
        if source.id == crate::builtin::ROUTING && self.routing_host10().is_some() {
            return crate::request::catalog_2_3();
        }
        crate::request::pinned_catalog(source)
    }
    fn routing_host10(&self) -> Option<crate::continuity::Target> {
        None
    }
    /// The server clock, in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
    /// Submits the internal task `id`; submitting an id again changes nothing.
    fn submit(&self, id: Uuid, request: &Request) -> impl Future<Output = Result<()>> + Send;
}

/// The engine for authored playbooks, published when its owner opens it.
pub struct Deployment<D>(Arc<RwLock<Option<Arc<D>>>>);

impl<D> Clone for Deployment<D> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<D> Default for Deployment<D> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<D> Deployment<D> {
    pub fn set(&self, driver: Arc<D>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = Some(driver);
        }
    }

    pub fn driver(&self) -> Result<Arc<D>> {
        let driver = self.0.read().ok().and_then(|slot| slot.clone());
        driver.ok_or_else(|| {
            crate::Error::Conflict("Playbooks are not running on this server".into())
        })
    }
}
