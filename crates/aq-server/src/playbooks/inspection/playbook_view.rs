//! Playbooks view read contracts (G11 X30-X33), additive on
//! playbooks-inspection-1: definition name and version, pinned-versus-active
//! comparison, the ordered public step graph and the pending human decision.
//! Read-only: no activation, install or decision authority.
use super::{RunView, bounded, catalog};
use crate::{
    authz::RequireOwner,
    playbooks::{domain::Result, driver::Driver, service::Service},
};
use aq_playbook_domain::view_store::ViewStore;
use aq_playbook_host::{model::Artifact, runner};
use axum::{
    Extension, Json,
    extract::{Path, Query},
};
use std::{sync::Arc, time::Duration};

const ACTIVATION_PAGES: usize = 4;
/// Per-page budget for reading current activations. 500 ms was too tight on
/// loaded CI runners once the 2.1.0 catalog landed: the read timed out and the
/// run's definition reported `active`/`pinned_is_active` as unknown.
const ACTIVATION_PAGE_BUDGET: Duration = Duration::from_secs(2);

pub use aq_playbook_domain::playbook_view::*;

/// Reads the same public projection as run inspection, keyed by the immutable
/// artifact so it also works before a run exists and for historical run pins.
pub(super) async fn definition_graph(
    RequireOwner(actor): RequireOwner,
    Extension(driver): Extension<Arc<Driver>>,
    Path(artifact): Path<String>,
    Query(query): Query<super::definitions::DefinitionsQuery>,
) -> Result<Json<DefinitionGraph>> {
    let service = &driver.service;
    query.validate(&service.namespace)?;
    validate_artifact_id(&artifact)?;
    service.authorize_inspection(actor.id).await?;
    let mut tx = service.tx().await?;
    let registered = registered_graph(&mut *tx, &service.namespace, &artifact).await?;
    let (description_markdown, authored) = if registered.is_some() {
        aq_playbook_authoring::runs::inspection_source(&mut *tx, &artifact).await?
    } else {
        (None, None)
    };
    tx.commit().await?;
    let graph = registered.flatten();
    let details = DefinitionDetails::new(graph.as_ref(), authored.as_ref(), description_markdown);
    service.authorize_inspection(actor.id).await?;
    bounded(DefinitionGraph::new(
        &service.namespace,
        artifact,
        graph,
        details,
    ))
}

impl Service {
    pub(crate) async fn register_definition(&self, artifact: &Artifact) -> Result<()> {
        let mut tx = self.tx().await?;
        ViewStore(&self.namespace)
            .register_definition(&mut tx, artifact, self.current_time())
            .await?;
        tx.commit().await?;
        Ok(())
    }
}
impl Driver {
    /// Every current activation, or None when the catalog is unsupported,
    /// unreadable or larger than this bounded observation.
    async fn current_activations(&self) -> Option<Vec<catalog::Activation>> {
        if !self.catalog_supported() {
            return None;
        }
        let mut out = Vec::new();
        let mut after = None;
        for _ in 0..ACTIVATION_PAGES {
            let query = runner::CatalogQuery {
                kind: runner::CatalogKind::Activations,
                exact: None,
                after: after.take(),
                limit: 25,
                expected: None,
            };
            let raw = self
                .owner
                .inspect_catalog(query.clone(), ACTIVATION_PAGE_BUDGET)
                .await
                .ok()?;
            let page = catalog::public_page(
                &self.service.namespace,
                catalog::Kind::Activations,
                &query,
                raw,
                self.service.current_time(),
            )
            .ok()?;
            out.extend(page.items.into_iter().filter_map(|item| match item.record {
                catalog::Record::Activation(a) => Some(a),
                catalog::Record::Definition(_) => None,
            }));
            match page.next_cursor {
                None => return Some(out),
                Some(cursor) => after = Some(cursor),
            }
        }
        None
    }
    /// Adds X30/X31 definition and X33 waiting views to already authorized
    /// public run projections.
    pub(super) async fn enrich_runs(&self, runs: &mut [RunView]) -> Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        let activations = self.current_activations().await;
        let mut tx = self.service.tx().await?;
        ViewStore(&self.service.namespace)
            .enrich_runs(&mut tx, runs, activations.as_deref())
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub(super) async fn step_timeline(&self, run: &RunView) -> Result<Option<StepTimeline>> {
        let service = &self.service;
        let mut tx = service.tx().await?;
        let graph = registered_graph(&mut *tx, &service.namespace, &run.artifact).await?;
        tx.commit().await?;
        Ok(graph
            .flatten()
            .and_then(|graph| ordered(&graph, &run.entry)))
    }
}
