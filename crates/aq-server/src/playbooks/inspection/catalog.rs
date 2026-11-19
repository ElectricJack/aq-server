//! Current component-owned catalog metadata. No startup-config projection,
//! snapshot loading, mutation authority or direct component SQL.
use super::bounded;
use crate::{
    authz::RequireOwner,
    playbooks::{domain::Error, driver::Driver},
};
use aq_playbook_host::{CatalogCallError, runner};
use axum::{Extension, Json, extract::Query};
use std::{sync::Arc, time::Duration};

pub use super::super::domain::catalog_view::*;
fn owner_error(error: CatalogCallError) -> Error {
    match error {
        CatalogCallError::Catalog(runner::CatalogError::Conflict) => Error::Conflict,
        CatalogCallError::Host(aq_playbook_host::Error::Overloaded) => Error::Overloaded,
        CatalogCallError::Host(aq_playbook_host::Error::Invalid) => Error::Invalid,
        _ => Error::Unknown,
    }
}
impl Driver {
    pub fn catalog_supported(&self) -> bool {
        self.owner.catalog_supported()
    }
    pub fn catalog_support(&self) -> aq_playbook_host::CatalogSupport {
        self.owner.catalog_support()
    }

    pub async fn inspect_current_catalog(
        &self,
        actor: &crate::store::User,
        kind: Kind,
        query: CatalogQuery,
    ) -> std::result::Result<CatalogPage, CatalogReadError> {
        let query = query.component(&self.service.namespace, kind)?;
        let authorization = self.service.authorize_current_catalog(actor).await?;
        let raw = self
            .owner
            .inspect_catalog(query.clone(), Duration::from_secs(2))
            .await;
        if self.service.authorize_current_catalog(actor).await? != authorization {
            return Err(CatalogReadError::Access);
        }
        Ok(public_page(
            &self.service.namespace,
            kind,
            &query,
            raw.map_err(owner_error)?,
            self.service.current_time(),
        )?)
    }
}
pub(super) async fn definitions(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<CatalogQuery>,
) -> std::result::Result<Json<CatalogPage>, CatalogReadError> {
    Ok(bounded(
        driver
            .inspect_current_catalog(&actor, Kind::Definitions, query)
            .await?,
    )?)
}
pub(super) async fn activations(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Query(query): Query<CatalogQuery>,
) -> std::result::Result<Json<CatalogPage>, CatalogReadError> {
    Ok(bounded(
        driver
            .inspect_current_catalog(&actor, Kind::Activations, query)
            .await?,
    )?)
}
pub fn schema_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&schemars::schema_for!(CatalogPage))
        .expect("current catalog schema serialization")
}
