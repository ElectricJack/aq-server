//! Human-only bounded projections of the existing protected engine inspector.
//! No workflow input, variables, private host reasons or execution authority.
use super::{
    domain::{Error, Result},
    driver::Driver,
    service::Namespace,
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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

pub mod catalog;
pub mod definitions;
pub mod playbook_view;

pub const VERSION: &str = "playbooks-inspection-1";
const MAX_BYTES: usize = 262144;
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageQuery {
    pub after: Option<String>,
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<usize>,
    pub project_id: Option<Uuid>,
}
impl PageQuery {
    fn validate(&self, namespace: &Namespace) -> Result<usize> {
        let limit = self.limit.unwrap_or(25);
        if limit == 0
            || limit > 25
            || self
                .after
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        {
            return Err(Error::Invalid);
        }
        if self.project_id.is_some_and(|id| id != namespace.project_id) {
            return Err(Error::Forbidden);
        }
        Ok(limit)
    }
}
/// X34: the runs list with an optional admission time range. `since_ms` is
/// inclusive and `until_ms` exclusive; either may be omitted.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunsQuery {
    pub after: Option<String>,
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<usize>,
    pub project_id: Option<Uuid>,
    #[schemars(range(min = 0))]
    pub since_ms: Option<i64>,
    #[schemars(range(min = 1))]
    pub until_ms: Option<i64>,
}
/// Engine pages examined by one ranged runs request; a sparse range returns a
/// short page whose `next_cursor` resumes the scan.
const RANGE_SCAN_PAGES: usize = 8;
impl RunsQuery {
    fn validate(&self) -> Result<()> {
        if self.since_ms.is_some_and(|v| v < 0)
            || self.until_ms.is_some_and(|v| v <= 0)
            || self
                .since_ms
                .zip(self.until_ms)
                .is_some_and(|(since, until)| until <= since)
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn ranged(&self) -> bool {
        self.since_ms.is_some() || self.until_ms.is_some()
    }
    fn contains(&self, at_ms: i64) -> bool {
        self.since_ms.is_none_or(|v| at_ms >= v) && self.until_ms.is_none_or(|v| at_ms < v)
    }
    fn page(&self, after: Option<String>, limit: Option<usize>) -> PageQuery {
        PageQuery {
            after,
            limit,
            project_id: self.project_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct NamespaceView {
    pub id: Uuid,
    pub generation: String,
    pub name: String,
    pub project_id: Uuid,
}
impl From<&Namespace> for NamespaceView {
    fn from(n: &Namespace) -> Self {
        Self {
            id: n.namespace_id,
            generation: n.generation.to_string(),
            name: n.name.clone(),
            project_id: n.project_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct Page<T> {
    pub api_version: &'static str,
    pub namespace: NamespaceView,
    pub server_time_ms: i64,
    /// Each page is a fresh bounded observation; not a cross-owner snapshot.
    pub revision: Option<String>,
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunView {
    pub id: String,
    pub artifact: String,
    pub entry: String,
    pub step: String,
    pub status: String,
    pub outcome: Option<String>,
    pub reason: Option<String>,
    pub root: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub dependencies: BTreeMap<String, String>,
    pub pending_kind: Option<String>,
    pub admitted_at_ms: i64,
    /// X30/X31: null when the artifact has no AQ registry entry.
    pub definition: Option<playbook_view::DefinitionView>,
    /// X33: set while the pending effect is a human decision request.
    pub waiting: Option<playbook_view::WaitingView>,
    /// Binding the run was admitted on; null for runs not started through a
    /// retained admission (children, event starts) or admitted before it was kept.
    pub binding: Option<String>,
    /// Activation revision the run was admitted at; null when not known.
    pub activation_revision: Option<String>,
    /// Effect intents the run waits on (a pending effect or join members);
    /// private join keys for X33, never serialized.
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) pending_intents: Vec<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct EffectView {
    pub id: String,
    pub step: String,
    pub command: String,
    pub status: String,
    pub deadline_at_ms: Option<i64>,
    pub cancel_requested: bool,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ReceiptView {
    pub sequence: String,
    pub step: String,
    pub at_ms: i64,
    pub kind: String,
    pub operation: Option<String>,
    pub outcome: Option<String>,
    pub child: Option<String>,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct RunDetail {
    pub api_version: &'static str,
    pub namespace: NamespaceView,
    pub server_time_ms: i64,
    pub run: RunView,
    /// X32: ordered step graph of the run's pinned definition from its entry;
    /// null when no public graph is registered for the artifact.
    pub steps: Option<playbook_view::StepTimeline>,
    pub effects: Page<EffectView>,
    pub receipts: Page<ReceiptView>,
    pub observation_is_atomic: bool,
}
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct AdmissionView {
    pub request_id: Uuid,
    pub status: String,
    pub created_at_ms: i64,
    pub run_id: Option<String>,
    pub activation_revision: Option<String>,
    pub catalog_revision: Option<String>,
}
fn text(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or(Error::Unknown)
}
fn optional(v: &Value, key: &str) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => text(v, key).map(Some),
    }
}
fn run(v: Value) -> Result<RunView> {
    let children: Vec<String> =
        serde_json::from_value(v["children"].clone()).map_err(|_| Error::Unknown)?;
    let dependencies: BTreeMap<String, String> =
        serde_json::from_value(v["dependencies"].clone()).map_err(|_| Error::Unknown)?;
    if children.len() > 128
        || dependencies.len() > 128
        || children
            .iter()
            .chain(dependencies.keys())
            .chain(dependencies.values())
            .any(|s| s.len() > 512 || s.chars().any(char::is_control))
    {
        return Err(Error::Overloaded);
    }
    Ok(RunView {
        id: text(&v, "id")?,
        artifact: text(&v, "artifact")?,
        entry: text(&v, "entry")?,
        step: text(&v, "step")?,
        status: text(&v, "status")?,
        outcome: optional(&v, "outcome")?,
        reason: optional(&v, "reason")?,
        root: text(&v, "root")?,
        parent: optional(&v, "parent")?,
        children,
        dependencies,
        pending_kind: optional(&v["pending"], "kind")?,
        admitted_at_ms: v["admitted_at_ms"].as_i64().ok_or(Error::Unknown)?,
        definition: None,
        waiting: None,
        binding: None,
        activation_revision: None,
        pending_intents: pending_intents(&v["pending"])?,
    })
}
fn pending_intents(pending: &Value) -> Result<Vec<String>> {
    let intents: Vec<String> = match pending["kind"].as_str() {
        Some("effect") => optional(pending, "intent")?.into_iter().collect(),
        // Human decisions are awaited as owned-async join members.
        Some("join") => {
            serde_json::from_value(pending["members"].clone()).map_err(|_| Error::Unknown)?
        }
        _ => Vec::new(),
    };
    if intents.len() > 128
        || intents
            .iter()
            .any(|s| s.is_empty() || s.len() > 512 || s.chars().any(char::is_control))
    {
        return Err(Error::Overloaded);
    }
    Ok(intents)
}
fn effect(v: Value) -> Result<EffectView> {
    Ok(EffectView {
        id: text(&v, "id")?,
        step: text(&v, "step")?,
        command: text(&v, "command")?,
        status: text(&v, "status")?,
        deadline_at_ms: v["deadline_at_ms"].as_i64(),
        cancel_requested: v["cancel_requested"].as_bool().ok_or(Error::Unknown)?,
    })
}
fn receipt(v: Value) -> Result<ReceiptView> {
    Ok(ReceiptView {
        sequence: v["sequence"].as_u64().ok_or(Error::Unknown)?.to_string(),
        step: text(&v, "step")?,
        at_ms: v["at_ms"].as_i64().ok_or(Error::Unknown)?,
        kind: text(&v, "kind")?,
        operation: optional(&v["detail"], "operation")?,
        outcome: optional(&v["detail"], "outcome")?,
        child: optional(&v["detail"], "child")?,
    })
}
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
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(tokio::sync::Semaphore::new(2)),
            read_gate,
        ))
}
async fn read_gate(
    axum::extract::State(gate): axum::extract::State<Arc<tokio::sync::Semaphore>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Ok(_permit) = gate.try_acquire() else {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn page_bounds_and_project_scope_are_explicit() {
        let n = Namespace {
            namespace_id: Uuid::new_v4(),
            generation: 1,
            name: "test".into(),
            workspace_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            owner_user_id: Uuid::new_v4(),
        };
        assert!(
            PageQuery {
                limit: Some(26),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
        assert!(
            PageQuery {
                after: Some("".into()),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
        assert!(
            PageQuery {
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            }
            .validate(&n)
            .is_err()
        );
    }
    #[test]
    fn runs_time_range_is_half_open_and_validated() {
        let q = |since_ms, until_ms| RunsQuery {
            since_ms,
            until_ms,
            ..Default::default()
        };
        assert!(q(None, None).validate().is_ok() && !q(None, None).ranged());
        assert!(q(Some(-1), None).validate().is_err());
        assert!(q(None, Some(0)).validate().is_err());
        assert!(q(Some(10), Some(10)).validate().is_err());
        let range = q(Some(10), Some(20));
        assert!(range.validate().is_ok() && range.ranged());
        assert!(range.contains(10) && range.contains(19));
        assert!(!range.contains(9) && !range.contains(20));
        assert!(serde_json::from_value::<RunsQuery>(serde_json::json!({"from": 1})).is_err());
    }
    #[test]
    fn run_projection_keeps_pending_effect_intent_private() {
        let v = serde_json::json!({"id":"r1","artifact":"a","entry":"main","step":"review","status":"waiting","outcome":null,"reason":null,"root":"r1","parent":null,"children":[],"dependencies":{},"pending":{"kind":"effect","intent":"i-7"},"admitted_at_ms":5});
        let view = run(v.clone()).unwrap();
        assert_eq!(view.pending_kind.as_deref(), Some("effect"));
        assert_eq!(view.pending_intents, ["i-7"]);
        assert!(!serde_json::to_string(&view).unwrap().contains("i-7"));
        let mut join = v.clone();
        join["pending"] = serde_json::json!({"kind":"join","members":["i-8","i-9"],"mode":"all"});
        let view = run(join).unwrap();
        assert_eq!(view.pending_intents, ["i-8", "i-9"]);
        assert!(!serde_json::to_string(&view).unwrap().contains("i-8"));
        let mut event = v;
        event["pending"] = serde_json::json!({"kind":"event","intent":"x"});
        assert!(run(event).unwrap().pending_intents.is_empty());
    }
    #[test]
    fn public_receipt_projection_excludes_values_and_private_diagnostics() {
        let v = serde_json::json!({"sequence":9007199254740992_u64,"step":"send","at_ms":1,"kind":"command_result","detail":{"operation":"op","outcome":"queued","public":{"body":"private-canary"},"reason":"secret"}});
        let s = serde_json::to_string(&receipt(v).unwrap()).unwrap();
        assert!(s.contains("9007199254740992"));
        assert!(!s.contains("canary") && !s.contains("secret"));
    }
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
