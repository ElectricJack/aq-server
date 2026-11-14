//! D9 owner artifact API; moved to the aq-playbook-domain crate. The
//! production catalog over the owner driver stays here.
use super::domain::Result;
pub use aq_playbook_domain::artifacts::*;
use aq_playbook_host::model::{Artifact, Registry};
use uuid::Uuid;

/// Production catalog: the owner driver, activating under an active grant.
pub struct DriverCatalog<'a> {
    pub driver: &'a super::driver::Driver,
    pub grant: Uuid,
    pub revision: i64,
}
impl Catalog for DriverCatalog<'_> {
    fn registry(&self) -> Registry {
        self.driver.service.profile.registry()
    }
    async fn install(&self, artifact: Artifact) -> Result<()> {
        self.driver.install(artifact).await
    }
    async fn activate(&self, binding: &str, digest: &str) -> Result<()> {
        self.driver
            .activate(binding.into(), digest.into(), self.grant, self.revision)
            .await
            .map(|_| ())
    }
    async fn current(&self, binding: &str) -> Result<Option<String>> {
        self.driver.binding_artifact(binding.into()).await
    }
}
