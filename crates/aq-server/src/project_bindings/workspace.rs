//! Task-scoped sibling source requests live in the execution layer. The target
//! binding supplies the repo, source policy and permissions; clients never name a
//! transport, local path, credential, or arbitrary ref.
use super::{Result, bad, conflict};
use crate::{
    AppState,
    authz::RequireOwner,
    error::{AppError, db},
};
use aq_project_policy::{
    ProjectRecord,
    workspace::{Access, Disposition, SourcePin},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(crate) mod creation;
mod source;
mod store;

pub(crate) use aq_project_policy::workspace::{Requirement, Service};
#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum RequestBody {
    One(Requirement),
    Batch { workspace: Vec<Requirement> },
}
impl RequestBody {
    fn requirements(self) -> Result<Vec<Requirement>> {
        let list = match self {
            Self::One(req) => vec![req],
            Self::Batch { workspace } => workspace,
        };
        if list.is_empty() || !aq_project_policy::workspace::valid_list(&list) {
            return Err(bad(
                "workspace must name distinct projects and each service only once",
            ));
        }
        Ok(list)
    }
}
#[derive(serde::Serialize, Deserialize)]
pub(crate) enum Selected {
    Source(SourcePin, Disposition),
    Postgres,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    decision: Resolution,
    project_revision: i64,
    commit_oid: String,
    access: Access,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Resolution {
    Approved,
    Denied,
}
pub(crate) fn owner_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{home}/tasks/{task}/workspace",
            get(show).post(request),
        )
        .route("/api/projects/{target}/workspace-requests", get(inbox))
        .route(
            "/api/projects/{target}/workspace-requests/{home}/{task}/decision",
            post(decide),
        )
}
async fn home_task(state: &AppState, home: Uuid, task: Uuid) -> Result<ProjectRecord> {
    let project = super::current(state, home).await?;
    let graph = state
        .agent_access
        .as_deref()
        .ok_or_else(|| AppError::Maintenance("Graph unavailable".into()))?;
    let node = crate::agent_access::human::graph_actions::current(
        graph,
        project.binding.graph_workspace_id,
        task,
        "task",
    )
    .await?;
    if node["metadata"]["aq:project_id"] != json!(home) {
        return Err(AppError::Forbidden(
            "task belongs to another project".into(),
        ));
    }
    Ok(project)
}
async fn show(
    State(s): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path((home, task)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    home_task(&s, home, task).await?;
    store::view(&s, home, task).await.map(Json)
}
async fn inbox(
    State(s): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(target): Path<Uuid>,
) -> Result<Json<Value>> {
    super::current(&s, target).await?;
    store::inbox(&s, target).await.map(Json)
}
async fn request(
    State(s): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path((home, task)): Path<(Uuid, Uuid)>,
    Json(requirement): Json<RequestBody>,
) -> Result<Json<Value>> {
    let project = home_task(&s, home, task).await?;
    apply(&s, &project, task, requirement, Caller::Owner(actor.id))
        .await
        .map(Json)
}
pub(crate) enum Caller<'a> {
    Owner(Uuid),
    Attempt(
        &'a crate::agent_access::grant_check::AgentCaller,
        &'a HeaderMap,
    ),
}
impl Caller<'_> {
    fn attempt(&self) -> Option<Uuid> {
        match self {
            Self::Attempt(a, _) => Some(a.ids.attempt_id),
            Self::Owner(_) => None,
        }
    }
    async fn fence(&self, s: &AppState, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        match self {
            Self::Owner(actor) => store::owner(tx, *actor).await,
            Self::Attempt(a, headers) => a
                .recheck_locked(s, headers, tx)
                .await
                .map(|_| ())
                .map_err(|_| AppError::Forbidden("workspace request credential changed".into())),
        }
    }
}

