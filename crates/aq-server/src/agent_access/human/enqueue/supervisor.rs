//! F1b credentials authorize only creation, with a session-private journal.
use super::super::graph_actions::{
    DependencyAdd, DependencyRemove, GraphAction, TaskEdit, TaskReprioritize, TaskRoleSet,
    TaskRouteClear, TaskRouteSet,
};
use super::*;
use crate::agent_access::supervisor_conversation::reply::{
    SupervisorPrincipal, authenticate_in_transaction,
};
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Outcome, Registry},
    contract::Scope,
    errors::ApiError,
};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use schemars::JsonSchema;

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
struct SingleGraph(GraphCreate);
impl GraphPayload for SingleGraph {
    const KIND: &'static str = "task.create";
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    const ENTITY: &'static str = "project";
    fn operation_id(&self) -> Uuid {
        self.0.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.0.project_id
    }
    fn expected(&self) -> &str {
        &self.0.expected_version
    }
    fn valid(&self) -> bool {
        self.0.valid()
    }
    fn tasks(&self) -> Vec<Uuid> {
        self.0.tasks()
    }
    fn roles(&self) -> Vec<&str> {
        self.0.roles()
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        self.0.operations(c)
    }
}
struct Single(GraphAction<SingleGraph>);
fn single(request: &ActionRequest, p: &super::super::commands::Create) -> ApiResult<SingleGraph> {
    let project = request
        .subject
        .id
        .parse()
        .map_err(|_| ApiError::invalid())?;
    Ok(SingleGraph(GraphCreate {
        operation_id: p.operation_id,
        project_id: project,
        expected_version: p.expected_project_revision.clone(),
        tasks: vec![Task {
            id: p.operation_id,
            title: p.title.clone(),
            body: p.body.clone(),
            acceptance_criteria: p.acceptance_criteria.clone(),
            labels: p.labels.clone(),
            task_state: serde_json::from_value(json!(p.task_state))
                .map_err(|_| ApiError::invalid())?,
            parent_id: p.parent_id,
            priority: p.priority,
            role: p.role.clone(),
            intelligence_hint: p.intelligence_hint.clone(),
            task_type: p.task_type.clone(),
            #[cfg(feature = "project_environments")]
            needs: p.needs.clone(),
        }],
        edges: vec![],
    }))
}
#[async_trait]
impl ActionKind for Single {
    type Payload = super::super::commands::Create;
    const KIND: &'static str = "task.create";
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    async fn authorize(
        &self,
        s: &AppState,
        u: &User,
        r: &ActionRequest,
        p: &Self::Payload,
    ) -> ApiResult<Scope> {
        self.0.authorize(s, u, r, &single(r, p)?).await
    }
    async fn authorize_lookup(
        &self,
        s: &AppState,
        u: &User,
        scope: &Scope,
        op: Option<&actions::Operation>,
    ) -> ApiResult<()> {
        self.0.authorize_lookup(s, u, scope, op).await
    }
    async fn execute(
        &self,
        s: &AppState,
        u: &User,
        r: &ActionRequest,
        p: Self::Payload,
    ) -> Outcome {
        match single(r, &p) {
            Ok(p) => self.0.execute(s, u, r, p).await,
            Err(_) => Outcome::unknown(),
        }
    }
    async fn reconcile(
        &self,
        s: &AppState,
        u: &User,
        op: &actions::Operation,
    ) -> ApiResult<Option<Outcome>> {
        self.0.reconcile(s, u, op).await
    }
}
fn registry(state: &AppState, p: SupervisorPrincipal) -> ApiResult<Registry> {
    let mut r = Registry::default();
    macro_rules! adapter {
        ($payload:ty) => {
            GraphAction::<$payload>::for_supervisor(state, p.clone())
        };
    }
    r.register(adapter!(GraphCreate))?;
    r.register(Single(adapter!(SingleGraph)))?;
    r.register(adapter!(TaskRoleSet))?;
    r.register(adapter!(TaskEdit))?;
    r.register(adapter!(TaskReprioritize))?;
    r.register(adapter!(TaskRouteSet))?;
    r.register(adapter!(TaskRouteClear))?;
    r.register(adapter!(DependencyAdd))?;
    r.register(adapter!(DependencyRemove))?;
    Ok(r)
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/actions", post(create))
        .route("/api/v1/operations/{id}", get(lookup))
        .layer(axum::extract::DefaultBodyLimit::max(65536))
}
fn bearer(headers: &HeaderMap) -> ApiResult<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::for_status(StatusCode::UNAUTHORIZED))
}
async fn principal(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    headers: &HeaderMap,
) -> ApiResult<SupervisorPrincipal> {
    let p = authenticate_in_transaction(
        &state.store.pool,
        tx,
        bearer(headers)?,
        "enqueue",
        (state.clock)()
            .timestamp_millis()
            .try_into()
            .map_err(|_| ApiError::unavailable())?,
    )
    .await
    .map_err(ApiError::from)?;
    let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(p.session_id)
        .fetch_one(&mut **tx)
        .await?;
    if collision {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(p)
}
fn journal_user(p: &SupervisorPrincipal) -> User {
    // This is only an opaque journal identity. No Owner adapter is installed.
    User {
        id: p.session_id,
        email: String::new(),
        role: crate::store::Role::Viewer,
        issuer: None,
        subject: None,
    }
}
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    bytes: std::result::Result<Bytes, axum::extract::rejection::BytesRejection>,
) -> ApiResult<(StatusCode, Json<actions::OperationResponse>)> {
    let bytes = bytes.map_err(|e| ApiError::for_status(e.status()))?;
    let mut tx = state.store.pool.begin().await?;
    // Keep lifecycle and credential locks through protected admission/dispatch.
    let p = principal(&state, &mut tx, &headers).await?;
    let user = journal_user(&p);
    let request: ActionRequest = serde_json::from_value(
        crate::agent_access::scheduling::parse::parse(&bytes).map_err(|_| ApiError::invalid())?,
    )
    .map_err(|_| ApiError::invalid())?;
    let op = actions::submit_scoped(&state, &registry(&state, p)?, &user, request, None).await?;
    let response = actions::response(&state, op);
    tx.commit().await?;
    Ok((actions::operation_status(&response.data), Json(response)))
}
async fn lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> ApiResult<Json<actions::OperationResponse>> {
    let mut tx = state.store.pool.begin().await?;
    let p = principal(&state, &mut tx, &headers).await?;
    let user = journal_user(&p);
    let Path(id) = path.map_err(|_| ApiError::invalid())?;
    let op = actions::lookup_scoped(&state, &registry(&state, p.clone())?, &user, id).await;
    #[cfg(feature = "supervisor_full_access")]
    let op = match (
        op,
        aq_supervisor_control::owner::approving_owner(&mut tx, &p).await,
    ) {
        // Commands sent through the owner bridge are in the approving owner's journal.
        (Err(e), Some(owner)) if e.status() == StatusCode::NOT_FOUND => {
            let found = actions::journal::lookup_for_authorization(&state.store.pool, owner, id);
            found.await?.and_then(|lookup| lookup.operation).ok_or(e)
        }
        (op, _) => op,
    };
    let response = actions::response(&state, op?);
    tx.commit().await?;
    Ok(Json(response))
}
/// Agent actions use an attempt-private journal and a fixed adapter set.
fn agent_registry(
    state: &AppState,
    ids: aq_agent_grants::AttemptIds,
    guards: std::collections::BTreeMap<Uuid, u64>,
    workspace: Option<Uuid>,
) -> ApiResult<Registry> {
    let mut registry = Registry::default();
    let (attempt, task, project) = (ids.attempt_id, ids.task_id, ids.project_id);
    macro_rules! adapter {
        ($payload:ty) => {
            GraphAction::<$payload>::for_agent(
                state,
                attempt,
                task,
                project,
                guards.clone(),
                workspace,
            )
        };
    }
    registry.register(Single(adapter!(SingleGraph)))?;
    registry.register(adapter!(GraphCreate))?;
    registry.register(adapter!(TaskEdit))?;
    registry.register(adapter!(DependencyAdd))?;
    registry.register(adapter!(DependencyRemove))?;
    registry.register(adapter!(aq_human_work::Execution))?;
    Ok(registry)
}

