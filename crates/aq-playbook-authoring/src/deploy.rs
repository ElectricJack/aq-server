//! What the authoring runs need from the server that hosts them: the engine
//! to install and activate on, the grant it holds for a source, internal
//! work to submit to, and its configuration.
use crate::{Config, Host, Result};
use aq_internal_work::Request;
use aq_playbooks_model::Artifact;
use std::collections::BTreeSet;
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
    /// The server clock, in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
    /// Submits the internal task `id`; submitting an id again changes nothing.
    fn submit(&self, id: Uuid, request: &Request) -> impl Future<Output = Result<()>> + Send;
}