pub(crate) async fn apply(
    s: &AppState,
    home: &ProjectRecord,
    task: Uuid,
    req: RequestBody,
    caller: Caller<'_>,
) -> Result<Value> {
    let home_id = home.binding.project_id;
    if !home.binding.enabled || task.is_nil() {
        return Err(conflict("workspace home is unavailable"));
    }
    // Repeated requests reuse the retained pin even after the configured branch moves.
    // Preflight outside any DB transaction. The locked write checks both revisions again.
    let selected = select_all(s, home, task, &req.requirements()?).await?;
    let mut tx = s.store.pool.begin().await.map_err(db)?;
    caller.fence(s, &mut tx).await?;
    store::lock_task(&mut tx, home_id, task).await?;
    store::fence(&mut tx, home_id, home.revision).await?;
    for requirement in &selected {
        requirement.fence(&mut tx).await?;
    }
    let predecessor=match caller.attempt(){
        Some(attempt)=>Some(attempt),
        None=>sqlx::query_scalar("SELECT a.attempt_id FROM execution_attempts a JOIN profiled_launches l USING(attempt_id)
          WHERE a.holds_task AND a.task_id=$1 AND l.launch_manifest#>>'{bindings,project_id}'=$2
            AND NOT EXISTS(SELECT 1 FROM profiled_result_heads h WHERE h.attempt_id=a.attempt_id)")
          .bind(task).bind(home_id.to_string()).fetch_optional(&mut *tx).await.map_err(db)?,
    };
    for requirement in selected {
        requirement
            .save(&mut tx, home_id, task, predecessor)
            .await?;
    }
    tx.commit().await.map_err(db)?;
    store::view(s, home_id, task).await
}
async fn decide(
    State(s): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path((target, home, task)): Path<(Uuid, Uuid, Uuid)>,
    Json(action): Json<Decision>,
) -> Result<Json<Value>> {
    let mut tx = s.store.pool.begin().await.map_err(db)?;
    store::owner(&mut tx, actor.id).await?;
    store::lock_task(&mut tx, home, task).await?;
    let revision =
        u64::try_from(action.project_revision).map_err(|_| bad("invalid project revision"))?;
    store::fence(&mut tx, target, revision).await?;
    store::decision(&mut tx, actor.id, (target, home, task), &action).await?;
    tx.commit().await.map_err(db)?;
    // A target owner's decision response only includes its own row. Other sibling
    // repositories and services belong to the home project.
    store::target_view(&s, home, task, target).await.map(Json)
}

async fn select_all(
    s: &AppState,
    home: &ProjectRecord,
    task: Uuid,
    requested: &[Requirement],
) -> Result<Vec<Selected>> {
    if !cfg!(feature = "project_environments")
        && requested
            .iter()
            .any(|r| matches!(r, Requirement::Service { .. }))
    {
        return Err(AppError::Maintenance(
            "Task workspace services require an AQ server built with project_environments".into(),
        ));
    }
    let mut requirements = Vec::with_capacity(requested.len());
    let mut repositories = std::collections::BTreeSet::new();
    for requirement in requested {
        let selected = select(s, home, task, *requirement).await?;
        if let Selected::Source(pin, _) = &selected
            && !repositories.insert(pin.repository_id)
        {
            return Err(bad(
                "workspace projects must refer to distinct repositories",
            ));
        }
        requirements.push(selected);
    }
    Ok(requirements)
}
async fn select(
    s: &AppState,
    home: &ProjectRecord,
    task: Uuid,
    req: Requirement,
) -> Result<Selected> {
    let home_id = home.binding.project_id;
    match req {
        Requirement::Source {
            target_project_id,
            access,
        } => {
            let target = super::current(s, target_project_id).await?;
            let decision = aq_project_policy::workspace::disposition(home_id, &target, access)
                .map_err(|e| AppError::Forbidden(e.into()))?;
            if home.binding.repository_id == target.binding.repository_id {
                return Err(bad("sibling project aliases the home repository"));
            }
            let pin = source::select(s, home_id, task, &target, access).await?;
            Ok(Selected::Source(pin, decision))
        }
        Requirement::Service {
            service: Service::Postgres,
        } => Ok(Selected::Postgres),
    }
}
impl Selected {
    async fn fence(&self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        if let Self::Source(p, _) = self {
            store::fence(tx, p.project_id, p.project_revision).await?;
        }
        Ok(())
    }
    async fn save(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        home_id: Uuid,
        task: Uuid,
        attempt: Option<Uuid>,
    ) -> Result<()> {
        if let Self::Source(pin, decision) = self {
            store::request(tx, home_id, task, pin, *decision, attempt).await?;
        } else {
            sqlx::query("INSERT INTO task_workspace_services(home_project_id,task_id,service,requested_attempt_id) VALUES($1,$2,'postgres',$3) ON CONFLICT DO NOTHING")
            .bind(home_id).bind(task).bind(attempt).execute(&mut **tx).await.map_err(db)?;
        }
        Ok(())
    }
}
