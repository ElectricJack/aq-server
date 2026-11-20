//! Human-only bounded projections of the existing protected engine inspector.
//! No workflow input, variables, private host reasons or execution authority.
use super::{
    domain::{Error, Result},
    driver::Driver,
};
use crate::{AppState, authz::RequireOwner};
use aq_playbook_host::{
    Request,
    runner::{InspectionKind, InspectionPage, InspectionQuery},
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query},
    routing::get,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

pub mod catalog;
pub mod definitions;
pub mod playbook_view;

pub const VERSION: &str = "playbooks-inspection-1";
const MAX_BYTES: usize = catalog::MAX_BYTES;
/// Engine pages examined by one ranged runs request; a sparse range returns a
/// short page whose `next_cursor` resumes the scan.
const RANGE_SCAN_PAGES: usize = 8;
pub use aq_playbook_domain::inspection::{
    AdmissionView, EffectView, NamespaceView, Page, PageQuery, ReceiptView, RunDetail, RunView,
    RunsQuery,
};
use aq_playbook_domain::inspection::{effect, receipt, run};
fn bounded<T: Serialize>(v: T) -> Result<Json<T>> {
    if serde_json::to_vec(&v).map_err(|_| Error::Unknown)?.len() > MAX_BYTES {
        return Err(Error::Overloaded);
    }
    Ok(Json(v))
}
impl Driver {
    async fn public_page<T>(
        &self,
        actor: Uuid,
        kind: InspectionKind,
        id: Option<String>,
        query: PageQuery,
        project: fn(Value) -> Result<T>,
    ) -> Result<Page<T>> {
        let limit = query.validate(&self.service.namespace)?;
        self.service.authorize_inspection(actor).await?;
        let raw = self
            .owner
            .call(
                Request::Inspect {
                    query: InspectionQuery {
                        kind,
                        run: id,
                        after: query.after,
                        limit,
                    },
                },
                Duration::from_secs(2),
            )
            .await
            .map_err(|_| Error::Unknown)?;
        let raw: InspectionPage = serde_json::from_value(raw).map_err(|_| Error::Unknown)?;
        if raw.items.len() > limit {
            return Err(Error::Unknown);
        }
        let items = raw
            .items
            .into_iter()
            .map(project)
            .collect::<Result<Vec<_>>>()?;
        self.service.authorize_inspection(actor).await?;
        Ok(Page {
            api_version: VERSION,
            namespace: NamespaceView::from(&self.service.namespace),
            server_time_ms: self.service.current_time(),
            revision: Some(raw.revision.to_string()),
            items,
            next_cursor: raw.next,
        })
    }
    pub async fn inspect_runs(&self, actor: Uuid, query: RunsQuery) -> Result<Page<RunView>> {
        query.validate()?;
        let mut page = if query.ranged() {
            self.ranged_runs(actor, &query).await?
        } else {
            self.public_page(
                actor,
                InspectionKind::Runs,
                None,
                query.page(query.after.clone(), query.limit),
                run,
            )
            .await?
        };
        self.enrich_runs(&mut page.items).await?;
        self.service.authorize_inspection(actor).await?;
        Ok(page)
    }
    /// Runs are ordered by id, not admission time, so a range is a bounded
    /// filtered scan over engine pages.
    async fn ranged_runs(&self, actor: Uuid, query: &RunsQuery) -> Result<Page<RunView>> {
        let limit = query
            .page(query.after.clone(), query.limit)
            .validate(&self.service.namespace)?;
        let mut after = query.after.clone();
        let mut items: Vec<RunView> = Vec::new();
        let mut header = None;
        for _ in 0..RANGE_SCAN_PAGES {
            let page = self
                .public_page(
                    actor,
                    InspectionKind::Runs,
                    None,
                    query.page(after, None),
                    run,
                )
                .await?;
            after = page.next_cursor.clone();
            let mut matched = page
                .items
                .iter()
                .filter(|view| query.contains(view.admitted_at_ms))
                .cloned();
            let header = header.insert(Page {
                items: Vec::new(),
                next_cursor: None,
                ..page
            });
            for view in matched.by_ref() {
                if items.len() == limit {
                    // A further match exists: resume after the last returned run.
                    return Ok(Page {
                        next_cursor: items.last().map(|v| v.id.clone()),
                        items,
                        ..header.clone()
                    });
                }
                items.push(view);
            }
            if after.is_none() {
                break;
            }
        }
        // Either the scan ended or its budget is spent and `after` resumes it.
        Ok(Page {
            items,
            next_cursor: after,
            ..header.ok_or(Error::Unknown)?
        })
    }
    pub async fn inspect_run_timeline(
        &self,
        actor: Uuid,
        id: String,
        query: PageQuery,
    ) -> Result<RunDetail> {
        query.validate(&self.service.namespace)?;
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return Err(Error::Invalid);
        }
        let runs = self
            .public_page(
                actor,
                InspectionKind::Runs,
                Some(id.clone()),
                PageQuery {
                    after: None,
                    limit: Some(1),
                    project_id: query.project_id,
                },
                run,
            )
            .await?;
        let mut found = runs.items;
        self.enrich_runs(&mut found).await?;
        let run = found.into_iter().next().ok_or(Error::Forbidden)?;
        let steps = self.step_timeline(&run).await?;
        let effects = self
            .public_page(
                actor,
                InspectionKind::Effects,
                Some(id.clone()),
                PageQuery {
                    after: None,
                    limit: Some(25),
                    project_id: query.project_id,
                },
                effect,
            )
            .await?;
        let receipts = self
            .public_page(actor, InspectionKind::Receipts, Some(id), query, receipt)
            .await?;
        self.service.authorize_inspection(actor).await?;
        Ok(RunDetail {
            api_version: VERSION,
            namespace: runs.namespace,
            server_time_ms: self.service.current_time(),
            run,
            steps,
            effects,
            receipts,
            observation_is_atomic: false,
        })
    }
}
pub fn routes(driver: Arc<Driver>) -> Router<AppState> {
    Router::new()
        .route("/api/playbooks/inspection/runs", get(runs))
        .route("/api/playbooks/inspection/runs/{id}", get(detail))
        .route("/api/playbooks/inspection/admissions", get(admissions))
        .route(
            "/api/playbooks/inspection/definitions/{artifact}/graph",
            get(playbook_view::definition_graph),
        )
        .route(
            "/api/playbooks/inspection/catalog/definitions",
            get(catalog::definitions),
        )
        .route(
            "/api/playbooks/inspection/catalog/activations",
            get(catalog::activations),
        )
        .route(
            "/api/playbooks/inspection/loaded-definitions",
            get(definitions::loaded_definitions),
        )
        .layer(Extension(driver))
        // 2 run (the bridge pool has 2 connections), 8 more wait briefly.
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(crate::read_gate::ReadGate::new(2, 8)),
            read_gate,
        ))
}
async fn read_gate(
    axum::extract::State(gate): axum::extract::State<Arc<crate::read_gate::ReadGate>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(_permits) = gate.enter().await else {
        let mut response = Error::Overloaded.into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        return response;
    };
    let mut response = tokio::time::timeout(Duration::from_secs(3), next.run(request))
        .await
        .unwrap_or_else(|_| Error::Unknown.into_response());
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn runs(
    RequireOwner(actor): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<Page<RunView>>> {
    bounded(d.inspect_runs(actor.id, q).await?)
}
async fn detail(
    RequireOwner(actor): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<String>,
    Query(q): Query<PageQuery>,
) -> Result<Json<RunDetail>> {
    bounded(d.inspect_run_timeline(actor.id, id, q).await?)
}
async fn admissions(
    RequireOwner(actor): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Query(q): Query<PageQuery>,
) -> Result<Json<Page<AdmissionView>>> {
    q.validate(&d.service.namespace)?;
    bounded(
        d.service
            .inspect_admissions(actor.id, q.after, q.limit.unwrap_or(25))
            .await?,
    )
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct Contract {
    query: PageQuery,
    runs_query: RunsQuery,
    runs: Page<RunView>,
    detail: RunDetail,
    admissions: Page<AdmissionView>,
}
/// Public inspection DTO schema; independent of the frozen engine artifact.
pub fn schema_bytes() -> Vec<u8> {
    serde_json::to_vec_pretty(&schemars::schema_for!(Contract))
        .expect("inspection schema serialization")
}