async fn agent_user(state: &AppState, ids: aq_agent_grants::AttemptIds) -> ApiResult<User> {
    let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(ids.attempt_id)
        .fetch_one(&state.store.pool)
        .await?;
    if collision {
        return Err(ApiError::for_status(StatusCode::FORBIDDEN));
    }
    Ok(User {
        id: ids.attempt_id,
        email: String::new(),
        role: crate::store::Role::Viewer,
        issuer: None,
        subject: None,
    })
}

fn agent_response(state: &AppState, op: actions::Operation) -> axum::response::Response {
    use axum::response::IntoResponse;
    let response = actions::response(state, op);
    (
        actions::operation_status(&response.data),
        [
            ("X-AQ-Agent-API-Version", "1"),
            ("Cache-Control", "no-store"),
        ],
        Json(response),
    )
        .into_response()
}

pub(crate) async fn agent_lookup(
    state: &AppState,
    ids: aq_agent_grants::AttemptIds,
    operation: Uuid,
) -> ApiResult<axum::response::Response> {
    let registry = agent_registry(state, ids, Default::default(), None)?;
    let op =
        actions::lookup_scoped(state, &registry, &agent_user(state, ids).await?, operation).await?;
    Ok(agent_response(state, op))
}

pub(crate) async fn agent_replay(
    state: &AppState,
    ids: aq_agent_grants::AttemptIds,
    request: &ActionRequest,
) -> ApiResult<Option<axum::response::Response>> {
    let registry = agent_registry(state, ids, Default::default(), None)?;
    let scope = Scope::Project {
        project_id: ids.project_id,
    };
    actions::replay_scoped(
        state,
        &registry,
        &agent_user(state, ids).await?,
        request,
        &scope,
    )
    .await
    .map(|op| op.map(|op| agent_response(state, op)))
}

pub(crate) async fn agent_action(
    state: &AppState,
    ids: aq_agent_grants::AttemptIds,
    request: ActionRequest,
    guards: std::collections::BTreeMap<Uuid, u64>,
    workspace: Option<Uuid>,
    admission: actions::AgentAdmission<'_>,
) -> ApiResult<axum::response::Response> {
    let registry = agent_registry(state, ids, guards, workspace)?;
    let op = actions::submit_scoped(
        state,
        &registry,
        &agent_user(state, ids).await?,
        request,
        Some(admission),
    )
    .await?;
    Ok(agent_response(state, op))
}
