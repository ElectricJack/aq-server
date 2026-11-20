//! Immutable metadata from actual startup configuration, not a current engine
//! catalog. No engine/database snapshot is loaded to serve this projection.
use super::bounded;
use crate::{
    authz::RequireOwner,
    playbooks::{domain::*, driver::Driver},
};
use aq_playbook_host::model::Artifact;
use axum::{Extension, Json, extract::Query};
use std::sync::Arc;
use uuid::Uuid;

pub use aq_playbook_domain::loaded_definitions::*;

impl Driver {
    /// Only runtime bootstrap calls this, with its actual verified configuration.
    /// Later installation/activation never restamps this historical snapshot.
    pub(crate) fn record_loaded_definitions<'a>(
        &self,
        artifacts: impl IntoIterator<Item = &'a Artifact>,
    ) -> Result<()> {
        let snapshot = LoadedDefinitions::capture(artifacts, self.service.current_time())?;
        self.loaded_definitions
            .set(snapshot)
            .map_err(|_| Error::Conflict)
    }

    pub fn loaded_definitions_available(&self) -> bool {
        self.loaded_definitions.get().is_some()
    }

    pub async fn inspect_loaded_definitions(
        &self,
        actor: Uuid,
        query: DefinitionsQuery,
    ) -> Result<DefinitionsPage> {
        query.validate(&self.service.namespace)?;
        self.service.authorize_inspection(actor).await?;
        let snapshot = self.loaded_definitions.get().ok_or(Error::Unknown)?;
        let page = snapshot.page(&self.service.namespace, self.service.current_time());
        self.service.authorize_inspection(actor).await?;
        Ok(page)
    }
}

pub(super) async fn loaded_definitions(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<DefinitionsQuery>,
) -> Result<Json<DefinitionsPage>> {
    bounded(driver.inspect_loaded_definitions(actor.id, query).await?)
}
